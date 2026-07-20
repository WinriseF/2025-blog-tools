use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
use socket2::SockRef;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::{JoinHandle, JoinSet},
};

use crate::{
    auth::TicketAuthority,
    file_http,
    file_transfer::{FILE_HTTP_PARALLELISM, FileTransferManager},
    metrics::Monitor,
};

pub const LNA_HTTP_BASE_PATH: &str = "/winrisef/lna/v1";
const PROBE_PATH: &str = "/winrisef/lna/v1/probe";
const BENCHMARK_PATH: &str = "/winrisef/lna/v1/benchmark";
const TICKET_HEADER: &str = "x-winrisef-ticket";
const FILE_TOKEN_HEADER: &str = "x-winrisef-transfer-token";
const MAX_HEADER_BYTES: usize = 16 * 1024;
const IO_BLOCK_BYTES: usize = 1024 * 1024;
const SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const MAX_BENCHMARK_REQUEST_BYTES: u64 = 64 * 1024 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(60);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct LnaHttpSettings {
    pub allowed_origin: String,
    pub authority: TicketAuthority,
    pub max_transfer_size: u64,
    pub max_sessions: usize,
    pub metrics_enabled: bool,
    pub files: FileTransferManager,
}

struct LnaHttpRuntime {
    settings: Arc<LnaHttpSettings>,
    active_requests: Arc<Semaphore>,
    file_requests: Arc<Semaphore>,
    zero_block: Arc<Vec<u8>>,
}

pub async fn start(
    listen: SocketAddr,
    settings: LnaHttpSettings,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind the LNA HTTP/TCP endpoint at {listen}"))?;
    let local_addr = listener.local_addr()?;
    tracing::info!(
        listen = %local_addr,
        allowed_origin = %settings.allowed_origin,
        max_sessions = settings.max_sessions,
        max_request_bytes = MAX_BENCHMARK_REQUEST_BYTES,
        "LNA HTTP/TCP endpoint is ready"
    );
    Ok(tokio::spawn(run(listener, settings, shutdown)))
}

async fn run(
    listener: TcpListener,
    settings: LnaHttpSettings,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let settings = Arc::new(settings);
    let active_requests = Arc::new(Semaphore::new(settings.max_sessions));
    let file_requests = Arc::new(Semaphore::new(FILE_HTTP_PARALLELISM));
    let connection_limit = settings.max_sessions.saturating_mul(4).saturating_add(8);
    let connections = Arc::new(Semaphore::new(connection_limit));
    let zero_block = Arc::new(vec![0_u8; IO_BLOCK_BYTES]);
    let runtime = Arc::new(LnaHttpRuntime {
        settings,
        active_requests,
        file_requests,
        zero_block,
    });
    let mut tasks = JoinSet::new();

    loop {
        let accepted = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    tracing::info!("LNA HTTP/TCP shutdown signal received");
                    break;
                }
                continue;
            }
            accepted = listener.accept() => accepted,
        };
        let (stream, remote) = accepted.context("LNA HTTP/TCP listener stopped accepting")?;
        let permit = match Arc::clone(&connections).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                tracing::warn!(%remote, connection_limit, "rejecting excess LNA TCP connection");
                continue;
            }
        };
        tune_socket(&stream, remote);
        let runtime = Arc::clone(&runtime);
        tasks.spawn(async move {
            let _permit = permit;
            if let Err(error) = handle_connection(stream, remote, runtime).await {
                tracing::warn!(%remote, error = ?error, "LNA HTTP/TCP connection ended with an error");
            }
        });
        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result {
                tracing::error!(error = ?error, "LNA HTTP/TCP connection task panicked");
            }
        }
    }

    tasks.abort_all();
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            tracing::error!(error = ?error, "LNA HTTP/TCP connection task panicked during shutdown");
        }
    }
    tracing::info!("LNA HTTP/TCP endpoint shut down cleanly");
    Ok(())
}

