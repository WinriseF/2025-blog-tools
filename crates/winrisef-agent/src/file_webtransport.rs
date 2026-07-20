use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncReadExt,
    sync::mpsc,
    task::JoinSet,
};
use web_transport_quinn::{RecvStream, SendStream, Session};

use crate::file_transfer::{
    FILE_IO_BLOCK_BYTES, FILE_WEBTRANSPORT_CONNECTIONS, FILE_WEBTRANSPORT_EXTENT_BYTES,
    FILE_WEBTRANSPORT_LANES_PER_CONNECTION, FileTransferManager, NativeFileDirection,
    SegmentLease, WebTransportConnectionLease,
};

pub const FILE_WEBTRANSPORT_PATH: &str = "/winrisef/file/v1";
const CONTROL_FRAME_MAX_BYTES: usize = 64 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);
const EXTENT_HEADER_BYTES: usize = 16;
const END_OFFSET: u64 = u64::MAX;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum FileControlInput {
    Hello {
        version: u16,
        #[serde(rename = "transferId")]
        transfer_id: String,
        token: String,
        #[serde(rename = "connectionIndex")]
        connection_index: usize,
        direction: NativeFileDirection,
        #[serde(rename = "peerDeviceId")]
        peer_device_id: String,
        lanes: usize,
        #[serde(rename = "blockBytes")]
        block_bytes: usize,
        #[serde(rename = "extentBytes")]
        extent_bytes: u64,
        #[serde(rename = "totalBytes")]
        total_bytes: u64,
    },
    Complete,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum FileControlOutput<'a> {
    HelloAck {
        version: u16,
        accepted: bool,
        #[serde(rename = "connectionIndex")]
        connection_index: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
    },
    PayloadComplete {
        #[serde(rename = "connectionIndex")]
        connection_index: usize,
    },
    TransferComplete,
}

pub async fn run_session(session: Session, files: FileTransferManager) -> anyhow::Result<()> {
    let (mut control_send, mut control_recv) = tokio::time::timeout(CONTROL_TIMEOUT, session.accept_bi())
        .await
        .context("timed out waiting for Native File V1 control")?
        .context("failed to accept Native File V1 control")?;
    let hello = read_control(&mut control_recv)
        .await?
        .context("Native File V1 control ended before hello")?;
    let FileControlInput::Hello {
        version,
        transfer_id,
        token,
        connection_index,
        direction,
        peer_device_id,
        lanes,
        block_bytes,
        extent_bytes,
        total_bytes,
    } = hello
    else {
        anyhow::bail!("Native File V1 hello is required");
    };
    let configuration_valid = version == 1
        && connection_index < FILE_WEBTRANSPORT_CONNECTIONS
        && lanes == FILE_WEBTRANSPORT_LANES_PER_CONNECTION
        && block_bytes == FILE_IO_BLOCK_BYTES
        && extent_bytes == FILE_WEBTRANSPORT_EXTENT_BYTES;
    if !configuration_valid {
        write_control(
            &mut control_send,
            &FileControlOutput::HelloAck {
                version: 1,
                accepted: false,
                connection_index,
                error: Some("Native File V1 configuration is invalid"),
            },
        )
        .await?;
        anyhow::bail!("Native File V1 configuration is invalid");
    }
    let token = parse_hex::<16>(&token).context("Native File V1 token is invalid")?;
    let connection = match files.begin_webtransport_connection(
        &transfer_id,
        &token,
        connection_index,
        direction,
        &peer_device_id,
    ) {
        Ok(connection) if connection.total_bytes() == total_bytes => connection,
        Ok(_) => {
            write_control(
                &mut control_send,
                &FileControlOutput::HelloAck {
                    version: 1,
                    accepted: false,
                    connection_index,
                    error: Some("Native File V1 total size does not match"),
                },
            )
            .await?;
            anyhow::bail!("Native File V1 total size does not match");
        }
        Err(error) => {
            write_control(
                &mut control_send,
                &FileControlOutput::HelloAck {
                    version: 1,
                    accepted: false,
                    connection_index,
                    error: Some("Native File V1 authorization was rejected"),
                },
            )
            .await?;
            return Err(error);
        }
    };
    write_control(
        &mut control_send,
        &FileControlOutput::HelloAck {
            version: 1,
            accepted: true,
            connection_index,
            error: None,
        },
    )
    .await?;

    let payload = match direction {
        NativeFileDirection::AgentToBrowser => {
            send_connection(&session, &files, &connection).await
        }
        NativeFileDirection::BrowserToAgent => {
            receive_connection(&session, &files, &connection).await
        }
    };
    if let Err(error) = payload {
        files.fail_transfer(connection.transfer_id(), "Native File V1 WebTransport payload failed");
        session.close(1, b"native file failed");
        return Err(error);
    }
    write_control(
        &mut control_send,
        &FileControlOutput::PayloadComplete { connection_index },
    )
    .await?;

    if direction == NativeFileDirection::BrowserToAgent && connection_index == 0 {
        let completion = tokio::time::timeout(CONTROL_TIMEOUT, read_control(&mut control_recv))
            .await
            .context("timed out waiting for Native File V1 completion")??
            .context("Native File V1 control ended before completion")?;
        anyhow::ensure!(matches!(completion, FileControlInput::Complete), "Native File V1 completion is invalid");
        files.complete_webtransport_receive(&connection)?;
        write_control(&mut control_send, &FileControlOutput::TransferComplete).await?;
    }
    control_send
        .finish()
        .context("failed to finish Native File V1 control")?;
    log_quic_summary(&session, direction, connection_index);
    session.close(0, b"native file connection complete");
    Ok(())
}

