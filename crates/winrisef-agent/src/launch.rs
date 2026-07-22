use std::{
    net::{IpAddr, Ipv4Addr},
    process::Command,
    time::Duration,
};

use anyhow::Context;
use url::Url;

use crate::{
    auth::{TicketAuthority, now_ms, random_token},
    certificate,
    cli::LaunchArgs,
    file_http::FILE_HTTP_BASE_PATH,
    file_webtransport::FILE_WEBTRANSPORT_PATH,
    firewall,
    lna_http::LNA_HTTP_BASE_PATH,
    network_endpoints::{self, NetworkEndpoints},
    server::{self, BENCHMARK_PATH, BRIDGE_PATH, LaunchedServerSettings, ReadyInfo},
};

const LAUNCH_TTL: Duration = Duration::from_secs(120);
const PRODUCTION_WEB_ORIGINS: [&str; 1] = ["https://e.winrisef.top"];

struct Activation {
    return_url: Url,
    nonce: String,
    allowed_origin: String,
}

pub async fn run(args: LaunchArgs) -> anyhow::Result<()> {
    tracing::info!(
        listen = %args.listen,
        additional_trusted_origin_count = args.trusted_origins.len(),
        max_sessions = args.max_sessions,
        max_transfer_size = args.max_transfer_size,
        metrics_enabled = args.metrics,
        "processing browser protocol activation"
    );
    let activation = parse_activation(&args.uri, &args.trusted_origins)?;
    let network_endpoints = network_endpoints::discover();
    if network_endpoints.has_public_ipv6()
        && let Err(error) = firewall::ensure_inbound_rules(args.listen.port())
    {
        tracing::warn!(error = ?error, "public IPv6 firewall authorization was not completed; WebRTC fallback remains available");
    }
    tracing::info!(
        allowed_origin = %activation.allowed_origin,
        return_scheme = activation.return_url.scheme(),
        return_host = ?activation.return_url.host_str(),
        return_path = activation.return_url.path(),
        private_http_endpoint_count = network_endpoints.http_ips.len(),
        webtransport_endpoint_count = network_endpoints.webtransport_ips.len(),
        network_epoch = network_endpoints.network_epoch,
        "browser activation validated"
    );
    let launch_token = random_token()?;
    let launch_expires_at_ms = now_ms()?.saturating_add(LAUNCH_TTL.as_millis() as u64);
    tracing::debug!(
        launch_expires_at_ms,
        launch_ttl_seconds = LAUNCH_TTL.as_secs(),
        "created one-time launch authority"
    );
    let authority = TicketAuthority::new(launch_token, launch_expires_at_ms);
    let callback_endpoints = network_endpoints.clone();
    let return_url = activation.return_url;
    let nonce = activation.nonce;
    server::run_launched(
        LaunchedServerSettings {
            listen: args.listen,
            allowed_origin: activation.allowed_origin,
            certificate_ips: Vec::new(),
            authority,
            max_transfer_size: args.max_transfer_size,
            max_sessions: args.max_sessions,
            metrics_enabled: args.metrics,
        },
        move |ready| {
            tracing::info!(
                port = ready.port,
                certificate_sha256 = %ready.certificate_sha256,
                callback_http_endpoint_count = callback_endpoints.http_ips.len(),
                callback_webtransport_endpoint_count = callback_endpoints.webtransport_ips.len(),
                "Agent endpoint is ready; building browser callback"
            );
            let callback = build_callback_url(
                return_url,
                &nonce,
                ready,
                &callback_endpoints,
                &launch_token,
                launch_expires_at_ms,
            );
            open_browser(callback.as_str())?;
            tracing::info!(
                callback_origin = %callback.origin().ascii_serialization(),
                callback_path = callback.path(),
                "secure Agent callback opened in the default browser"
            );
            Ok(())
        },
    )
    .await
}

