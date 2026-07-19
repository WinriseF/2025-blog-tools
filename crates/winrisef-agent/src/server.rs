use std::{net::IpAddr, sync::Arc};

use anyhow::Context;
use rustls::version::TLS13;
use tokio::sync::{Semaphore, watch};
use web_transport_quinn::http::StatusCode;

use crate::{
    auth::TicketAuthority,
    bridge, certificate,
    cli::ServeArgs,
    lna_http::{self, LnaHttpSettings},
    transfer::{self, TransferAuthenticator, TransferSettings},
    tuning,
};

pub const BRIDGE_PATH: &str = "/winrisef/bridge/v1";
pub const BENCHMARK_PATH: &str = "/winrisef/benchmark/v3";

pub struct LaunchedServerSettings {
    pub listen: std::net::SocketAddr,
    pub allowed_origin: String,
    pub certificate_ips: Vec<IpAddr>,
    pub authority: TicketAuthority,
    pub max_transfer_size: u64,
    pub max_sessions: usize,
    pub metrics_enabled: bool,
}

pub struct ReadyInfo {
    pub port: u16,
    pub certificate_sha256: String,
}

enum ServerMode {
    Manual {
        path: String,
    },
    Launched {
        authority: TicketAuthority,
        shutdown: watch::Sender<bool>,
    },
}

struct ServerSettings {
    allowed_origins: Vec<String>,
    mode: ServerMode,
    transfer: TransferSettings,
}

#[derive(Clone, Copy, Debug)]
enum Route {
    Bridge,
    Transfer,
}

#[derive(Debug)]
enum RouteRejection {
    MissingOrigin,
    InvalidOrigin,
    OriginNotAllowed,
    PathNotAllowed,
}

pub async fn run_manual(args: ServeArgs) -> anyhow::Result<()> {
    validate_origin_list(&args.allowed_origins)?;
    anyhow::ensure!(
        args.path.starts_with('/'),
        "WebTransport path must start with '/'"
    );
    let settings = ServerSettings {
        allowed_origins: args.allowed_origins,
        mode: ServerMode::Manual { path: args.path },
        transfer: TransferSettings {
            authenticator: TransferAuthenticator::Fixed(args.token),
            max_transfer_size: args.max_transfer_size,
            metrics_enabled: args.metrics,
        },
    };
    run_server(
        args.listen,
        Vec::new(),
        args.max_sessions,
        settings,
        None,
        None,
        |_| Ok(()),
    )
    .await
}

pub async fn run_launched(
    args: LaunchedServerSettings,
    on_ready: impl FnOnce(&ReadyInfo) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    validate_origin_list(std::slice::from_ref(&args.allowed_origin))?;
    let authority = args.authority;
    let (shutdown, shutdown_rx) = watch::channel(false);
    let launch_timeout_authority = authority.clone();
    let launch_timeout_shutdown = shutdown.clone();
    tokio::spawn(async move {
        tracing::debug!(
            timeout_seconds = 125,
            "launch authentication timeout task started"
        );
        tokio::time::sleep(std::time::Duration::from_secs(125)).await;
        if !launch_timeout_authority.launch_is_consumed() {
            tracing::warn!("launch token was not consumed before timeout; stopping Agent");
            let _ = launch_timeout_shutdown.send(true);
        } else {
            tracing::trace!("launch timeout elapsed after Bridge authentication; no action needed");
        }
    });
    let lna_settings = LnaHttpSettings {
        allowed_origin: args.allowed_origin.clone(),
        authority: authority.clone(),
        max_transfer_size: args.max_transfer_size,
        max_sessions: args.max_sessions,
        metrics_enabled: args.metrics_enabled,
    };
    let settings = ServerSettings {
        allowed_origins: vec![args.allowed_origin],
        mode: ServerMode::Launched {
            authority: authority.clone(),
            shutdown,
        },
        transfer: TransferSettings {
            authenticator: TransferAuthenticator::Tickets(authority),
            max_transfer_size: args.max_transfer_size,
            metrics_enabled: args.metrics_enabled,
        },
    };
    run_server(
        args.listen,
        args.certificate_ips,
        args.max_sessions,
        settings,
        Some(lna_settings),
        Some(shutdown_rx),
        on_ready,
    )
    .await
}