async fn send_connection(
    session: &Session,
    files: &FileTransferManager,
    connection: &WebTransportConnectionLease,
) -> anyhow::Result<()> {
    let mut lanes = JoinSet::new();
    for lane_index in 0..FILE_WEBTRANSPORT_LANES_PER_CONNECTION {
        lanes.spawn(send_lane(
            session.clone(),
            files.clone(),
            connection.clone(),
            lane_index,
        ));
    }
    while let Some(result) = lanes.join_next().await {
        result.context("Native File V1 sender lane panicked")??;
    }
    Ok(())
}

async fn send_lane(
    session: Session,
    files: FileTransferManager,
    connection: WebTransportConnectionLease,
    lane_index: usize,
) -> anyhow::Result<()> {
    let mut send = session.open_uni().await.context("failed to open a Native File V1 lane")?;
    send.write_all(&(lane_index as u16).to_be_bytes()).await?;
    for (offset, len) in assigned_extents(&connection, lane_index) {
        send.write_all(&extent_header(offset, len)).await?;
        let lease = files.begin_webtransport_extent(&connection, lane_index, offset, len)?;
        let lease = send_extent(&mut send, lease).await?;
        files.commit_segment(lease)?;
    }
    send.write_all(&extent_header(END_OFFSET, 0)).await?;
    send.finish().context("failed to finish a Native File V1 lane")?;
    Ok(())
}

async fn receive_connection(
    session: &Session,
    files: &FileTransferManager,
    connection: &WebTransportConnectionLease,
) -> anyhow::Result<()> {
    let mut seen = [false; FILE_WEBTRANSPORT_LANES_PER_CONNECTION];
    let mut lanes = JoinSet::new();
    for _ in 0..FILE_WEBTRANSPORT_LANES_PER_CONNECTION {
        let mut recv = session.accept_uni().await.context("failed to accept a Native File V1 lane")?;
        let lane_index = usize::from(recv.read_u16().await.context("failed to read Native File V1 lane index")?);
        anyhow::ensure!(lane_index < seen.len() && !seen[lane_index], "Native File V1 lane index is duplicate or invalid");
        seen[lane_index] = true;
        lanes.spawn(receive_lane(recv, files.clone(), connection.clone(), lane_index));
    }
    while let Some(result) = lanes.join_next().await {
        result.context("Native File V1 receiver lane panicked")??;
    }
    anyhow::ensure!(seen.into_iter().all(|value| value), "Native File V1 lanes are incomplete");
    Ok(())
}

