#[cfg(target_os = "windows")]
pub fn ensure_inbound_rules(port: u16) -> anyhow::Result<()> {
    use std::{os::windows::process::CommandExt, process::Command};

    use anyhow::Context;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let executable = std::env::current_exe().context("failed to resolve the Agent executable path")?;
    let executable = executable
        .to_str()
        .context("Agent executable path is not valid Unicode")?;
    let udp_name = format!("WinriseF Agent WebTransport UDP {port}");
    let tcp_name = format!("WinriseF Agent LAN HTTP TCP {port}");
    let path = powershell_literal(executable);
    let query = format!(
        "$rules = Get-NetFirewallRule -DisplayName '{udp_name}','{tcp_name}' -ErrorAction SilentlyContinue; \
         $programs = @($rules | Get-NetFirewallApplicationFilter | Where-Object {{ $_.Program -eq '{path}' }}); \
         if ($rules.Count -ge 2 -and $programs.Count -ge 2) {{ exit 0 }} else {{ exit 1 }}"
    );
    let status = hidden_powershell().arg("-Command").arg(query).status()?;
    if status.success() {
        tracing::debug!(port, "required Windows Firewall rules already exist");
        return Ok(());
    }

    let elevated_script = format!(
        "$ErrorActionPreference = 'Stop'; \
         Remove-NetFirewallRule -DisplayName '{udp_name}','{tcp_name}' -ErrorAction SilentlyContinue; \
         New-NetFirewallRule -DisplayName '{udp_name}' -Direction Inbound -Action Allow -Protocol UDP -LocalPort {port} -Program '{path}' -Profile Any | Out-Null; \
         New-NetFirewallRule -DisplayName '{tcp_name}' -Direction Inbound -Action Allow -Protocol TCP -LocalPort {port} -RemoteAddress LocalSubnet -Program '{path}' -Profile Any | Out-Null"
    );
    let encoded = encode_powershell(&elevated_script);
    let elevate = format!(
        "$process = Start-Process -FilePath 'powershell.exe' -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{encoded}'; exit $process.ExitCode"
    );
    let status = hidden_powershell()
        .arg("-Command")
        .arg(elevate)
        .status()
        .context("failed to request Windows Firewall authorization")?;
    anyhow::ensure!(
        status.success(),
        "Windows Firewall authorization was cancelled or failed"
    );
    tracing::info!(
        port,
        "installed path-scoped UDP and LocalSubnet TCP inbound firewall rules"
    );
    return Ok(());

    fn hidden_powershell() -> Command {
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive"])
            .creation_flags(CREATE_NO_WINDOW);
        command
    }

    fn powershell_literal(value: &str) -> String {
        value.replace('\'', "''")
    }

    fn encode_powershell(script: &str) -> String {
        let bytes = script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        base64(&bytes)
    }

    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let value = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            output.push(ALPHABET[((value >> 18) & 63) as usize] as char);
            output.push(ALPHABET[((value >> 12) & 63) as usize] as char);
            output.push(if chunk.len() > 1 {
                ALPHABET[((value >> 6) & 63) as usize] as char
            } else {
                '='
            });
            output.push(if chunk.len() > 2 {
                ALPHABET[(value & 63) as usize] as char
            } else {
                '='
            });
        }
        output
    }
}

#[cfg(not(target_os = "windows"))]
pub fn ensure_inbound_rules(_port: u16) -> anyhow::Result<()> {
    Ok(())
}
