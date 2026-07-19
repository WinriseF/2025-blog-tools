use std::{
    fs::{self, File, OpenOptions},
    io::{self, LineWriter, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use tracing_subscriber::fmt::MakeWriter;

const DIAGNOSTIC_FILTER_ENV: &str = "WINRISEF_AGENT_LOG";
const DEFAULT_DIAGNOSTIC_FILTER: &str = "winrisef_agent=debug,winrisef_core=debug,web_transport_quinn=info,quinn=warn,quinn_proto=warn,rustls=warn,h3=warn";
#[derive(Clone)]
struct TeeMakeWriter {
    file: Arc<Mutex<LineWriter<File>>>,
    console: bool,
}

struct TeeWriter {
    file: Arc<Mutex<LineWriter<File>>>,
    stderr: Option<io::Stderr>,
}

impl<'writer> MakeWriter<'writer> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        TeeWriter {
            file: Arc::clone(&self.file),
            stderr: self.console.then(io::stderr),
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("diagnostic log lock is poisoned"))?;
        file.write_all(bytes)?;
        if bytes.contains(&b'\n') {
            file.flush()?;
        }
        if let Some(stderr) = &mut self.stderr {
            stderr.write_all(bytes)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .lock()
            .map_err(|_| io::Error::other("diagnostic log lock is poisoned"))?
            .flush()?;
        if let Some(stderr) = &mut self.stderr {
            stderr.flush()?;
        }
        Ok(())
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
    let filter_text = std::env::var(DIAGNOSTIC_FILTER_ENV)
        .unwrap_or_else(|_| DEFAULT_DIAGNOSTIC_FILTER.to_owned());
    let filter = tracing_subscriber::EnvFilter::try_new(&filter_text)
        .with_context(|| format!("invalid {DIAGNOSTIC_FILTER_ENV} log filter"))?;
    let writer = TeeMakeWriter {
        file: Arc::new(Mutex::new(LineWriter::new(file))),
        console: cfg!(debug_assertions),
    };
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
        filter = %filter_text,
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