fn parse_activation(value: &str, additional_trusted_origins: &[String]) -> anyhow::Result<Activation> {
    tracing::trace!(
        activation_length = value.len(),
        "parsing winrisef protocol activation"
    );
    let activation = Url::parse(value).context("invalid winrisef activation URI")?;
    anyhow::ensure!(
        activation.scheme() == "winrisef" && activation.host_str() == Some("launch"),
        "unsupported winrisef activation"
    );
    let mut return_url = None;
    let mut nonce = None;
    for (key, value) in activation.query_pairs() {
        match key.as_ref() {
            "returnUrl" => return_url = Some(value.into_owned()),
            "nonce" => nonce = Some(value.into_owned()),
            _ => {}
        }
    }
    let mut return_url = Url::parse(return_url.as_deref().context("activation is missing returnUrl")?)
        .context("invalid browser return URL")?;
    anyhow::ensure!(
        return_url.username().is_empty() && return_url.password().is_none(),
        "browser return URL must not contain credentials"
    );
    let secure = return_url.scheme() == "https";
    let loopback = matches!(return_url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    anyhow::ensure!(
        secure || (return_url.scheme() == "http" && loopback),
        "browser return URL is not secure"
    );
    let allowed_origin = return_url.origin().ascii_serialization();
    anyhow::ensure!(allowed_origin != "null", "browser return URL has no Origin");
    anyhow::ensure!(
        loopback
            || PRODUCTION_WEB_ORIGINS.contains(&allowed_origin.as_str())
            || additional_trusted_origins
                .iter()
                .any(|origin| origin == &allowed_origin),
        "browser Origin is not trusted by this Agent"
    );
    tracing::debug!(
        %allowed_origin,
        secure,
        loopback,
        official_origin = PRODUCTION_WEB_ORIGINS.contains(&allowed_origin.as_str()),
        explicitly_trusted = additional_trusted_origins
            .iter()
            .any(|origin| origin == &allowed_origin),
        "browser callback Origin passed trust checks"
    );
    let nonce = nonce.context("activation is missing nonce")?;
    anyhow::ensure!(
        nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "activation nonce must be 32 hexadecimal characters"
    );
    return_url.set_fragment(None);
    Ok(Activation {
        return_url,
        nonce,
        allowed_origin,
    })
}

fn build_callback_url(
    mut return_url: Url,
    nonce: &str,
    ready: &ReadyInfo,
    endpoints: &NetworkEndpoints,
    launch_token: &[u8; 16],
    launch_expires_at_ms: u64,
) -> Url {
    let bridge = endpoint(IpAddr::V4(Ipv4Addr::LOCALHOST), ready.port, BRIDGE_PATH);
    let mut fragment = url::form_urlencoded::Serializer::new(String::new());
    fragment.append_pair("winrisef-agent", "1");
    fragment.append_pair("nonce", nonce);
    fragment.append_pair("bridge", &bridge);
    fragment.append_pair("certificate", &ready.certificate_sha256);
    fragment.append_pair("token", &certificate::format_hex(launch_token));
    fragment.append_pair("expires", &launch_expires_at_ms.to_string());
    fragment.append_pair("network-epoch", &format!("{:016x}", endpoints.network_epoch));
    for ip in &endpoints.webtransport_ips {
        fragment.append_pair("lan", &endpoint(*ip, ready.port, BENCHMARK_PATH));
        fragment.append_pair("file-wt", &endpoint(*ip, ready.port, FILE_WEBTRANSPORT_PATH));
    }
    for ip in &endpoints.http_ips {
        fragment.append_pair("lan-http", &http_endpoint(*ip, ready.port, LNA_HTTP_BASE_PATH));
        fragment.append_pair("file-http", &http_endpoint(*ip, ready.port, FILE_HTTP_BASE_PATH));
    }
    return_url.set_fragment(Some(&fragment.finish()));
    return_url
}

fn http_endpoint(ip: IpAddr, port: u16, path: &str) -> String {
    match ip {
        IpAddr::V4(ip) => format!("http://{ip}:{port}{path}"),
        IpAddr::V6(ip) => format!("http://[{ip}]:{port}{path}"),
    }
}

fn endpoint(ip: IpAddr, port: u16, path: &str) -> String {
    match ip {
        IpAddr::V4(ip) => format!("https://{ip}:{port}{path}"),
        IpAddr::V6(ip) => format!("https://[{ip}]:{port}{path}"),
    }
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> anyhow::Result<()> {
    let child = Command::new("rundll32.exe")
        .arg("url.dll,FileProtocolHandler")
        .arg(url)
        .spawn()
        .context("failed to open the browser callback")?;
    tracing::debug!(
        process_id = child.id(),
        launcher = "rundll32",
        "spawned browser callback handler"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> anyhow::Result<()> {
    Command::new("open")
        .arg(url)
        .spawn()
        .context("failed to open the browser callback")?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_browser(url: &str) -> anyhow::Result<()> {
    Command::new("xdg-open")
        .arg(url)
        .spawn()
        .context("failed to open the browser callback")?;
    Ok(())
}