fn tune_socket(stream: &TcpStream, remote: SocketAddr) {
    if let Err(error) = stream.set_nodelay(true) {
        tracing::warn!(%remote, error = ?error, "failed to enable TCP_NODELAY");
    }
    let socket = SockRef::from(stream);
    if let Err(error) = socket.set_send_buffer_size(SOCKET_BUFFER_BYTES) {
        tracing::debug!(%remote, error = ?error, "could not enlarge the TCP send buffer");
    }
    if let Err(error) = socket.set_recv_buffer_size(SOCKET_BUFFER_BYTES) {
        tracing::debug!(%remote, error = ?error, "could not enlarge the TCP receive buffer");
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    remote: SocketAddr,
    runtime: Arc<LnaHttpRuntime>,
) -> anyhow::Result<()> {
    let mut pending = Vec::with_capacity(MAX_HEADER_BYTES);
    loop {
        let request =
            match tokio::time::timeout(IDLE_TIMEOUT, read_request(&mut stream, &mut pending)).await
            {
                Ok(result) => match result {
                    Ok(Some(request)) => request,
                    Ok(None) => return Ok(()),
                    Err(error) => {
                        write_error(
                            &mut stream,
                            400,
                            "invalid_request",
                            &error.to_string(),
                            None,
                        )
                        .await?;
                        return Ok(());
                    }
                },
                Err(_) => return Ok(()),
            };
        let keep_alive = request.keep_alive;
        let origin_allowed = request.origin.as_deref() == Some(runtime.settings.allowed_origin.as_str());
        if !origin_allowed {
            tracing::warn!(%remote, origin = ?request.origin, "rejected LNA HTTP request from an untrusted Origin");
            write_error(
                &mut stream,
                403,
                "origin_not_allowed",
                "request Origin is not allowed",
                None,
            )
            .await?;
            return Ok(());
        }

        let result = route_request(
            &mut stream,
            &mut pending,
            request,
            remote,
            &runtime,
        )
        .await;
        if let Err(error) = result {
            tracing::warn!(%remote, error = ?error, "failed to serve LNA HTTP request");
            return Err(error);
        }
        if !keep_alive {
            return Ok(());
        }
    }
}

async fn route_request(
    stream: &mut TcpStream,
    pending: &mut Vec<u8>,
    request: HttpRequest,
    remote: SocketAddr,
    runtime: &LnaHttpRuntime,
) -> anyhow::Result<()> {
    let settings = &runtime.settings;
    let origin = settings.allowed_origin.as_str();
    if request.path.starts_with(file_http::FILE_HTTP_BASE_PATH) {
        return file_http::route(
            stream,
            pending,
            request,
            remote,
            origin,
            Arc::clone(&runtime.file_requests),
            settings.files.clone(),
        )
        .await;
    }
    if request.method == "OPTIONS" {
        if request.path != PROBE_PATH && request.path != BENCHMARK_PATH {
            write_error(
                stream,
                404,
                "not_found",
                "HTTP endpoint does not exist",
                Some(origin),
            )
            .await?;
            return Ok(());
        }
        if request.content_length.unwrap_or(0) != 0 || request.transfer_encoding {
            write_error(
                stream,
                400,
                "invalid_preflight",
                "preflight request must not have a body",
                Some(origin),
            )
            .await?;
            return Ok(());
        }
        write_preflight(stream, origin, request.keep_alive).await?;
        return Ok(());
    }

    if request.path == PROBE_PATH {
        if request.method != "GET" {
            write_error(
                stream,
                405,
                "method_not_allowed",
                "probe only accepts GET",
                Some(origin),
            )
            .await?;
        } else if request.content_length.unwrap_or(0) != 0 || request.transfer_encoding {
            write_error(
                stream,
                400,
                "invalid_probe",
                "probe request must not have a body",
                Some(origin),
            )
            .await?;
        } else {
            tracing::info!(%remote, "LNA HTTP capability probe succeeded");
            write_response(
                stream,
                204,
                "text/plain",
                0,
                origin,
                request.keep_alive,
                &[],
            )
            .await?;
        }
        return Ok(());
    }

    if request.path != BENCHMARK_PATH {
        write_error(
            stream,
            404,
            "not_found",
            "HTTP endpoint does not exist",
            Some(origin),
        )
        .await?;
        return Ok(());
    }
    if request.transfer_encoding {
        write_error(
            stream,
            400,
            "chunked_not_supported",
            "Transfer-Encoding is not accepted",
            Some(origin),
        )
        .await?;
        return Ok(());
    }

    let request_bytes = match request.method.as_str() {
        "POST" => match request.content_length {
            Some(bytes) => bytes,
            None => {
                write_error(
                    stream,
                    400,
                    "content_length_required",
                    "benchmark upload requires Content-Length",
                    Some(origin),
                )
                .await?;
                return Ok(());
            }
        },
        "GET" => match parse_download_bytes(request.query.as_deref()) {
            Ok(bytes) => bytes,
            Err(error) => {
                write_error(
                    stream,
                    400,
                    "invalid_download_size",
                    &error.to_string(),
                    Some(origin),
                )
                .await?;
                return Ok(());
            }
        },
        _ => {
            write_error(
                stream,
                405,
                "method_not_allowed",
                "benchmark accepts GET or POST",
                Some(origin),
            )
            .await?;
            return Ok(());
        }
    };
    let max_request_bytes = settings.max_transfer_size.min(MAX_BENCHMARK_REQUEST_BYTES);
    if request_bytes == 0 || request_bytes > max_request_bytes {
        write_error(
            stream,
            413,
            "request_too_large",
            "benchmark request size is outside the allowed range",
            Some(origin),
        )
        .await?;
        return Ok(());
    }
    if request.method == "GET" && request.content_length.unwrap_or(0) != 0 {
        write_error(
            stream,
            400,
            "invalid_download",
            "benchmark download must not have a request body",
            Some(origin),
        )
        .await?;
        return Ok(());
    }
    let Some(ticket) = request.ticket else {
        write_error(
            stream,
            401,
            "ticket_required",
            "benchmark ticket is required",
            Some(origin),
        )
        .await?;
        return Ok(());
    };
    let permit = match Arc::clone(&runtime.active_requests).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            write_error(
                stream,
                503,
                "benchmark_busy",
                "all benchmark lanes are busy",
                Some(origin),
            )
            .await?;
            return Ok(());
        }
    };
    if !settings.authority.consume_ticket(&ticket)? {
        write_error(
            stream,
            401,
            "ticket_invalid",
            "benchmark ticket is expired or already used",
            Some(origin),
        )
        .await?;
        return Ok(());
    }
    let _permit = permit;

    tracing::info!(
        %remote,
        method = %request.method,
        bytes = request_bytes,
        keep_alive = request.keep_alive,
        "LNA HTTP benchmark request accepted"
    );

    if request.method == "POST" {
        receive_benchmark(
            stream,
            pending,
            remote,
            request_bytes,
            origin,
            request.keep_alive,
            settings.metrics_enabled,
        )
        .await
    } else {
        send_benchmark(
            stream,
            remote,
            request_bytes,
            origin,
            request.keep_alive,
            settings.metrics_enabled,
            Arc::clone(&runtime.zero_block),
        )
        .await
    }
}

