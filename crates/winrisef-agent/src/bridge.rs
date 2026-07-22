use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use web_transport_quinn::{RecvStream, SendStream, Session};

use crate::{
    auth::{TicketAuthority, parse_hex},
    certificate,
    file_transfer::{FileTransferManager, NativeDataPlane},
    network_endpoints::{self, PublishedNetworkEndpoints},
};

const BRIDGE_VERSION: u16 = 3;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum BridgeInput {
    Hello {
        version: u16,
        #[serde(rename = "launchToken")]
        launch_token: String,
    },
    IssueBenchmarkTicket {
        #[serde(rename = "requestId")]
        request_id: u32,
    },
    GetNetworkEndpoints {
        #[serde(rename = "requestId")]
        request_id: u32,
    },
    SelectFiles {
        #[serde(rename = "requestId")]
        request_id: u32,
    },
    CreateSendTransfer {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "sourceId")]
        source_id: String,
        #[serde(rename = "attachmentId")]
        attachment_id: String,
        #[serde(rename = "ownerDeviceId")]
        owner_device_id: String,
        #[serde(rename = "peerDeviceId")]
        peer_device_id: String,
        #[serde(rename = "dataPlane")]
        data_plane: NativeDataPlane,
    },
    PrepareReceiveTransfer {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "attachmentId")]
        attachment_id: String,
        #[serde(rename = "ownerDeviceId")]
        owner_device_id: String,
        #[serde(rename = "peerDeviceId")]
        peer_device_id: String,
        name: String,
        #[serde(rename = "totalBytes")]
        total_bytes: u64,
        #[serde(rename = "dataPlane")]
        data_plane: NativeDataPlane,
    },
    CancelTransfer {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "transferId")]
        transfer_id: String,
    },
    FinishSendTransfer {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "transferId")]
        transfer_id: String,
    },
    ReleaseSource {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "sourceId")]
        source_id: String,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum BridgeOutput<'a> {
    HelloAck {
        version: u16,
        accepted: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
    },
    Response {
        #[serde(rename = "requestId")]
        request_id: u32,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    NetworkEndpointsChanged {
        snapshot: PublishedNetworkEndpoints,
    },
}

pub async fn run_session(
    session: Session,
    authority: TicketAuthority,
    files: FileTransferManager,
    port: u16,
) -> anyhow::Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(AUTH_TIMEOUT, session.accept_bi())
        .await
        .context("timed out waiting for the Bridge control stream")?
        .context("failed to accept the Bridge control stream")?;
    let hello = tokio::time::timeout(AUTH_TIMEOUT, read_frame(&mut recv))
        .await
        .context("timed out waiting for Bridge authentication")??
        .context("Bridge stream ended before hello")?;
    let BridgeInput::Hello {
        version,
        launch_token,
    } = hello
    else {
        write_frame(
            &mut send,
            &BridgeOutput::HelloAck {
                version: BRIDGE_VERSION,
                accepted: false,
                error: Some("Bridge hello is required"),
            },
        )
        .await?;
        anyhow::bail!("Bridge first frame was not hello");
    };
    let token = parse_hex::<16>(&launch_token, "Bridge launch token")?;
    let accepted = version == BRIDGE_VERSION && authority.consume_launch_token(&token)?;
    write_frame(
        &mut send,
        &BridgeOutput::HelloAck {
            version: BRIDGE_VERSION,
            accepted,
            error: (!accepted).then_some("Bridge version or launch token was rejected"),
        },
    )
    .await?;
    anyhow::ensure!(accepted, "Bridge authentication was rejected");
    tracing::info!(bridge_version = BRIDGE_VERSION, "Bridge V3 authenticated");

    let mut events = files.subscribe();
    let mut endpoint_changes = network_endpoints::watch_changes();
    let mut endpoint_snapshot = network_endpoints::published(port);
    loop {
        tokio::select! {
            input = read_frame(&mut recv) => {
                let Some(input) = input? else { break };
                let (request_id, result) = handle_input(input, &authority, &files, port).await?;
                let output = match result {
                    Ok(result) => BridgeOutput::Response { request_id, ok: true, result: Some(result), error: None },
                    Err(error) => BridgeOutput::Response { request_id, ok: false, result: None, error: Some(error.to_string()) },
                };
                write_frame(&mut send, &output).await?;
            }
            event = events.recv() => {
                match event {
                    Ok(event) => write_frame(&mut send, &event).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "Bridge file event receiver lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = endpoint_changes.changed() => {
                let next = network_endpoints::published(port);
                if next.network_epoch != endpoint_snapshot.network_epoch {
                    endpoint_snapshot = next.clone();
                    write_frame(&mut send, &BridgeOutput::NetworkEndpointsChanged { snapshot: next }).await?;
                }
            }
        }
    }
    files.cancel_all();
    send.finish().context("failed to finish the Bridge stream")?;
    session.close(0, b"bridge closed");
    tracing::info!("Bridge V3 session closed");
    Ok(())
}

