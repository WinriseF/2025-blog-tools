use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
use socket2::{Domain, Protocol, SockAddr, SockRef, Socket, Type};

const SOCKET_BUFFER_SIZE: usize = 16 * 1024 * 1024;
const CONNECTION_WINDOW: u64 = 128 * 1024 * 1024;
const STREAM_WINDOW: u64 = 32 * 1024 * 1024;
const INITIAL_CONGESTION_WINDOW: u64 = 1024 * 1024;
const INITIAL_RTT: Duration = Duration::from_millis(10);

pub fn endpoint(
    listen: SocketAddr,
    crypto: quinn::crypto::rustls::QuicServerConfig,
) -> anyhow::Result<quinn::Endpoint> {
    tracing::debug!(
        %listen,
        socket_buffer_bytes = SOCKET_BUFFER_SIZE,
        connection_window_bytes = CONNECTION_WINDOW,
        stream_window_bytes = STREAM_WINDOW,
        congestion_controller = "bbr",
        initial_congestion_window_bytes = INITIAL_CONGESTION_WINDOW,
        initial_rtt_ms = INITIAL_RTT.as_millis(),
        send_fairness = true,
        max_bidi_streams = 8,
        max_uni_streams = 32,
        keep_alive_seconds = 5,
        idle_timeout_seconds = 30,
        "configuring Quinn transport"
    );
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(8));
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(32));
    transport.stream_receive_window(quinn::VarInt::from_u32(STREAM_WINDOW as u32));
    transport.receive_window(quinn::VarInt::from_u32(CONNECTION_WINDOW as u32));
    transport.send_window(CONNECTION_WINDOW);
    transport.send_fairness(true);
    transport.initial_rtt(INITIAL_RTT);
    let mut congestion = quinn::congestion::BbrConfig::default();
    congestion.initial_window(INITIAL_CONGESTION_WINDOW);
    transport.congestion_controller_factory(Arc::new(congestion));
    transport.keep_alive_interval(Some(Duration::from_secs(5)));
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into()?));

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    server_config.transport_config(Arc::new(transport));

    let socket = bind_udp_socket(listen)?;
    socket.set_nonblocking(true)?;
    tracing::info!(local_addr = %socket.local_addr()?, "bound Agent UDP socket");
    tune_udp_socket(&socket);

    Ok(quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        Arc::new(quinn::TokioRuntime),
    )?)
}

fn bind_udp_socket(listen: SocketAddr) -> anyhow::Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::for_address(listen), Type::DGRAM, Some(Protocol::UDP))?;
    if listen.is_ipv6() {
        socket
            .set_only_v6(false)
            .context("failed to enable dual-stack UDP on the Agent socket")?;
    }
    socket.bind(&SockAddr::from(listen))?;
    Ok(socket.into())
}

fn tune_udp_socket(socket: &std::net::UdpSocket) {
    let socket = SockRef::from(socket);
    log_tuning_result("receive", socket.set_recv_buffer_size(SOCKET_BUFFER_SIZE));
    log_tuning_result("send", socket.set_send_buffer_size(SOCKET_BUFFER_SIZE));
    match socket.recv_buffer_size() {
        Ok(actual) => tracing::info!(
            direction = "receive",
            actual_bytes = actual,
            "UDP socket buffer size"
        ),
        Err(error) => {
            tracing::warn!(%error, direction = "receive", "could not read UDP socket buffer size")
        }
    }
    match socket.send_buffer_size() {
        Ok(actual) => tracing::info!(
            direction = "send",
            actual_bytes = actual,
            "UDP socket buffer size"
        ),
        Err(error) => {
            tracing::warn!(%error, direction = "send", "could not read UDP socket buffer size")
        }
    }
}

fn log_tuning_result(direction: &'static str, result: io::Result<()>) {
    match result {
        Ok(()) => tracing::debug!(
            direction,
            requested_bytes = SOCKET_BUFFER_SIZE,
            "requested UDP socket buffer size"
        ),
        Err(error) => {
            tracing::warn!(%error, direction, requested_bytes = SOCKET_BUFFER_SIZE, "could not enlarge UDP socket buffer");
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::bind_udp_socket;
    use std::{net::UdpSocket, time::Duration};

    #[test]
    fn ipv6_wildcard_udp_socket_accepts_both_families() {
        let receiver = bind_udp_socket("[::]:0".parse().unwrap()).unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let port = receiver.local_addr().unwrap().port();
        for target in [format!("127.0.0.1:{port}"), format!("[::1]:{port}")] {
            let sender = UdpSocket::bind(if target.starts_with('[') {
                "[::1]:0"
            } else {
                "127.0.0.1:0"
            })
            .unwrap();
            sender.send_to(b"dual-stack", target).unwrap();
            let mut bytes = [0_u8; 16];
            let (count, _) = receiver.recv_from(&mut bytes).unwrap();
            assert_eq!(&bytes[..count], b"dual-stack");
        }
    }
}
