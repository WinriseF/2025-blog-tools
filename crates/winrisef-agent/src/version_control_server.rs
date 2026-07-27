use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
use rustls::version::TLS13;
use web_transport_quinn::http::StatusCode;

use crate::{
    auth::TicketAuthority,
    certificate,
    server::ReadyInfo,
    tuning,
    version_control_bridge::{self, VersionControlManager},
};

pub const VERSION_CONTROL_PATH: &str = "/winrisef/version-control/v1";

pub async fn run(
    allowed_origin: String,
    authority: TicketAuthority,
    on_ready: impl FnOnce(&ReadyInfo) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let identity = certificate::generate_identity(&[])?;
    let certificate_sha256 = certificate::format_sha256(&identity.sha256);
    let mut tls = rustls::ServerConfig::builder_with_provider(
        web_transport_quinn::crypto::default_provider(),
    )
    .with_protocol_versions(&[&TLS13])?
    .with_no_client_auth()
    .with_single_cert(identity.certificate_chain, identity.private_key)
    .context("failed to configure the version-control TLS identity")?;
    tls.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];
    let crypto: quinn::crypto::rustls::QuicServerConfig = tls
        .try_into()
        .context("failed to create the version-control QUIC configuration")?;
    let endpoint = tuning::endpoint("127.0.0.1:0".parse::<SocketAddr>()?, crypto)?;
    let local_addr = endpoint.local_addr()?;
    on_ready(&ReadyInfo {
        port: local_addr.port(),
        certificate_sha256,
    })?;
    tracing::info!(
        port = local_addr.port(),
        "read-only version-control Agent ready"
    );

    let incoming = tokio::time::timeout(Duration::from_secs(125), endpoint.accept())
        .await
        .context("version-control launch timed out")?
        .context("version-control endpoint stopped accepting")?;
    let remote = incoming.remote_address();
    anyhow::ensure!(
        remote.ip().is_loopback(),
        "version-control connection is not loopback"
    );
    let connection = tokio::time::timeout(Duration::from_secs(8), incoming)
        .await
        .context("version-control QUIC handshake timed out")?
        .context("version-control QUIC handshake failed")?;
    let request = tokio::time::timeout(
        Duration::from_secs(8),
        web_transport_quinn::Request::accept(connection),
    )
    .await
    .context("version-control WebTransport handshake timed out")?
    .context("version-control WebTransport handshake failed")?;
    let origin = request
        .headers
        .get("origin")
        .and_then(|value| value.to_str().ok());
    if origin != Some(allowed_origin.as_str()) || request.url.path() != VERSION_CONTROL_PATH {
        request
            .reject(StatusCode::FORBIDDEN)
            .await
            .context("failed to reject a version-control request")?;
        anyhow::bail!("version-control Origin or path was rejected");
    }
    let session = request
        .ok()
        .await
        .context("failed to accept the version-control WebTransport session")?;
    let manager = Arc::new(VersionControlManager::new());
    let result = version_control_bridge::run_session(session, authority, manager).await;
    endpoint.close(0_u32.into(), b"version-control bridge closed");
    endpoint.wait_idle().await;
    result
}
