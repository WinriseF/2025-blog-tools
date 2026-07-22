use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use bytes::Bytes;
use tokio::task::JoinSet;
use web_transport_quinn::{RecvStream, SendStream, Session};
use winrisef_core::{
    AckStatus, CoverageTracker, ExtentHeader, Hello, HelloAck, LaneHeader, TransferDirection,
    TransferResult,
    protocol::{EXTENT_HEADER_LEN, HELLO_LEN, LANE_HEADER_LEN},
};

use crate::{
    auth::TicketAuthority,
    metrics::{Monitor, Progress, TransferStats},
};

pub const LANE_COUNT: usize = 4;
pub const BLOCK_SIZE: usize = 4 * 1024 * 1024;
pub const EXTENT_SIZE: u64 = 16 * 1024 * 1024;
const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone)]
pub struct TransferSettings {
    pub authenticator: TransferAuthenticator,
    pub max_transfer_size: u64,
    pub metrics_enabled: bool,
}

#[derive(Clone)]
pub enum TransferAuthenticator {
    Fixed([u8; 16]),
    Tickets(TicketAuthority),
}

impl TransferAuthenticator {
    fn consume(&self, token: &[u8; 16]) -> anyhow::Result<bool> {
        match self {
            Self::Fixed(expected) => Ok(crate::auth::constant_time_equal(token, expected)),
            Self::Tickets(authority) => authority.consume_ticket(token),
        }
    }
}

pub async fn run_session(
    session: Session,
    settings: TransferSettings,
) -> anyhow::Result<TransferStats> {
    tracing::info!(
        max_transfer_size = settings.max_transfer_size,
        metrics_enabled = settings.metrics_enabled,
        "memory transfer session handler started"
    );
    tracing::debug!(
        timeout_seconds = CONTROL_TIMEOUT.as_secs(),
        "waiting for transfer control stream"
    );
    let (mut control_send, mut control_recv) =
        tokio::time::timeout(AUTH_TIMEOUT, session.accept_bi())
            .await
            .context("timed out waiting for the control stream")?
            .context("failed to accept the control stream")?;
    tracing::info!("accepted transfer bidirectional control stream");

    let hello = tokio::time::timeout(AUTH_TIMEOUT, read_hello(&mut control_recv))
        .await
        .context("timed out waiting for transfer authentication")??;
    tracing::info!(
        direction = ?hello.direction,
        lanes = hello.lanes,
        block_size = hello.block_size,
        extent_size = hello.extent_size,
        total_size = hello.total_size,
        "decoded transfer hello"
    );
    if !settings.authenticator.consume(&hello.token)? {
        tracing::warn!("transfer authentication failed");
        send_ack(&mut control_send, hello, AckStatus::AuthenticationFailed).await?;
        anyhow::bail!("session authentication failed");
    }
    tracing::info!("transfer authentication succeeded");
    if !valid_configuration(hello, settings.max_transfer_size) {
        tracing::warn!(
            expected_lanes = LANE_COUNT,
            expected_block_size = BLOCK_SIZE,
            expected_extent_size = EXTENT_SIZE,
            "transfer configuration was rejected"
        );
        send_ack(&mut control_send, hello, AckStatus::InvalidConfiguration).await?;
        anyhow::bail!("session requested an invalid transfer configuration");
    }
    send_ack(&mut control_send, hello, AckStatus::Accepted).await?;
    tracing::info!("transfer configuration accepted");

    let direction = match hello.direction {
        TransferDirection::BrowserToAgentMemory => "browser-to-agent-memory",
        TransferDirection::AgentToBrowserMemory => "agent-to-browser-memory",
    };
    let monitor = Monitor::start(direction, settings.metrics_enabled);
    let progress = monitor.progress();

    let outcome = match hello.direction {
        TransferDirection::BrowserToAgentMemory => {
            receive_memory(&session, hello, Arc::clone(&progress)).await
        }
        TransferDirection::AgentToBrowserMemory => {
            send_memory(&session, hello, Arc::clone(&progress)).await
        }
    };
    if let Err(error) = &outcome {
        tracing::error!(error = ?error, "memory payload phase failed");
    }
    let stats = monitor.finish().await;
    log_quic_summary(&session, direction);
    let status = if outcome.is_ok() {
        AckStatus::Accepted
    } else {
        AckStatus::TransferFailed
    };
    let elapsed_nanos = u64::try_from(stats.elapsed.as_nanos()).unwrap_or(u64::MAX);
    let result = TransferResult {
        status,
        bytes: stats.bytes,
        elapsed_nanos,
    };
    control_send
        .write_all(&result.encode())
        .await
        .context("failed to write the final transfer result")?;
    control_send
        .finish()
        .context("failed to finish the control stream")?;
    tracing::debug!(
        ?status,
        bytes = stats.bytes,
        elapsed_nanos,
        "sent final transfer result"
    );

    if outcome.is_ok() {
        tokio::time::timeout(CONTROL_TIMEOUT, control_send.stopped())
            .await
            .context("timed out waiting for the browser to consume the result")??;
        session.close(0, b"complete");
    } else {
        session.close(1, b"transfer failed");
    }
    outcome?;
    tracing::info!(
        direction,
        lanes = LANE_COUNT,
        bytes = stats.bytes,
        elapsed_ms = stats.elapsed.as_secs_f64() * 1000.0,
        average_mbps = stats.average_mbps,
        "memory transfer session completed successfully"
    );
    Ok(stats)
}

