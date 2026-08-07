use anyhow::Context;

use crate::{cli::RegisterProtocolArgs, protocol_registration};

const READY_URL: &str = "https://e.winrisef.top/toolbox/agent?agent-ready=1";
const FAILED_URL: &str = "https://e.winrisef.top/toolbox/agent?agent-ready=0";

pub fn run() -> anyhow::Result<()> {
    tracing::info!(
        ready_url = READY_URL,
        "starting portable Agent first-run bootstrap"
    );

    match protocol_registration::register(RegisterProtocolArgs {
        trusted_origins: Vec::new(),
    }) {
        Ok(()) => {
            open_browser(READY_URL).context("failed to open the Agent ready page")?;
            tracing::info!("portable Agent bootstrap completed successfully");
            Ok(())
        }
        Err(error) => {
            tracing::error!(error = ?error, "portable Agent self-registration failed");
            if let Err(callback_error) = open_browser(FAILED_URL) {
                tracing::error!(
                    error = ?callback_error,
                    "failed to open the Agent setup failure page"
                );
            }
            Err(error).context("failed to register this executable as the winrisef:// handler")
        }
    }
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> anyhow::Result<()> {
    let child = std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .context("failed to start the Windows URL handler")?;
    tracing::info!(
        process_id = child.id(),
        url,
        "opened Agent setup result page"
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn open_browser(_url: &str) -> anyhow::Result<()> {
    anyhow::bail!("portable Agent bootstrap is currently available only on Windows")
}
