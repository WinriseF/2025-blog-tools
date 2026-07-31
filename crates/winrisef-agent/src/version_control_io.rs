use std::sync::Arc;

use anyhow::Context;
use serde::Serialize;
use web_transport_quinn::{RecvStream, SendStream, Session};
use winrisef_version_control::PreviewContent;

use crate::version_control_bridge::BridgeInput;

const MAX_FRAME_BYTES: usize = 64 * 1024;

pub(super) async fn write_preview_stream(
    session: &Session,
    request_id: u32,
    content: Arc<PreviewContent>,
) -> anyhow::Result<()> {
    let original = content.original.as_bytes();
    let modified = content.modified.as_bytes();
    let metadata = serde_json::to_vec(&serde_json::json!({
        "requestId": request_id,
        "originalBytes": original.len(),
        "modifiedBytes": modified.len(),
        "contentType": "text/plain; charset=utf-8",
        "perspective": content.perspective
    }))?;
    let mut send = session
        .open_uni()
        .await
        .context("failed to open preview stream")?;
    send.write_all(&(metadata.len() as u32).to_be_bytes())
        .await?;
    send.write_all(&metadata).await?;
    send.write_all(original).await?;
    send.write_all(modified).await?;
    send.finish().context("failed to finish preview stream")?;
    Ok(())
}

pub(super) async fn read_frame(recv: &mut RecvStream) -> anyhow::Result<Option<BridgeInput>> {
    let mut prefix = [0_u8; 4];
    let Some(first) = recv.read(&mut prefix[..1]).await? else {
        return Ok(None);
    };
    anyhow::ensure!(first == 1, "invalid version-control frame prefix");
    recv.read_exact(&mut prefix[1..]).await?;
    let len = u32::from_be_bytes(prefix) as usize;
    anyhow::ensure!(
        len > 0 && len <= MAX_FRAME_BYTES,
        "invalid version-control frame length"
    );
    let mut bytes = vec![0_u8; len];
    recv.read_exact(&mut bytes).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(super) async fn write_frame<T: Serialize>(
    send: &mut SendStream,
    value: &T,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_FRAME_BYTES,
        "version-control response is too large"
    );
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}