fn log_quic_summary(session: &Session, direction: &'static str) {
    let stats = quinn::Connection::stats(session);
    tracing::info!(
        direction,
        rtt_ms = stats.path.rtt.as_secs_f64() * 1000.0,
        congestion_window_bytes = stats.path.cwnd,
        congestion_events = stats.path.congestion_events,
        lost_packets = stats.path.lost_packets,
        lost_bytes = stats.path.lost_bytes,
        sent_packets = stats.path.sent_packets,
        current_mtu = stats.path.current_mtu,
        black_holes_detected = stats.path.black_holes_detected,
        udp_tx_datagrams = stats.udp_tx.datagrams,
        udp_tx_bytes = stats.udp_tx.bytes,
        udp_tx_ios = stats.udp_tx.ios,
        udp_rx_datagrams = stats.udp_rx.datagrams,
        udp_rx_bytes = stats.udp_rx.bytes,
        udp_rx_ios = stats.udp_rx.ios,
        tx_data_blocked = stats.frame_tx.data_blocked,
        tx_stream_data_blocked = stats.frame_tx.stream_data_blocked,
        tx_streams_blocked_uni = stats.frame_tx.streams_blocked_uni,
        tx_max_data = stats.frame_tx.max_data,
        tx_max_stream_data = stats.frame_tx.max_stream_data,
        rx_data_blocked = stats.frame_rx.data_blocked,
        rx_stream_data_blocked = stats.frame_rx.stream_data_blocked,
        rx_streams_blocked_uni = stats.frame_rx.streams_blocked_uni,
        rx_max_data = stats.frame_rx.max_data,
        rx_max_stream_data = stats.frame_rx.max_stream_data,
        "QUIC payload path summary"
    );
}

async fn read_hello(recv: &mut RecvStream) -> anyhow::Result<Hello> {
    let mut bytes = [0; HELLO_LEN];
    recv.read_exact(&mut bytes)
        .await
        .context("failed to read the session hello")?;
    Hello::decode(bytes).context("invalid session hello")
}

async fn send_ack(send: &mut SendStream, hello: Hello, status: AckStatus) -> anyhow::Result<()> {
    let ack = HelloAck {
        status,
        lanes: hello.lanes,
        block_size: hello.block_size,
        extent_size: hello.extent_size,
        total_size: hello.total_size,
    };
    send.write_all(&ack.encode())
        .await
        .context("failed to write the hello acknowledgement")
}

fn valid_configuration(hello: Hello, max_transfer_size: u64) -> bool {
    usize::from(hello.lanes) == LANE_COUNT
        && hello.block_size as usize == BLOCK_SIZE
        && hello.extent_size == EXTENT_SIZE
        && hello.total_size <= max_transfer_size
}

