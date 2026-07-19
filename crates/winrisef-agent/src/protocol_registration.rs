use anyhow::Context;

use crate::cli::RegisterProtocolArgs;

#[cfg(target_os = "windows")]
pub fn register(args: RegisterProtocolArgs) -> anyhow::Result<()> {
    tracing::info!(
        trusted_origin_count = args.trusted_origins.len(),
        "registering winrisef protocol for current Windows user"
    );
    let executable = std::env::current_exe().context("failed to locate winrisef-agent.exe")?;
    let executable = executable
        .to_str()
        .context("winrisef-agent.exe path is not valid Unicode")?;
    let mut command = format!("\"{executable}\" launch \"%1\"");
    for origin in args.trusted_origins {
        command.push_str(" --trusted-origin \"");
        command.push_str(&origin);
        command.push('"');
    }
    reg_add(
        r"HKCU\Software\Classes\winrisef",
        None,
        "URL:WinriseF Native Transfer",
    )?;
    reg_add(r"HKCU\Software\Classes\winrisef", Some("URL Protocol"), "")?;
    reg_add(
        r"HKCU\Software\Classes\winrisef\shell\open\command",
        None,
        &command,
    )?;
    tracing::info!(
        registry_key = r"HKCU\Software\Classes\winrisef",
        "winrisef protocol registry entries written"
    );
    println!("winrisef:// protocol registered for the current user");
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn unregister() -> anyhow::Result<()> {
    tracing::info!("removing winrisef protocol registration for current Windows user");
    let status = quiet_reg_command()
        .args(["delete", r"HKCU\Software\Classes\winrisef", "/f"])
        .status()
        .context("failed to start reg.exe")?;
    anyhow::ensure!(status.success(), "reg.exe could not remove winrisef://");
    tracing::info!(
        registry_key = r"HKCU\Software\Classes\winrisef",
        "winrisef protocol registration removed"
    );
    println!("winrisef:// protocol registration removed");
    Ok(())
}

#[cfg(target_os = "windows")]
fn reg_add(key: &str, value_name: Option<&str>, data: &str) -> anyhow::Result<()> {
    tracing::debug!(
        key,
        ?value_name,
        data_length = data.len(),
        "writing protocol registry value"
    );
    let mut command = quiet_reg_command();
    command.args(["add", key]);
    match value_name {
        Some(name) => command.args(["/v", name]),
        None => command.arg("/ve"),
    };
    let status = command
        .args(["/d", data, "/f"])
        .status()
        .context("failed to start reg.exe")?;
    anyhow::ensure!(status.success(), "reg.exe could not register winrisef://");
    tracing::trace!(key, ?value_name, "protocol registry value write succeeded");
    Ok(())
}

#[cfg(target_os = "windows")]
fn quiet_reg_command() -> std::process::Command {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut command = std::process::Command::new("reg.exe");
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(target_os = "windows"))]
pub fn register(_args: RegisterProtocolArgs) -> anyhow::Result<()> {
    anyhow::bail!("automatic protocol registration is currently available only on Windows")
}

#[cfg(not(target_os = "windows"))]
pub fn unregister() -> anyhow::Result<()> {
    anyhow::bail!("automatic protocol registration is currently available only on Windows")
}