async fn handle_input(
    input: BridgeInput,
    authority: &TicketAuthority,
    files: &FileTransferManager,
    port: u16,
) -> anyhow::Result<(u32, anyhow::Result<Value>)> {
    let response = match input {
        BridgeInput::Hello { .. } => anyhow::bail!("Bridge hello cannot be repeated"),
        BridgeInput::IssueBenchmarkTicket { request_id } => {
            let snapshot = network_endpoints::published(port);
            let result = authority.issue_ticket().and_then(|ticket| {
                serde_json::to_value(serde_json::json!({
                    "token": certificate::format_hex(&ticket.token),
                    "expiresAt": ticket.expires_at_ms,
                    "endpoints": snapshot.benchmark_endpoints,
                    "networkEpoch": snapshot.network_epoch,
                }))
                .context("failed to encode benchmark ticket")
            });
            (request_id, result)
        }
        BridgeInput::GetNetworkEndpoints { request_id } => (
            request_id,
            serde_json::to_value(network_endpoints::published(port))
                .context("failed to encode network endpoint snapshot"),
        ),
        BridgeInput::SelectFiles { request_id } => {
            let result = files.select_files().await.and_then(|selected| {
                serde_json::to_value(selected).context("failed to encode selected files")
            });
            (request_id, result)
        }
        BridgeInput::CreateSendTransfer {
            request_id,
            source_id,
            attachment_id,
            owner_device_id,
            peer_device_id,
            data_plane,
        } => {
            let result = files
                .create_send_transfer(
                    &source_id,
                    &attachment_id,
                    &owner_device_id,
                    &peer_device_id,
                    data_plane,
                )
                .and_then(|grant| encode_transfer_grant(grant, port));
            (request_id, result)
        }
        BridgeInput::PrepareReceiveTransfer {
            request_id,
            attachment_id,
            owner_device_id,
            peer_device_id,
            name,
            total_bytes,
            data_plane,
        } => {
            let result = files
                .prepare_receive_transfer(
                    &attachment_id,
                    &owner_device_id,
                    &peer_device_id,
                    &name,
                    total_bytes,
                    data_plane,
                )
                .await
                .and_then(|grant| encode_transfer_grant(grant, port));
            (request_id, result)
        }
        BridgeInput::CancelTransfer {
            request_id,
            transfer_id,
        } => (
            request_id,
            files
                .cancel_transfer(&transfer_id)
                .map(|()| serde_json::json!({ "cancelled": true })),
        ),
        BridgeInput::FinishSendTransfer {
            request_id,
            transfer_id,
        } => (
            request_id,
            files
                .finish_send_transfer(&transfer_id)
                .map(|()| serde_json::json!({ "finished": true })),
        ),
        BridgeInput::ReleaseSource {
            request_id,
            source_id,
        } => (
            request_id,
            files
                .release_source(&source_id)
                .map(|()| serde_json::json!({ "released": true })),
        ),
    };
    Ok(response)
}

fn encode_transfer_grant(grant: impl Serialize, port: u16) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(grant).context("failed to encode transfer grant")?;
    let object = value
        .as_object_mut()
        .context("encoded transfer grant was not an object")?;
    object.insert(
        "endpointSnapshot".to_owned(),
        serde_json::to_value(network_endpoints::published(port))
            .context("failed to encode transfer endpoint snapshot")?,
    );
    Ok(value)
}

async fn read_frame(recv: &mut RecvStream) -> anyhow::Result<Option<BridgeInput>> {
    let mut prefix = [0_u8; 4];
    let Some(first) = recv
        .read(&mut prefix[..1])
        .await
        .context("failed to read a Bridge frame")?
    else {
        return Ok(None);
    };
    anyhow::ensure!(first == 1, "invalid Bridge frame prefix");
    recv.read_exact(&mut prefix[1..])
        .await
        .context("failed to read a Bridge frame length")?;
    let len = usize::try_from(u32::from_be_bytes(prefix)).expect("u32 fits usize on supported targets");
    anyhow::ensure!(
        len > 0 && len <= MAX_FRAME_BYTES,
        "Bridge frame length is invalid"
    );
    let mut bytes = vec![0_u8; len];
    recv.read_exact(&mut bytes)
        .await
        .context("failed to read a complete Bridge frame")?;
    serde_json::from_slice(&bytes)
        .context("Bridge frame contains invalid JSON")
        .map(Some)
}

async fn write_frame<T: Serialize>(send: &mut SendStream, value: &T) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value).context("failed to encode a Bridge frame")?;
    anyhow::ensure!(bytes.len() <= MAX_FRAME_BYTES, "Bridge response is too large");
    let len = u32::try_from(bytes.len()).context("Bridge response length does not fit u32")?;
    send.write_all(&len.to_be_bytes())
        .await
        .context("failed to write a Bridge frame length")?;
    send.write_all(&bytes)
        .await
        .context("failed to write a Bridge frame body")
}