async fn receive_lane(
    mut recv: RecvStream,
    files: FileTransferManager,
    connection: WebTransportConnectionLease,
    lane_index: usize,
) -> anyhow::Result<()> {
    loop {
        let (offset, len) = read_extent_header(&mut recv).await?;
        if offset == END_OFFSET {
            anyhow::ensure!(len == 0, "Native File V1 lane terminator is invalid");
            break;
        }
        let lease = files.begin_webtransport_extent(&connection, lane_index, offset, len)?;
        let lease = receive_extent(&mut recv, lease).await?;
        files.commit_segment(lease)?;
    }
    Ok(())
}

async fn send_extent(send: &mut SendStream, lease: SegmentLease) -> anyhow::Result<SegmentLease> {
    let (payload_tx, mut payload_rx) = mpsc::channel::<Vec<u8>>(1);
    let (recycle_tx, mut recycle_rx) = mpsc::channel::<Vec<u8>>(1);
    recycle_tx.send(vec![0_u8; FILE_IO_BLOCK_BYTES]).await?;
    let worker = tokio::task::spawn_blocking(move || -> anyhow::Result<SegmentLease> {
        let mut position = lease.offset();
        let end = position + lease.len();
        while position < end {
            let mut buffer = recycle_rx.blocking_recv().context("Native File V1 read buffer closed")?;
            let count = usize::try_from((end - position).min(FILE_IO_BLOCK_BYTES as u64)).expect("file block is bounded");
            read_exact_at(&lease, &mut buffer[..count], position)?;
            lease.touch()?;
            buffer.truncate(count);
            payload_tx.blocking_send(buffer).map_err(|_| anyhow::anyhow!("Native File V1 sender closed"))?;
            position += count as u64;
        }
        Ok(lease)
    });
    while let Some(mut buffer) = payload_rx.recv().await {
        send.write_all(&buffer).await?;
        buffer.resize(FILE_IO_BLOCK_BYTES, 0);
        recycle_tx.send(buffer).await.map_err(|_| anyhow::anyhow!("Native File V1 read recycler closed"))?;
    }
    worker.await.context("Native File V1 read worker panicked")?
}

async fn receive_extent(recv: &mut RecvStream, lease: SegmentLease) -> anyhow::Result<SegmentLease> {
    let total = lease.len();
    let (payload_tx, mut payload_rx) = mpsc::channel::<Vec<u8>>(1);
    let (recycle_tx, mut recycle_rx) = mpsc::channel::<Vec<u8>>(1);
    recycle_tx.send(vec![0_u8; FILE_IO_BLOCK_BYTES]).await?;
    let worker = tokio::task::spawn_blocking(move || -> anyhow::Result<SegmentLease> {
        let mut position = lease.offset();
        while let Some(buffer) = payload_rx.blocking_recv() {
            write_all_at(&lease, &buffer, position)?;
            lease.touch()?;
            position += buffer.len() as u64;
            let mut buffer = buffer;
            buffer.resize(FILE_IO_BLOCK_BYTES, 0);
            recycle_tx.blocking_send(buffer).map_err(|_| anyhow::anyhow!("Native File V1 write recycler closed"))?;
        }
        anyhow::ensure!(position == lease.offset() + lease.len(), "Native File V1 extent length is inconsistent");
        Ok(lease)
    });
    let mut remaining = total;
    while remaining > 0 {
        let mut buffer = recycle_rx.recv().await.context("Native File V1 write buffer closed")?;
        let count = usize::try_from(remaining.min(FILE_IO_BLOCK_BYTES as u64)).expect("file block is bounded");
        buffer.resize(count, 0);
        recv.read_exact(&mut buffer).await.context("Native File V1 extent ended early")?;
        payload_tx.send(buffer).await.map_err(|_| anyhow::anyhow!("Native File V1 write worker closed"))?;
        remaining -= count as u64;
    }
    drop(payload_tx);
    worker.await.context("Native File V1 write worker panicked")?
}

