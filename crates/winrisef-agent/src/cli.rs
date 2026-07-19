use std::net::SocketAddr;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "winrisef-agent",
    version,
    about = "Headless WinriseF WebTransport accelerator"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the phase-0 server with an explicitly supplied benchmark token.
    Serve(ServeArgs),
    /// Handle a winrisef:// browser activation and start a secured local Agent session.
    Launch(LaunchArgs),
    /// Register this executable as the per-user winrisef:// handler on Windows.
    RegisterProtocol(RegisterProtocolArgs),
    /// Remove the per-user winrisef:// handler on Windows.
    UnregisterProtocol,
}

#[derive(Clone, Debug, Args)]
pub struct ServeArgs {
    /// UDP address for the browser-facing WebTransport endpoint.
    #[arg(long, default_value = "0.0.0.0:17691")]
    pub listen: SocketAddr,

    /// Exact browser Origin allowed to create a session. Repeat for development origins.
    #[arg(long = "allowed-origin", required = true)]
    pub allowed_origins: Vec<String>,

    /// Exact WebTransport URL path.
    #[arg(long, default_value = "/winrisef/p0")]
    pub path: String,

    /// Benchmark-only 128-bit token encoded as 32 hexadecimal characters.
    #[arg(long, value_parser = parse_token)]
    pub token: [u8; 16],

    /// Maximum accepted memory benchmark size.
    #[arg(long, default_value_t = 1024_u64 * 1024 * 1024 * 1024)]
    pub max_transfer_size: u64,

    /// Maximum concurrently active benchmark sessions.
    #[arg(long, default_value_t = 1, value_parser = parse_session_count)]
    pub max_sessions: usize,

    /// Sample CPU/RSS once per second.
    #[arg(long)]
    pub metrics: bool,
}

#[derive(Clone, Debug, Args)]
pub struct LaunchArgs {
    /// Full winrisef:// activation URI supplied by the browser.
    pub uri: String,

    /// UDP address used for the local Bridge and remote benchmark endpoint.
    #[arg(long, default_value = "0.0.0.0:17691")]
    pub listen: SocketAddr,

    /// Maximum accepted memory benchmark size.
    #[arg(long, default_value_t = 1024_u64 * 1024 * 1024 * 1024)]
    pub max_transfer_size: u64,

    /// Maximum concurrently active remote benchmark sessions.
    #[arg(long, default_value_t = 1, value_parser = parse_session_count)]
    pub max_sessions: usize,

    /// Sample CPU/RSS once per second.
    #[arg(long)]
    pub metrics: bool,

    /// Additional exact HTTPS Origin trusted to activate this Agent. Repeat for staging sites.
    #[arg(long = "trusted-origin", value_parser = parse_trusted_origin)]
    pub trusted_origins: Vec<String>,
}

#[derive(Clone, Debug, Args)]
pub struct RegisterProtocolArgs {
    /// Additional exact HTTPS Origin embedded in the URI handler. Repeat for staging sites.
    #[arg(long = "trusted-origin", value_parser = parse_trusted_origin)]
    pub trusted_origins: Vec<String>,
}

fn parse_session_count(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| "session count must be an integer".to_owned())?;
    if (1..=8).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("session count must be between 1 and 8".to_owned())
    }
}

fn parse_trusted_origin(value: &str) -> Result<String, String> {
    let url = url::Url::parse(value).map_err(|_| "trusted Origin is not a valid URL".to_owned())?;
    if url.origin().ascii_serialization() != value {
        return Err(
            "trusted Origin must not contain a path, query, fragment, or trailing slash".to_owned(),
        );
    }
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(
            "trusted Origin must use HTTPS; loopback HTTP is allowed for development".to_owned(),
        );
    }
    Ok(value.to_owned())
}

fn parse_token(value: &str) -> Result<[u8; 16], String> {
    if value.len() != 32 {
        return Err("token must contain exactly 32 hexadecimal characters".to_owned());
    }
    let mut token = [0; 16];
    for (index, byte) in token.iter_mut().enumerate() {
        let start = index * 2;
        *byte = u8::from_str_radix(&value[start..start + 2], 16)
            .map_err(|_| "token contains a non-hexadecimal character".to_owned())?;
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::parse_token;

    #[test]
    fn parses_exact_token() {
        assert_eq!(
            parse_token("000102030405060708090a0b0c0d0e0f").unwrap(),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
    }
}