async fn run_server(
    listen: std::net::SocketAddr,
    certificate_ips: Vec<IpAddr>,
    max_sessions: usize,
    settings: ServerSettings,
    lna_settings: Option<LnaHttpSettings>,
    mut shutdown: Option<watch::Receiver<bool>>,
    on_ready: impl FnOnce(&ReadyInfo) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    tracing::debug!(
        %listen,
        certificate_ip_count = certificate_ips.len(),
        max_sessions,
        max_transfer_size = settings.transfer.max_transfer_size,
        metrics_enabled = settings.transfer.metrics_enabled,
        "building QUIC/WebTransport server"
    );
    anyhow::ensure!(
        settings.transfer.max_transfer_size > 0,
        "maximum transfer size must be positive"
    );
    let identity = certificate::generate_identity(&certificate_ips)?;
    let certificate_sha256 = certificate::format_sha256(&identity.sha256);
    let mut tls = rustls::ServerConfig::builder_with_provider(
        web_transport_quinn::crypto::default_provider(),
    )
    .with_protocol_versions(&[&TLS13])?
    .with_no_client_auth()
    .with_single_cert(identity.certificate_chain, identity.private_key)
    .context("failed to configure the WebTransport certificate")?;
    tls.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];
    tracing::debug!(
        tls_version = "1.3",
        alpn = web_transport_quinn::ALPN,
        certificate_sha256,
        "configured WebTransport TLS identity"
    );
    let crypto: quinn::crypto::rustls::QuicServerConfig = tls
        .try_into()
        .context("failed to create the QUIC TLS configuration")?;
    let endpoint = tuning::endpoint(listen, crypto)?;
    let local_addr = endpoint.local_addr()?;
    let ready = ReadyInfo {
        port: local_addr.port(),
        certificate_sha256,
    };
    let lna_task = if let Some(lna_settings) = lna_settings {
        let lna_shutdown = shutdown
            .as_ref()
            .context("LNA HTTP endpoint requires a shutdown channel")?
            .clone();
        Some(lna_http::start(local_addr, lna_settings, lna_shutdown).await?)
    } else {
        None
    };
    on_ready(&ready)?;
    tracing::info!(
        port = ready.port,
        "headless LNA HTTP/TCP and WebTransport accelerator ready"
    );

    let settings = Arc::new(settings);
    let transfer_sessions = Arc::new(Semaphore::new(max_sessions));
    let bridge_sessions = Arc::new(Semaphore::new(1));
    tracing::info!(
        listen = %local_addr,
        max_transfer_sessions = max_sessions,
        allowed_origins = ?settings.allowed_origins,
        "QUIC endpoint is accepting connections"
    );
    loop {
        let incoming = if let Some(shutdown) = shutdown.as_mut() {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!("Agent shutdown signal received by QUIC accept loop");
                        break;
                    }
                    continue;
                }
                incoming = endpoint.accept() => incoming,
            }
        } else {
            endpoint.accept().await
        };
        let Some(incoming) = incoming else {
            tracing::warn!("QUIC endpoint stopped accepting connections");
            break;
        };
        let remote = incoming.remote_address();
        let local_ip = incoming.local_ip();
        let address_validated = incoming.remote_address_validated();
        tracing::info!(
            %remote,
            ?local_ip,
            address_validated,
            "received an incoming QUIC connection"
        );
        let settings = Arc::clone(&settings);
        let transfer_sessions = Arc::clone(&transfer_sessions);
        let bridge_sessions = Arc::clone(&bridge_sessions);
        tokio::spawn(async move {
            let result = handle_incoming(
                incoming,
                settings,
                transfer_sessions,
                bridge_sessions,
                remote,
            )
            .await;
            if let Err(error) = result {
                tracing::error!(%remote, error = ?error, "QUIC/WebTransport connection failed");
            }
        });
    }
    endpoint.close(0_u32.into(), b"Agent shutdown");
    endpoint.wait_idle().await;
    if let ServerMode::Launched { shutdown, .. } = &settings.mode {
        let _ = shutdown.send(true);
    }
    if let Some(task) = lna_task {
        task.await.context("LNA HTTP/TCP task panicked")??;
    }
    tracing::info!("QUIC endpoint shut down cleanly");
    Ok(())
}

