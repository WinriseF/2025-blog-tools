#![forbid(unsafe_code)]
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod auth;
mod bootstrap;
mod bridge;
mod certificate;
mod cli;
mod diagnostics;
mod launch;
mod lna_http;
mod metrics;
mod protocol_registration;
mod server;
mod transfer;
mod tuning;

use clap::Parser;
use cli::{Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let diagnostic_log = diagnostics::init()?;
    tracing::info!(
        path = %diagnostic_log.display(),
        "Agent diagnostic log initialized"
    );
    #[cfg(debug_assertions)]
    eprintln!(
        "WinriseF Agent diagnostic log: {}",
        diagnostic_log.display()
    );
    let result = if std::env::args_os().nth(1).is_none() {
        tracing::info!(
            mode = "portable-bootstrap",
            "agent started without arguments"
        );
        bootstrap::run()
    } else {
        run_cli().await
    };
    match &result {
        Ok(()) => tracing::info!("agent command completed successfully"),
        Err(error) => {
            tracing::error!(error = ?error, "agent command terminated with an error")
        }
    }
    result
}

async fn run_cli() -> anyhow::Result<()> {
    let args = match Cli::try_parse() {
        Ok(args) => args,
        Err(error) => {
            tracing::error!(error = %error, "command-line parsing failed");
            error.exit();
        }
    };

    match args.command {
        Command::Serve(args) => {
            certificate::install_crypto_provider()?;
            tracing::info!(mode = "manual-serve", listen = %args.listen, "agent command started");
            server::run_manual(args).await
        }
        Command::Launch(args) => {
            certificate::install_crypto_provider()?;
            tracing::info!(mode = "protocol-launch", listen = %args.listen, "agent command started");
            launch::run(args).await
        }
        Command::RegisterProtocol(args) => protocol_registration::register(args),
        Command::UnregisterProtocol => protocol_registration::unregister(),
    }
}