async fn receive_benchmark(
    stream: &mut TcpStream,
    pending: &mut Vec<u8>,
    remote: SocketAddr,
    total_bytes: u64,
    origin: &str,
    keep_alive: bool,
    metrics_enabled: bool,
) -> anyhow::Result<()> {
    let monitor = Monitor::start("lna-browser-to-agent-memory", metrics_enabled);
    let progress = monitor.progress();
    let buffered = total_bytes.min(pending.len() as u64) as usize;
    if buffered > 0 {
        pending.drain(..buffered);
        progress.add(buffered);
    }
    let mut remaining = total_bytes - buffered as u64;
    let mut buffer = vec![0_u8; IO_BLOCK_BYTES];
    while remaining > 0 {
        let count =
            usize::try_from(remaining.min(buffer.len() as u64)).expect("read size is bounded");
        let read = tokio::time::timeout(IO_TIMEOUT, stream.read(&mut buffer[..count]))
            .await
            .context("timed out reading an LNA benchmark upload")??;
        anyhow::ensure!(read > 0, "LNA benchmark upload ended before Content-Length");
        progress.add(read);
        remaining -= read as u64;
    }
    let stats = monitor.finish().await;
    anyhow::ensure!(
        stats.bytes == total_bytes,
        "LNA upload byte count is inconsistent"
    );
    tracing::info!(
        %remote,
        bytes = stats.bytes,
        elapsed_seconds = stats.elapsed.as_secs_f64(),
        average_mbps = stats.average_mbps,
        "LNA HTTP browser-to-Agent memory request complete"
    );
    let body = format!(
        "{{\"bytes\":{},\"elapsedNanos\":{}}}",
        stats.bytes,
        u64::try_from(stats.elapsed.as_nanos()).unwrap_or(u64::MAX)
    );
    write_response(
        stream,
        200,
        "application/json",
        body.len() as u64,
        origin,
        keep_alive,
        &[],
    )
    .await?;
    stream.write_all(body.as_bytes()).await?;
    Ok(())
}