async fn handle_incoming(
    incoming: quinn::Incoming,
    settings: Arc<ServerSettings>,
    transfer_sessions: Arc<Semaphore>,
    bridge_sessions: Arc<Semaphore>,
    remote: std::net::SocketAddr,
) -> anyhow::Result<()> {
    let connection = incoming
        .await
        .context("QUIC TLS handshake failed before HTTP/3 negotiation")?;
    let connection_id = connection.stable_id();
    tracing::info!(
        %remote,
        connection_id,
        local_ip = ?connection.local_ip(),
        "QUIC TLS handshake completed"
    );
    let request = web_transport_quinn::Request::accept(connection)
        .await
        .context("HTTP/3 settings or WebTransport CONNECT handshake failed")?;
    let origin = request
        .headers
        .get("origin")
        .and_then(|value| value.to_str().ok());
    tracing::info!(
        %remote,
        connection_id,
        url = %request.url,
        ?origin,
        protocols = ?request.protocols,
        header_names = ?request.headers.keys().collect::<Vec<_>>(),
        "received WebTransport CONNECT request"
    );
    let route = match request_route(&request, &settings) {
        Ok(route) => route,
        Err(reason) => {
            tracing::warn!(
                %remote,
                connection_id,
                ?reason,
                url = %request.url,
                ?origin,
                allowed_origins = ?settings.allowed_origins,
                "rejecting WebTransport CONNECT request"
            );
            request
                .reject(StatusCode::FORBIDDEN)
                .await
                .context("failed to reject an unauthorized WebTransport request")?;
            return Ok(());
        }
    };
    let permits = match route {
        Route::Bridge => bridge_sessions,
        Route::Transfer => transfer_sessions,
    };
    let Ok(permit) = permits.try_acquire_owned() else {
        tracing::warn!(%remote, connection_id, ?route, "rejecting WebTransport request while route is busy");
        request
            .reject(StatusCode::SERVICE_UNAVAILABLE)
            .await
            .context("failed to reject a WebTransport request while busy")?;
        return Ok(());
    };
    tracing::info!(%remote, connection_id, ?route, "accepting WebTransport session");
    let session = request
        .ok()
        .await
        .context("failed to send the successful WebTransport CONNECT response")?;
    tracing::info!(%remote, connection_id, ?route, "WebTransport session accepted");
    let _permit = permit;
    match route {
        Route::Bridge => {
            let ServerMode::Launched {
                authority,
                shutdown,
            } = &settings.mode
            else {
                anyhow::bail!("Bridge route is unavailable");
            };
            let result = bridge::run_session(session, authority.clone()).await;
            tracing::info!(
                %remote,
                connection_id,
                launch_consumed = authority.launch_is_consumed(),
                success = result.is_ok(),
                "Bridge session ended"
            );
            if authority.launch_is_consumed() {
                let _ = shutdown.send(true);
            }
            result
        }
        Route::Transfer => {
            let stats = transfer::run_session(session, settings.transfer.clone()).await?;
            tracing::info!(
                %remote,
                connection_id,
                bytes = stats.bytes,
                elapsed_seconds = stats.elapsed.as_secs_f64(),
                average_mbps = stats.average_mbps,
                "memory transfer complete"
            );
            Ok(())
        }
    }
}

fn request_route(
    request: &web_transport_quinn::Request,
    settings: &ServerSettings,
) -> Result<Route, RouteRejection> {
    let origin = request
        .headers
        .get("origin")
        .ok_or(RouteRejection::MissingOrigin)?
        .to_str()
        .map_err(|_| RouteRejection::InvalidOrigin)?;
    if !settings
        .allowed_origins
        .iter()
        .any(|allowed| allowed == origin)
    {
        return Err(RouteRejection::OriginNotAllowed);
    }
    match &settings.mode {
        ServerMode::Manual { path } if request.url.path() == path => Ok(Route::Transfer),
        ServerMode::Launched { .. } if request.url.path() == BRIDGE_PATH => Ok(Route::Bridge),
        ServerMode::Launched { .. } if request.url.path() == BENCHMARK_PATH => Ok(Route::Transfer),
        _ => Err(RouteRejection::PathNotAllowed),
    }
}

fn validate_origin_list(origins: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !origins.is_empty(),
        "at least one browser Origin is required"
    );
    anyhow::ensure!(
        origins.iter().all(|origin| valid_origin(origin)),
        "origins must use HTTPS; loopback HTTP is allowed only for development"
    );
    Ok(())
}

fn valid_origin(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    if url.origin().ascii_serialization() != origin {
        return false;
    }
    url.scheme() == "https"
        || (url.scheme() == "http"
            && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1")))
}