async fn send_memory(
    session: &Session,
    hello: Hello,
    progress: Arc<Progress>,
) -> anyhow::Result<()> {
    let zero_block = Bytes::from(vec![0; BLOCK_SIZE]);
    tracing::debug!(
        shared_zero_block_bytes = zero_block.len(),
        "allocated zero-copy benchmark payload"
    );
    let mut lanes = JoinSet::new();
    for lane_id in 0..LANE_COUNT {
        lanes.spawn(send_lane(
            session.clone(),
            lane_id as u16,
            hello,
            zero_block.clone(),
            Arc::clone(&progress),
        ));
    }
    while let Some(result) = lanes.join_next().await {
        result.context("memory sender lane task panicked")??;
    }
    anyhow::ensure!(
        progress.bytes() == hello.total_size,
        "memory sender byte count does not match the declared size"
    );
    Ok(())
}

async fn send_lane(
    session: Session,
    lane_id: u16,
    hello: Hello,
    zero_block: Bytes,
    progress: Arc<Progress>,
) -> anyhow::Result<()> {
    tracing::debug!(lane_id, "Agent-to-browser lane started");
    let mut send = session
        .open_uni()
        .await
        .context("failed to open a data stream")?;
    let lane_header = LaneHeader {
        lane_id,
        lane_count: LANE_COUNT as u16,
        total_size: hello.total_size,
        extent_size: EXTENT_SIZE,
    };
    send.write_all(&lane_header.encode())
        .await
        .context("failed to write a lane header")?;

    let mut extent_index = 0_u64;
    while let Some(extent) = lane_extent(lane_id, extent_index, hello.total_size) {
        send.write_all(
            &ExtentHeader {
                offset: extent.offset,
                len: extent.len,
            }
            .encode(),
        )
        .await
        .context("failed to write an extent header")?;

        let mut remaining = extent.len;
        while remaining > 0 {
            let chunk_len = usize::try_from(remaining.min(BLOCK_SIZE as u64))
                .expect("chunk length is bounded by BLOCK_SIZE");
            send.write_chunk(zero_block.slice(..chunk_len))
                .await
                .context("failed to queue zero-copy memory payload")?;
            progress.add(chunk_len);
            remaining -= chunk_len as u64;
        }
        extent_index += 1;
    }

    send.write_all(&ExtentHeader::END.encode())
        .await
        .context("failed to write the lane terminator")?;
    send.finish().context("failed to finish a data stream")?;
    match send
        .stopped()
        .await
        .context("data stream was not acknowledged")?
    {
        None => {
            tracing::debug!(lane_id, "Agent-to-browser lane completed");
            Ok(())
        }
        Some(code) => anyhow::bail!("browser stopped data stream with code {code}"),
    }
}

fn lane_extent(lane_id: u16, extent_index: u64, total_size: u64) -> Option<ExtentHeader> {
    let extent_number = extent_index
        .checked_mul(LANE_COUNT as u64)?
        .checked_add(u64::from(lane_id))?;
    let offset = extent_number.checked_mul(EXTENT_SIZE)?;
    if offset >= total_size {
        return None;
    }
    Some(ExtentHeader {
        offset,
        len: EXTENT_SIZE.min(total_size - offset),
    })
}

async fn receive_memory(
    session: &Session,
    hello: Hello,
    progress: Arc<Progress>,
) -> anyhow::Result<()> {
    let coverage = Arc::new(CoverageTracker::new(hello.total_size, EXTENT_SIZE)?);
    let seen_lanes = Arc::new(AtomicU64::new(0));
    let mut lanes = JoinSet::new();
    for _ in 0..LANE_COUNT {
        let recv = session
            .accept_uni()
            .await
            .context("failed to accept a browser data stream")?;
        lanes.spawn(receive_lane(
            recv,
            hello,
            Arc::clone(&coverage),
            Arc::clone(&seen_lanes),
            Arc::clone(&progress),
        ));
    }
    while let Some(result) = lanes.join_next().await {
        result.context("memory receiver lane task panicked")??;
    }
    let expected_lanes = (1_u64 << LANE_COUNT) - 1;
    anyhow::ensure!(
        seen_lanes.load(Ordering::Relaxed) == expected_lanes,
        "one or more lane identifiers were not received"
    );
    anyhow::ensure!(
        coverage.is_complete()?,
        "received extent coverage is incomplete"
    );
    anyhow::ensure!(
        progress.bytes() == hello.total_size,
        "memory receiver byte count does not match the declared size"
    );
    Ok(())
}