async fn send_benchmark(
    stream: &mut TcpStream,
    remote: SocketAddr,
    total_bytes: u64,
    origin: &str,
    keep_alive: bool,
    metrics_enabled: bool,
    zero_block: Arc<Vec<u8>>,
) -> anyhow::Result<()> {
    write_response(
        stream,
        200,
        "application/octet-stream",
        total_bytes,
        origin,
        keep_alive,
        &[("X-WinriseF-Bytes", total_bytes.to_string())],
    )
    .await?;
    let monitor = Monitor::start("lna-agent-to-browser-memory", metrics_enabled);
    let progress = monitor.progress();
    let mut remaining = total_bytes;
    while remaining > 0 {
        let count =
            usize::try_from(remaining.min(zero_block.len() as u64)).expect("write size is bounded");
        tokio::time::timeout(IO_TIMEOUT, stream.write_all(&zero_block[..count]))
            .await
            .context("timed out writing an LNA benchmark download")??;
        progress.add(count);
        remaining -= count as u64;
    }
    let stats = monitor.finish().await;
    tracing::info!(
        %remote,
        bytes = stats.bytes,
        elapsed_seconds = stats.elapsed.as_secs_f64(),
        average_mbps = stats.average_mbps,
        "LNA HTTP Agent-to-browser memory request complete"
    );
    Ok(())
}

#[derive(Debug)]
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) ticket: Option<[u8; 16]>,
    pub(crate) file_token: Option<[u8; 32]>,
    pub(crate) content_length: Option<u64>,
    pub(crate) transfer_encoding: bool,
    pub(crate) keep_alive: bool,
}

async fn read_request(
    stream: &mut TcpStream,
    pending: &mut Vec<u8>,
) -> anyhow::Result<Option<HttpRequest>> {
    let header_end = loop {
        if let Some(end) = find_header_end(pending) {
            break end;
        }
        anyhow::ensure!(
            pending.len() < MAX_HEADER_BYTES,
            "HTTP request headers are too large"
        );
        let mut buffer = [0_u8; 4096];
        let read = tokio::time::timeout(HEADER_TIMEOUT, stream.read(&mut buffer))
            .await
            .context("timed out reading HTTP request headers")??;
        if read == 0 {
            if pending.is_empty() {
                return Ok(None);
            }
            anyhow::bail!("HTTP connection ended inside request headers");
        }
        pending.extend_from_slice(&buffer[..read]);
    };
    let header = std::str::from_utf8(&pending[..header_end])
        .context("HTTP headers are not valid UTF-8/ASCII")?;
    let request = parse_request(header)?;
    pending.drain(..header_end + 4);
    Ok(Some(request))
}

fn parse_request(header: &str) -> anyhow::Result<HttpRequest> {
    let mut lines = header.split("\r\n");
    let request_line = lines.next().context("HTTP request line is missing")?;
    let mut parts = request_line.split(' ');
    let method = parts.next().context("HTTP method is missing")?;
    let target = parts.next().context("HTTP target is missing")?;
    let version = parts.next().context("HTTP version is missing")?;
    anyhow::ensure!(parts.next().is_none(), "HTTP request line has extra fields");
    anyhow::ensure!(version == "HTTP/1.1", "only HTTP/1.1 is supported");
    anyhow::ensure!(
        target.starts_with('/') && !target.contains('#'),
        "HTTP target must be origin-form"
    );

    let mut origin = None;
    let mut ticket = None;
    let mut file_token = None;
    let mut content_length = None;
    let mut transfer_encoding = false;
    let mut connection_close = false;
    for line in lines {
        let (name, value) = line.split_once(':').context("HTTP header is malformed")?;
        anyhow::ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "HTTP header name is invalid"
        );
        let name = name.to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "origin" => set_once(&mut origin, value.to_owned(), "Origin")?,
            TICKET_HEADER => set_once(&mut ticket, parse_hex_token(value, "benchmark ticket")?, "ticket")?,
            FILE_TOKEN_HEADER => set_once(
                &mut file_token,
                parse_hex_token(value, "file transfer token")?,
                "file transfer token",
            )?,
            "content-length" => set_once(
                &mut content_length,
                value.parse::<u64>().context("Content-Length is invalid")?,
                "Content-Length",
            )?,
            "transfer-encoding" => transfer_encoding = true,
            "connection" if value.eq_ignore_ascii_case("close") => connection_close = true,
            "expect" => anyhow::bail!("Expect requests are not supported"),
            _ => {}
        }
    }
    let (path, query) = target
        .split_once('?')
        .map_or((target, None), |(path, query)| {
            (path, Some(query.to_owned()))
        });
    Ok(HttpRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        query,
        origin,
        ticket,
        file_token,
        content_length,
        transfer_encoding,
        keep_alive: !connection_close,
    })
}

