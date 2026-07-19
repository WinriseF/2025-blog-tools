use anyhow::Context;
use web_transport_quinn::{RecvStream, SendStream, Session};

use crate::auth::TicketAuthority;

const BRIDGE_VERSION: u16 = 1;
const HELLO_LEN: usize = 32;
const ACK_LEN: usize = 16;
const TICKET_REQUEST_LEN: usize = 16;
const TICKET_RESPONSE_LEN: usize = 40;
const HELLO_MAGIC: [u8; 8] = *b"WRNFBH01";
const ACK_MAGIC: [u8; 8] = *b"WRNFBA01";
const TICKET_REQUEST_MAGIC: [u8; 8] = *b"WRNFTR01";
const TICKET_RESPONSE_MAGIC: [u8; 8] = *b"WRNFTS01";
const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub async fn run_session(session: Session, authority: TicketAuthority) -> anyhow::Result<()> {
    tracing::info!("Bridge session handler started");
    tracing::debug!(
        timeout_seconds = CONTROL_TIMEOUT.as_secs(),
        "waiting for Bridge bidirectional control stream"
    );
    let (mut send, mut recv) = tokio::time::timeout(CONTROL_TIMEOUT, session.accept_bi())
        .await
        .context("timed out waiting for the Bridge control stream")?
        .context("failed to accept the Bridge control stream")?;
    tracing::info!("accepted Bridge bidirectional control stream");
    let token = read_hello(&mut recv).await?;
    tracing::debug!("received structurally valid Bridge hello");
    let accepted = authority.consume_launch_token(&token)?;
    send_ack(&mut send, accepted).await?;
    tracing::info!(accepted, "sent Bridge launch acknowledgement");
    anyhow::ensure!(accepted, "Bridge launch token was rejected");

    loop {
        let Some(request) = read_request(&mut recv).await? else {
            tracing::info!("Bridge control stream reached clean EOF");
            break;
        };
        tracing::debug!(request_id = request, "received peer ticket request");
        let response = match authority.issue_ticket() {
            Ok(ticket) => {
                tracing::debug!(
                    request_id = request,
                    expires_at_ms = ticket.expires_at_ms,
                    "peer ticket request succeeded"
                );
                encode_ticket_response(request, 0, ticket.token, ticket.expires_at_ms)
            }
            Err(error) => {
                tracing::warn!(request_id = request, error = ?error, "failed to issue a peer ticket");
                encode_ticket_response(request, 1, [0; 16], 0)
            }
        };
        send.write_all(&response)
            .await
            .context("failed to write a peer ticket response")?;
        tracing::trace!(request_id = request, "wrote peer ticket response");
    }
    send.finish()
        .context("failed to finish the Bridge stream")?;
    session.close(0, b"bridge closed");
    tracing::info!("Bridge WebTransport session closed");
    Ok(())
}

async fn read_hello(recv: &mut RecvStream) -> anyhow::Result<[u8; 16]> {
    let mut bytes = [0_u8; HELLO_LEN];
    recv.read_exact(&mut bytes)
        .await
        .context("failed to read the Bridge hello")?;
    anyhow::ensure!(bytes[0..8] == HELLO_MAGIC, "invalid Bridge hello magic");
    anyhow::ensure!(
        u16::from_be_bytes([bytes[8], bytes[9]]) == BRIDGE_VERSION,
        "unsupported Bridge protocol version"
    );
    tracing::trace!(
        version = u16::from_be_bytes([bytes[8], bytes[9]]),
        reserved = ?&bytes[10..16],
        "decoded Bridge hello header"
    );
    Ok(bytes[16..32].try_into().expect("fixed Bridge token range"))
}

async fn send_ack(send: &mut SendStream, accepted: bool) -> anyhow::Result<()> {
    let mut bytes = [0_u8; ACK_LEN];
    bytes[0..8].copy_from_slice(&ACK_MAGIC);
    bytes[8..10].copy_from_slice(&BRIDGE_VERSION.to_be_bytes());
    bytes[10] = u8::from(!accepted);
    send.write_all(&bytes)
        .await
        .context("failed to write the Bridge acknowledgement")
}

async fn read_request(recv: &mut RecvStream) -> anyhow::Result<Option<u32>> {
    let mut bytes = [0_u8; TICKET_REQUEST_LEN];
    let Some(first) = recv
        .read(&mut bytes[..1])
        .await
        .context("failed to read a Bridge command")?
    else {
        return Ok(None);
    };
    anyhow::ensure!(first == 1, "invalid Bridge command prefix");
    recv.read_exact(&mut bytes[1..])
        .await
        .context("failed to read a complete Bridge command")?;
    anyhow::ensure!(
        bytes[0..8] == TICKET_REQUEST_MAGIC,
        "invalid Bridge command magic"
    );
    anyhow::ensure!(
        u16::from_be_bytes([bytes[8], bytes[9]]) == BRIDGE_VERSION,
        "unsupported Bridge command version"
    );
    let request_id = u32::from_be_bytes(bytes[12..16].try_into().expect("fixed request id range"));
    tracing::trace!(
        request_id,
        version = u16::from_be_bytes([bytes[8], bytes[9]]),
        "decoded Bridge ticket command"
    );
    Ok(Some(request_id))
}

fn encode_ticket_response(
    request_id: u32,
    status: u8,
    token: [u8; 16],
    expires_at_ms: u64,
) -> [u8; TICKET_RESPONSE_LEN] {
    let mut bytes = [0_u8; TICKET_RESPONSE_LEN];
    bytes[0..8].copy_from_slice(&TICKET_RESPONSE_MAGIC);
    bytes[8..10].copy_from_slice(&BRIDGE_VERSION.to_be_bytes());
    bytes[10] = status;
    bytes[12..16].copy_from_slice(&request_id.to_be_bytes());
    bytes[16..32].copy_from_slice(&token);
    bytes[32..40].copy_from_slice(&expires_at_ms.to_be_bytes());
    bytes
}