async fn receive_lane(
    mut recv: RecvStream,
    hello: Hello,
    coverage: Arc<CoverageTracker>,
    seen_lanes: Arc<AtomicU64>,
    progress: Arc<Progress>,
) -> anyhow::Result<()> {
    let mut lane_bytes = [0; LANE_HEADER_LEN];
    recv.read_exact(&mut lane_bytes)
        .await
        .context("failed to read a lane header")?;
    let lane = LaneHeader::decode(lane_bytes).context("invalid lane header")?;
    tracing::debug!(
        lane_id = lane.lane_id,
        lane_count = lane.lane_count,
        total_size = lane.total_size,
        extent_size = lane.extent_size,
        "browser-to-Agent lane header accepted"
    );
    anyhow::ensure!(
        lane.lane_count as usize == LANE_COUNT
            && lane.total_size == hello.total_size
            && lane.extent_size == EXTENT_SIZE,
        "lane header does not match the session"
    );
    let lane_bit = 1_u64 << lane.lane_id;
    let previous = seen_lanes.fetch_or(lane_bit, Ordering::Relaxed);
    anyhow::ensure!(previous & lane_bit == 0, "duplicate lane identifier");

    loop {
        let mut extent_bytes = [0; EXTENT_HEADER_LEN];
        recv.read_exact(&mut extent_bytes)
            .await
            .context("failed to read an extent header")?;
        let extent = ExtentHeader::decode(extent_bytes).context("invalid extent header")?;
        if extent.is_end() {
            break;
        }
        coverage.record(extent)?;

        let mut remaining = extent.len;
        while remaining > 0 {
            let max_chunk_len = usize::try_from(remaining.min(BLOCK_SIZE as u64))
                .expect("chunk length is bounded by BLOCK_SIZE");
            let chunk = recv
                .read_chunk(max_chunk_len, true)
                .await
                .context("failed to read zero-copy memory payload")?
                .context("data stream ended inside a declared extent")?;
            let chunk_len = chunk.bytes.len();
            anyhow::ensure!(chunk_len > 0, "received an empty memory payload chunk");
            progress.add(chunk_len);
            remaining -= chunk_len as u64;
        }
    }

    let mut trailing = [0_u8; 1];
    anyhow::ensure!(
        recv.read(&mut trailing).await?.is_none(),
        "data stream contains bytes after its terminator"
    );
    tracing::debug!(lane_id = lane.lane_id, "browser-to-Agent lane completed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{BLOCK_SIZE, EXTENT_SIZE, LANE_COUNT, lane_extent, valid_configuration};
    use winrisef_core::{Hello, TransferDirection};

    fn hello() -> Hello {
        Hello {
            direction: TransferDirection::AgentToBrowserMemory,
            lanes: 4,
            block_size: BLOCK_SIZE as u32,
            extent_size: EXTENT_SIZE,
            total_size: 1024,
            token: [1; 16],
        }
    }

    #[test]
    fn accepts_only_fixed_hot_path_shape() {
        assert!(valid_configuration(hello(), 2048));
        let mut wrong = hello();
        wrong.lanes = 2;
        assert!(!valid_configuration(wrong, 2048));
    }

    #[test]
    fn sixty_four_mib_uses_all_four_lanes() {
        let total_size = 64 * 1024 * 1024;
        let mut extents = (0..LANE_COUNT)
            .map(|lane_id| lane_extent(lane_id as u16, 0, total_size).unwrap())
            .collect::<Vec<_>>();
        extents.sort_unstable_by_key(|extent| extent.offset);

        assert_eq!(extents.len(), LANE_COUNT);
        for (index, extent) in extents.into_iter().enumerate() {
            assert_eq!(extent.offset, index as u64 * EXTENT_SIZE);
            assert_eq!(extent.len, EXTENT_SIZE);
            assert!(lane_extent(index as u16, 1, total_size).is_none());
        }
    }
}