fn set_once<T>(slot: &mut Option<T>, value: T, label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(slot.is_none(), "duplicate {label} header");
    *slot = Some(value);
    Ok(())
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_hex_token<const N: usize>(value: &str, label: &str) -> anyhow::Result<[u8; N]> {
    anyhow::ensure!(
        value.len() == N * 2,
        "{label} must contain {} hexadecimal characters",
        N * 2
    );
    let mut token = [0_u8; N];
    for (index, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .with_context(|| format!("{label} is not hexadecimal"))?;
    }
    Ok(token)
}

fn parse_download_bytes(query: Option<&str>) -> anyhow::Result<u64> {
    let query = query.context("benchmark download requires a bytes query parameter")?;
    let mut bytes = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        anyhow::ensure!(
            key == "bytes",
            "benchmark download query contains an unknown parameter"
        );
        set_once(
            &mut bytes,
            value
                .parse::<u64>()
                .context("benchmark download byte count is invalid")?,
            "bytes query",
        )?;
    }
    bytes.context("benchmark download requires a bytes query parameter")
}

pub(crate) async fn write_preflight(
    stream: &mut TcpStream,
    origin: &str,
    keep_alive: bool,
) -> anyhow::Result<()> {
    let headers = [
        (
            "Access-Control-Allow-Methods",
            "GET, POST, OPTIONS".to_owned(),
        ),
        (
            "Access-Control-Allow-Headers",
            "Content-Type, X-WinriseF-Ticket, X-WinriseF-Transfer-Token".to_owned(),
        ),
        ("Access-Control-Max-Age", "600".to_owned()),
    ];
    write_response(stream, 204, "text/plain", 0, origin, keep_alive, &headers).await
}

pub(crate) async fn write_error(
    stream: &mut TcpStream,
    status: u16,
    code: &str,
    message: &str,
    origin: Option<&str>,
) -> anyhow::Result<()> {
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    let body = format!("{{\"error\":{{\"code\":\"{code}\",\"message\":\"{escaped}\"}}}}");
    write_response(
        stream,
        status,
        "application/json",
        body.len() as u64,
        origin.unwrap_or("null"),
        false,
        &[],
    )
    .await?;
    stream.write_all(body.as_bytes()).await?;
    Ok(())
}

pub(crate) async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    content_length: u64,
    origin: &str,
    keep_alive: bool,
    extra_headers: &[(&str, String)],
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let mut header = format!(
        "HTTP/1.1 {status} {reason}\r\nServer: WinriseF-Agent\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nConnection: {connection}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Private-Network: true\r\nAccess-Control-Expose-Headers: X-WinriseF-Bytes\r\nVary: Origin, Access-Control-Request-Private-Network\r\n"
    );
    for (name, value) in extra_headers {
        header.push_str(name);
        header.push_str(": ");
        header.push_str(value);
        header.push_str("\r\n");
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{BENCHMARK_PATH, parse_download_bytes, parse_request};

    #[test]
    fn parses_bounded_benchmark_upload() {
        let request = parse_request(&format!(
            "POST {BENCHMARK_PATH} HTTP/1.1\r\nOrigin: https://e.winrisef.top\r\nContent-Length: 1024\r\nX-WinriseF-Ticket: 000102030405060708090a0b0c0d0e0f"
        ))
        .unwrap();
        assert_eq!(request.content_length, Some(1024));
        assert_eq!(
            request.ticket.unwrap(),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
    }

    #[test]
    fn rejects_duplicate_content_length() {
        let request =
            format!("POST {BENCHMARK_PATH} HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2");
        assert!(parse_request(&request).is_err());
    }

    #[test]
    fn accepts_only_one_download_size() {
        assert_eq!(
            parse_download_bytes(Some("bytes=31457280")).unwrap(),
            31_457_280
        );
        assert!(parse_download_bytes(Some("bytes=1&bytes=2")).is_err());
        assert!(parse_download_bytes(Some("size=1")).is_err());
    }
}
