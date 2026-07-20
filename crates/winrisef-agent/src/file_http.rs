use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Semaphore, mpsc},
};

use crate::{
    file_transfer::{
        FILE_HTTP_PARALLELISM, FILE_IO_BLOCK_BYTES, FileTransferManager,
        NativeFileDirection, SegmentLease,
    },
    lna_http::{HttpRequest, write_error, write_preflight, write_response},
};

pub const FILE_HTTP_BASE_PATH: &str = "/winrisef/file/v1";
const FILE_PROBE_PATH: &str = "/winrisef/file/v1/probe";
const FILE_TRANSFER_PREFIX: &str = "/winrisef/file/v1/transfers/";
const IO_TIMEOUT: Duration = Duration::from_secs(120);

pub async fn route(
    stream: &mut TcpStream,
    pending: &mut Vec<u8>,
    request: HttpRequest,
    remote: SocketAddr,
    origin: &str,
    active_requests: Arc<Semaphore>,
    files: FileTransferManager,
) -> anyhow::Result<()> {
    if request.method == "OPTIONS" {
        if request.content_length.unwrap_or(0) != 0 || request.transfer_encoding {
            return write_error(
                stream,
                400,
                "invalid_preflight",
                "preflight request must not have a body",
                Some(origin),
            )
            .await;
        }
        return write_preflight(stream, origin, request.keep_alive).await;
    }
    if request.path == FILE_PROBE_PATH {
        if request.method != "GET" || request.content_length.unwrap_or(0) != 0 {
            return write_error(
                stream,
                405,
                "invalid_probe",
                "file probe only accepts an empty GET request",
                Some(origin),
            )
            .await;
        }
        tracing::info!(%remote, "Native File V1 LNA probe succeeded");
        return write_response(
            stream,
            204,
            "text/plain",
            0,
            origin,
            request.keep_alive,
            &[],
        )
        .await;
    }
    if request.transfer_encoding {
        return write_error(
            stream,
            400,
            "chunked_not_supported",
            "native file transfers require a fixed Content-Length",
            Some(origin),
        )
        .await;
    }
    let Some((transfer_id, action)) = parse_path(&request.path) else {
        return write_error(
            stream,
            404,
            "not_found",
            "file endpoint does not exist",
            Some(origin),
        )
        .await;
    };
    let Some(token) = request.file_token else {
        return write_error(
            stream,
            401,
            "transfer_token_required",
            "native file transfer token is required",
            Some(origin),
        )
        .await;
    };
    if action == "complete" {
        if request.method != "POST" || request.content_length.unwrap_or(0) != 0 {
            return write_error(
                stream,
                405,
                "invalid_complete",
                "file completion requires an empty POST request",
                Some(origin),
            )
            .await;
        }
        let _completion_permit = match Arc::clone(&active_requests)
            .try_acquire_many_owned(FILE_HTTP_PARALLELISM as u32)
        {
            Ok(permit) => permit,
            Err(_) => {
                return write_error(
                    stream,
                    409,
                    "transfer_still_active",
                    "native file requests are still active",
                    Some(origin),
                )
                .await;
            }
        };
        return match files.complete_receive(transfer_id, &token) {
            Ok(()) => {
                tracing::info!(%remote, transfer_id, "Native File V1 receive completed");
                write_response(
                    stream,
                    204,
                    "text/plain",
                    0,
                    origin,
                    request.keep_alive,
                    &[],
                )
                .await
            }
            Err(error) => {
                write_error(
                    stream,
                    409,
                    "transfer_incomplete",
                    &error.to_string(),
                    Some(origin),
                )
                .await
            }
        };
    }
    if action != "segments" {
        return write_error(
            stream,
            404,
            "not_found",
            "file endpoint does not exist",
            Some(origin),
        )
        .await;
    }
    let Ok(permit) = active_requests.try_acquire_owned() else {
        return write_error(
            stream,
            503,
            "transfer_busy",
            "all native file lanes are busy",
            Some(origin),
        )
        .await;
    };
    let (offset, bytes) = match request.method.as_str() {
        "GET" => match parse_query(request.query.as_deref(), true) {
            _ if request.content_length.unwrap_or(0) != 0 => {
                return write_error(
                    stream,
                    400,
                    "invalid_download",
                    "file download must not have a request body",
                    Some(origin),
                )
                .await;
            }
            Ok(value) => value,
            Err(error) => {
                return write_error(
                    stream,
                    400,
                    "invalid_segment",
                    &error.to_string(),
                    Some(origin),
                )
                .await;
            }
        },
        "POST" => {
            let (offset, _) = match parse_query(request.query.as_deref(), false) {
                Ok(value) => value,
                Err(error) => {
                    return write_error(
                        stream,
                        400,
                        "invalid_segment",
                        &error.to_string(),
                        Some(origin),
                    )
                    .await;
                }
            };
            let Some(bytes) = request.content_length else {
                return write_error(
                    stream,
                    400,
                    "content_length_required",
                    "file upload requires Content-Length",
                    Some(origin),
                )
                .await;
            };
            (offset, bytes)
        }
        _ => {
            return write_error(
                stream,
                405,
                "method_not_allowed",
                "file segment accepts GET or POST",
                Some(origin),
            )
            .await;
        }
    };
    let direction = if request.method == "GET" {
        NativeFileDirection::AgentToBrowser
    } else {
        NativeFileDirection::BrowserToAgent
    };
    let lease = match files.begin_lna_segment(transfer_id, &token, direction, offset, bytes) {
        Ok(lease) => lease,
        Err(error) => {
            return write_error(
                stream,
                401,
                "transfer_rejected",
                &error.to_string(),
                Some(origin),
            )
            .await;
        }
    };
    let result = if request.method == "GET" {
        send_segment(stream, lease, origin, request.keep_alive).await
    } else {
        receive_segment(stream, pending, lease).await
    };
    match result {
        Ok(lease) => {
            files.commit_segment(lease)?;
            drop(permit);
            if request.method == "POST" {
                let body = format!(r#"{{"offset":{offset},"bytes":{bytes}}}"#);
                write_response(
                    stream,
                    200,
                    "application/json",
                    body.len() as u64,
                    origin,
                    request.keep_alive,
                    &[],
                )
                .await?;
                stream.write_all(body.as_bytes()).await?;
            }
            Ok(())
        }
        Err(error) => {
            files.fail_transfer(transfer_id, "native file segment failed");
            Err(error)
        }
    }
}

async fn send_segment(
    stream: &mut TcpStream,
    lease: SegmentLease,
    origin: &str,
    keep_alive: bool,
) -> anyhow::Result<SegmentLease> {
    write_response(
        stream,
        200,
        "application/octet-stream",
        lease.len(),
        origin,
        keep_alive,
        &[("X-WinriseF-Offset", lease.offset().to_string())],
    )
    .await?;
    let (payload_tx, mut payload_rx) = mpsc::channel::<Vec<u8>>(2);
    let (recycle_tx, mut recycle_rx) = mpsc::channel::<Vec<u8>>(2);
    recycle_tx.send(vec![0_u8; FILE_IO_BLOCK_BYTES]).await?;
    recycle_tx.send(vec![0_u8; FILE_IO_BLOCK_BYTES]).await?;
    let producer = tokio::task::spawn_blocking(move || -> anyhow::Result<SegmentLease> {
        let mut position = lease.offset();
        let end = lease.offset().saturating_add(lease.len());
        while position < end {
            let mut buffer = recycle_rx
                .blocking_recv()
                .context("file read buffer recycler closed")?;
            let count = usize::try_from((end - position).min(FILE_IO_BLOCK_BYTES as u64))
                .expect("file block is bounded");
            read_exact_at(&lease, &mut buffer[..count], position)?;
            lease.touch()?;
            buffer.truncate(count);
            payload_tx
                .blocking_send(buffer)
                .map_err(|_| anyhow::anyhow!("file download consumer closed"))?;
            position += count as u64;
        }
        Ok(lease)
    });
    while let Some(mut buffer) = payload_rx.recv().await {
        tokio::time::timeout(IO_TIMEOUT, stream.write_all(&buffer))
            .await
            .context("timed out writing a native file segment")??;
        buffer.resize(FILE_IO_BLOCK_BYTES, 0);
        recycle_tx
            .send(buffer)
            .await
            .map_err(|_| anyhow::anyhow!("file read buffer recycler closed"))?;
    }
    producer
        .await
        .context("native file read worker panicked")?
}

async fn receive_segment(
    stream: &mut TcpStream,
    pending: &mut Vec<u8>,
    lease: SegmentLease,
) -> anyhow::Result<SegmentLease> {
    let total_len = lease.len();
    let (payload_tx, mut payload_rx) = mpsc::channel::<Vec<u8>>(2);
    let (recycle_tx, mut recycle_rx) = mpsc::channel::<Vec<u8>>(2);
    recycle_tx.send(vec![0_u8; FILE_IO_BLOCK_BYTES]).await?;
    recycle_tx.send(vec![0_u8; FILE_IO_BLOCK_BYTES]).await?;
    let writer = tokio::task::spawn_blocking(move || -> anyhow::Result<SegmentLease> {
        let mut position = lease.offset();
        while let Some(buffer) = payload_rx.blocking_recv() {
            write_all_at(&lease, &buffer, position)?;
            lease.touch()?;
            position += buffer.len() as u64;
            let mut buffer = buffer;
            buffer.resize(FILE_IO_BLOCK_BYTES, 0);
            recycle_tx
                .blocking_send(buffer)
                .map_err(|_| anyhow::anyhow!("file upload buffer recycler closed"))?;
        }
        anyhow::ensure!(position == lease.offset() + lease.len(), "uploaded segment length is inconsistent");
        Ok(lease)
    });
    let mut remaining = total_len;
    while remaining > 0 {
        let mut buffer = recycle_rx
            .recv()
            .await
            .context("file upload buffer recycler closed")?;
        let count = usize::try_from(remaining.min(FILE_IO_BLOCK_BYTES as u64))
            .expect("file block is bounded");
        buffer.resize(count, 0);

        let buffered = count.min(pending.len());
        if buffered > 0 {
            buffer[..buffered].copy_from_slice(&pending[..buffered]);
            pending.drain(..buffered);
        }
        if buffered < count {
            tokio::time::timeout(
                IO_TIMEOUT,
                stream.read_exact(&mut buffer[buffered..count]),
            )
            .await
            .context("timed out reading a native file segment")??;
        }
        payload_tx
            .send(buffer)
            .await
            .map_err(|_| anyhow::anyhow!("native file write worker closed"))?;
        remaining -= count as u64;
    }
    drop(payload_tx);
    let lease = writer
        .await
        .context("native file write worker panicked")??;
    Ok(lease)
}

fn read_exact_at(lease: &SegmentLease, mut buffer: &mut [u8], mut offset: u64) -> anyhow::Result<()> {
    while !buffer.is_empty() {
        let read = lease.read_at(buffer, offset).context("failed to read the selected file")?;
        anyhow::ensure!(read > 0, "selected file ended during transfer");
        offset += read as u64;
        buffer = &mut buffer[read..];
    }
    Ok(())
}

fn write_all_at(lease: &SegmentLease, mut buffer: &[u8], mut offset: u64) -> anyhow::Result<()> {
    while !buffer.is_empty() {
        let written = lease
            .write_at(buffer, offset)
            .context("failed to write the destination file")?;
        anyhow::ensure!(written > 0, "destination file accepted an empty write");
        offset += written as u64;
        buffer = &buffer[written..];
    }
    Ok(())
}

fn parse_path(path: &str) -> Option<(&str, &str)> {
    let suffix = path.strip_prefix(FILE_TRANSFER_PREFIX)?;
    let (transfer_id, action) = suffix.split_once('/')?;
    if transfer_id.len() != 32 || !transfer_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some((transfer_id, action))
}

fn parse_query(query: Option<&str>, require_bytes: bool) -> anyhow::Result<(u64, u64)> {
    let query = query.context("file segment query is missing")?;
    let mut offset = None;
    let mut bytes = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "offset" if offset.is_none() => offset = Some(value.parse::<u64>().context("offset is invalid")?),
            "bytes" if bytes.is_none() => bytes = Some(value.parse::<u64>().context("bytes is invalid")?),
            _ => anyhow::bail!("file segment query contains an unknown or duplicate parameter"),
        }
    }
    let offset = offset.context("file segment offset is missing")?;
    if require_bytes {
        Ok((offset, bytes.context("file segment byte count is missing")?))
    } else {
        anyhow::ensure!(bytes.is_none(), "upload segment must use Content-Length");
        Ok((offset, 0))
    }
}
