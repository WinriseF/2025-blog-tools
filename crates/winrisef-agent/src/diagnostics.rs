use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use tracing_subscriber::fmt::MakeWriter;

const DIAGNOSTIC_FILTER: &str = "trace";

#[derive(Clone)]
struct TeeMakeWriter {
    file: Arc<Mutex<File>>,
}

struct TeeWriter {
    file: Arc<Mutex<File>>,
    stderr: io::Stderr,
}

impl<'writer> MakeWriter<'writer> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        TeeWriter {
            file: Arc::clone(&self.file),
            stderr: io::stderr(),
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stderr.write_all(bytes)?;
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("diagnostic log lock is poisoned"))?;
        file.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()?;
        self.file
            .lock()
            .map_err(|_| io::Error::other("diagnostic log lock is poisoned"))?
            .flush()
    }
}

pub fn init() -> anyhow::Result<PathBuf> {
    let directory = diagnostic_directory();
    fs::create_dir_all(&directory).with_context(|| {
        format!(
            "failed to create diagnostic log directory {}",
            directory.display()
        )
    })?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    let path = directory.join(format!(
        "winrisef-agent-{now_ms}-{}.log",
        std::process::id()
    ));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to create diagnostic log {}", path.display()))?;
    let writer = TeeMakeWriter {
        file: Arc::new(Mutex::new(file)),
    };
    let filter = tracing_subscriber::EnvFilter::new(DIAGNOSTIC_FILTER);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_writer(writer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize diagnostic logging: {error}"))?;
    std::panic::set_hook(Box::new(|panic| {
        tracing::error!(panic = %panic, "agent process panicked");
    }));
    tracing::info!(
        log_path = %path.display(),
        process_id = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "diagnostic logging initialized"
    );
    Ok(path)
}

fn diagnostic_directory() -> PathBuf {
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        PathBuf::from(user_profile)
            .join("Documents")
            .join("WinriseF-Agent-Logs")
    } else if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        PathBuf::from(local_app_data).join("WinriseF").join("logs")
    } else {
        std::env::temp_dir().join("WinriseF").join("logs")
    }
}