fn assigned_extents(
    connection: &WebTransportConnectionLease,
    lane_index: usize,
) -> impl Iterator<Item = (u64, u64)> {
    let first = connection.connection_index() * FILE_WEBTRANSPORT_LANES_PER_CONNECTION + lane_index;
    let stride = FILE_WEBTRANSPORT_CONNECTIONS * FILE_WEBTRANSPORT_LANES_PER_CONNECTION;
    let total = connection.total_bytes();
    (first..)
        .step_by(stride)
        .map(move |index| index as u64 * FILE_WEBTRANSPORT_EXTENT_BYTES)
        .take_while(move |offset| *offset < total)
        .map(move |offset| (offset, FILE_WEBTRANSPORT_EXTENT_BYTES.min(total - offset)))
}

fn extent_header(offset: u64, len: u64) -> [u8; EXTENT_HEADER_BYTES] {
    let mut header = [0_u8; EXTENT_HEADER_BYTES];
    header[..8].copy_from_slice(&offset.to_be_bytes());
    header[8..].copy_from_slice(&len.to_be_bytes());
    header
}

async fn read_extent_header(recv: &mut RecvStream) -> anyhow::Result<(u64, u64)> {
    let mut header = [0_u8; EXTENT_HEADER_BYTES];
    recv.read_exact(&mut header).await.context("failed to read Native File V1 extent header")?;
    let offset = u64::from_be_bytes(header[..8].try_into().expect("extent offset has fixed size"));
    let len = u64::from_be_bytes(header[8..].try_into().expect("extent length has fixed size"));
    Ok((offset, len))
}

fn read_exact_at(lease: &SegmentLease, mut buffer: &mut [u8], mut offset: u64) -> anyhow::Result<()> {
    while !buffer.is_empty() {
        let read = lease.read_at(buffer, offset).context("failed to read a Native File V1 extent")?;
        anyhow::ensure!(read > 0, "Native File V1 source ended early");
        offset += read as u64;
        buffer = &mut buffer[read..];
    }
    Ok(())
}

fn write_all_at(lease: &SegmentLease, mut buffer: &[u8], mut offset: u64) -> anyhow::Result<()> {
    while !buffer.is_empty() {
        let written = lease.write_at(buffer, offset).context("failed to write a Native File V1 extent")?;
        anyhow::ensure!(written > 0, "Native File V1 destination accepted an empty write");
        offset += written as u64;
        buffer = &buffer[written..];
    }
    Ok(())
}

async fn read_control(recv: &mut RecvStream) -> anyhow::Result<Option<FileControlInput>> {
    let mut prefix = [0_u8; 4];
    let Some(first) = recv.read(&mut prefix[..1]).await? else {
        return Ok(None);
    };
    anyhow::ensure!(first == 1, "Native File V1 control prefix is invalid");
    recv.read_exact(&mut prefix[1..]).await?;
    let len = usize::try_from(u32::from_be_bytes(prefix)).expect("u32 fits usize");
    anyhow::ensure!(len > 0 && len <= CONTROL_FRAME_MAX_BYTES, "Native File V1 control frame is invalid");
    let mut body = vec![0_u8; len];
    recv.read_exact(&mut body).await?;
    serde_json::from_slice(&body).context("Native File V1 control JSON is invalid").map(Some)
}

async fn write_control<T: Serialize>(send: &mut SendStream, value: &T) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    anyhow::ensure!(body.len() <= CONTROL_FRAME_MAX_BYTES, "Native File V1 response is too large");
    send.write_all(&(body.len() as u32).to_be_bytes()).await?;
    send.write_all(&body).await?;
    Ok(())
}

fn parse_hex<const N: usize>(value: &str) -> anyhow::Result<[u8; N]> {
    anyhow::ensure!(value.len() == N * 2, "hex token has the wrong length");
    let mut bytes = [0_u8; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).context("hex token is malformed")?;
    }
    Ok(bytes)
}

fn log_quic_summary(session: &Session, direction: NativeFileDirection, connection_index: usize) {
    let stats = quinn::Connection::stats(session);
    tracing::info!(
        ?direction,
        connection_index,
        rtt_ms = stats.path.rtt.as_secs_f64() * 1000.0,
        congestion_window_bytes = stats.path.cwnd,
        lost_packets = stats.path.lost_packets,
        lost_bytes = stats.path.lost_bytes,
        current_mtu = stats.path.current_mtu,
        "Native File V1 QUIC connection summary"
    );
}
