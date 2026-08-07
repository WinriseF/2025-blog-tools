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
    firewall,
    network_endpoints::{self, EndpointPolicy, PublicIpv6State, PublishedNetworkEndpoints},
    server::{self, BRIDGE_PATH, LaunchedServerSettings, ReadyInfo},
    single_instance, version_control_server,
};

const LAUNCH_TTL: Duration = Duration::from_secs(120);
const PRODUCTION_WEB_ORIGINS: [&str; 3] = [
    "https://e.winrisef.top",
    "https://n.winrisef.top",
    "https://v.winrisef.top",
];

struct Activation {
    return_url: Url,
    nonce: String,
    allowed_origin: String,
    feature: LaunchFeature,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchFeature {
    Transfer,
    VersionControl,
}

pub async fn run(args: LaunchArgs) -> anyhow::Result<()> {
    let activation = parse_activation(&args.uri, &args.trusted_origins)?;
    let _instance_guard = match single_instance::acquire() {
        Ok(guard) => guard,
        Err(_error) if activation.feature == LaunchFeature::VersionControl => {
            let callback = build_version_control_error_callback(
                activation.return_url,
                &activation.nonce,
                "agent_busy",
            );
            open_browser(callback.as_str())?;
            tracing::info!("reported version-control Agent busy state to the browser");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    tracing::info!(
        listen = %args.listen,
        additional_trusted_origin_count = args.trusted_origins.len(),
        max_sessions = args.max_sessions,
        max_transfer_size = args.max_transfer_size,
        metrics_enabled = args.metrics,
        "processing browser protocol activation"
    );
    if activation.feature == LaunchFeature::VersionControl {
        return run_version_control(activation).await;
    }
    let network_endpoints = network_endpoints::discover();
    let has_public_ipv6 = network_endpoints.has_public_ipv6();
    let endpoint_policy = EndpointPolicy::new(if has_public_ipv6 {
        PublicIpv6State::Authorizing
    } else {
        PublicIpv6State::NotPresent
    });
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
    let return_url = activation.return_url;
    let nonce = activation.nonce;
    let firewall_policy = endpoint_policy.clone();
    let firewall_port = args.listen.port();
    let (firewall_start, firewall_ready) = tokio::sync::oneshot::channel();
    let firewall_task = tokio::spawn(async move {
        if firewall_ready.await.is_ok() {
            maintain_public_ipv6_firewall(firewall_port, firewall_policy).await;
        }
    });
    let callback_policy = endpoint_policy.clone();
    let result = server::run_launched(
        LaunchedServerSettings {
            listen: args.listen,
            allowed_origin: activation.allowed_origin,
            certificate_ips: Vec::new(),
            authority,
            endpoint_policy,
            max_transfer_size: args.max_transfer_size,
            max_sessions: args.max_sessions,
            metrics_enabled: args.metrics,
        },
        move |ready| {
            let callback_endpoints = callback_policy.published(ready.port);
            tracing::info!(
                port = ready.port,
                certificate_sha256 = %ready.certificate_sha256,
                callback_http_endpoint_count = callback_endpoints.lna_http_endpoints.len(),
                callback_webtransport_endpoint_count = callback_endpoints.benchmark_endpoints.len(),
                public_ipv6_state = ?callback_endpoints.public_ipv6_state,
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
            let _ = firewall_start.send(());
            tracing::info!(
                callback_origin = %callback.origin().ascii_serialization(),
                callback_path = callback.path(),
                "secure Agent callback opened in the default browser"
            );
            Ok(())
        },
    )
    .await;
    firewall_task.abort();
    result
}

async fn run_version_control(activation: Activation) -> anyhow::Result<()> {
    let launch_token = random_token()?;
    let expires_at_ms = now_ms()?.saturating_add(LAUNCH_TTL.as_millis() as u64);
    let authority = TicketAuthority::new(launch_token, expires_at_ms);
    let return_url = activation.return_url;
    let nonce = activation.nonce;
    version_control_server::run(activation.allowed_origin, authority, move |ready| {
        let callback =
            build_version_control_callback(return_url, &nonce, ready, &launch_token, expires_at_ms);
        open_browser(callback.as_str())?;
        Ok(())
    })
    .await
}

async fn maintain_public_ipv6_firewall(port: u16, policy: EndpointPolicy) {
    let mut endpoint_changes = network_endpoints::watch_changes();
    let mut authorization_result = None;
    loop {
        let has_public_ipv6 = network_endpoints::discover().has_public_ipv6();
        if !has_public_ipv6 {
            policy.set_public_ipv6_state(PublicIpv6State::NotPresent);
        } else if let Some(authorized) = authorization_result {
            policy.set_public_ipv6_state(if authorized {
                PublicIpv6State::Available
            } else {
                PublicIpv6State::Unavailable
            });
        } else {
            policy.set_public_ipv6_state(PublicIpv6State::Authorizing);
            tracing::info!(
                port,
                "requesting asynchronous public IPv6 firewall authorization"
            );
            let started = std::time::Instant::now();
            let result =
                tokio::task::spawn_blocking(move || firewall::ensure_inbound_rules(port)).await;
            let authorized = match result {
                Ok(Ok(())) => true,
                Ok(Err(error)) => {
                    tracing::warn!(error = ?error, "public IPv6 firewall authorization was not completed; private endpoints and WebRTC remain available");
                    false
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "public IPv6 firewall authorization task failed; private endpoints and WebRTC remain available");
                    false
                }
            };
            tracing::info!(
                authorized,
                elapsed_ms = started.elapsed().as_millis(),
                "asynchronous public IPv6 firewall authorization finished"
            );
            authorization_result = Some(authorized);
            if network_endpoints::discover().has_public_ipv6() {
                policy.set_public_ipv6_state(if authorized {
                    PublicIpv6State::Available
                } else {
                    PublicIpv6State::Unavailable
                });
            } else {
                policy.set_public_ipv6_state(PublicIpv6State::NotPresent);
            }
        }
        endpoint_changes.changed().await;
    }
}

fn parse_activation(
    value: &str,
    additional_trusted_origins: &[String],
) -> anyhow::Result<Activation> {
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
    let mut feature = LaunchFeature::Transfer;
    for (key, value) in activation.query_pairs() {
        match key.as_ref() {
            "returnUrl" => return_url = Some(value.into_owned()),
            "nonce" => nonce = Some(value.into_owned()),
            "feature" => {
                feature = match value.as_ref() {
                    "transfer" => LaunchFeature::Transfer,
                    "version-control" => LaunchFeature::VersionControl,
                    _ => anyhow::bail!("unsupported Agent feature"),
                }
            }
            _ => {}
        }
    }
    let mut return_url = Url::parse(
        return_url
            .as_deref()
            .context("activation is missing returnUrl")?,
    )
    .context("invalid browser return URL")?;
    anyhow::ensure!(
        return_url.username().is_empty() && return_url.password().is_none(),
        "browser return URL must not contain credentials"
    );
    let secure = return_url.scheme() == "https";
    let loopback = matches!(
        return_url.host_str(),
        Some("localhost" | "127.0.0.1" | "::1")
    );
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
        feature,
    })
}

fn build_version_control_callback(
    mut return_url: Url,
    nonce: &str,
    ready: &ReadyInfo,
    launch_token: &[u8; 16],
    expires_at_ms: u64,
) -> Url {
    let bridge = endpoint(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        ready.port,
        version_control_server::VERSION_CONTROL_PATH,
    );
    let mut fragment = url::form_urlencoded::Serializer::new(String::new());
    fragment.append_pair("winrisef-version-control", "1");
    fragment.append_pair("nonce", nonce);
    fragment.append_pair("bridge", &bridge);
    fragment.append_pair("certificate", &ready.certificate_sha256);
    fragment.append_pair("token", &certificate::format_hex(launch_token));
    fragment.append_pair("expires", &expires_at_ms.to_string());
    return_url.set_fragment(Some(&fragment.finish()));
    return_url
}

fn build_version_control_error_callback(mut return_url: Url, nonce: &str, error: &str) -> Url {
    let mut fragment = url::form_urlencoded::Serializer::new(String::new());
    fragment.append_pair("winrisef-version-control", "1");
    fragment.append_pair("nonce", nonce);
    fragment.append_pair("error", error);
    return_url.set_fragment(Some(&fragment.finish()));
    return_url
}

fn build_callback_url(
    mut return_url: Url,
    nonce: &str,
    ready: &ReadyInfo,
    endpoints: &PublishedNetworkEndpoints,
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
    fragment.append_pair("network-epoch", &endpoints.network_epoch);
    fragment.append_pair("public-ipv6-state", endpoints.public_ipv6_state.as_str());
    for endpoint in &endpoints.benchmark_endpoints {
        fragment.append_pair("lan", endpoint);
    }
    for endpoint in &endpoints.file_web_transport_endpoints {
        fragment.append_pair("file-wt", endpoint);
    }
    for endpoint in &endpoints.lna_http_endpoints {
        fragment.append_pair("lan-http", endpoint);
    }
    for endpoint in &endpoints.file_http_endpoints {
        fragment.append_pair("file-http", endpoint);
    }
    return_url.set_fragment(Some(&fragment.finish()));
    return_url
}

fn endpoint(ip: IpAddr, port: u16, path: &str) -> String {
    match ip {
        IpAddr::V4(ip) => format!("https://{ip}:{port}{path}"),
        IpAddr::V6(ip) => format!("https://[{ip}]:{port}{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{PRODUCTION_WEB_ORIGINS, parse_activation};
    use url::Url;

    fn activation_uri(return_url: &str) -> String {
        let mut activation = Url::parse("winrisef://launch").expect("valid launch URL");
        activation
            .query_pairs_mut()
            .append_pair("returnUrl", return_url)
            .append_pair("nonce", "0123456789abcdef0123456789abcdef");
        activation.into()
    }

    #[test]
    fn production_origins_are_trusted_for_callbacks() {
        for origin in PRODUCTION_WEB_ORIGINS {
            let callback = format!("{origin}/t");
            let activation = parse_activation(&activation_uri(&callback), &[])
                .expect("production callback should be trusted");
            assert_eq!(activation.allowed_origin, origin);
        }
    }

    #[test]
    fn production_origin_matching_is_exact() {
        for callback in [
            "http://n.winrisef.top/t",
            "https://other.n.winrisef.top/t",
            "https://v.winrisef.top.example.com/t",
        ] {
            assert!(parse_activation(&activation_uri(callback), &[]).is_err());
        }
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
