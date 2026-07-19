use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use sysinfo::{ProcessesToUpdate, System};
use tokio::{sync::oneshot, task::JoinHandle};

#[derive(Debug, Default)]
pub struct Progress {
    bytes: AtomicU64,
}

impl Progress {
    pub fn add(&self, bytes: usize) {
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TransferStats {
    pub elapsed: Duration,
    pub bytes: u64,
    pub average_mbps: f64,
}

pub struct Monitor {
    started: Instant,
    progress: Arc<Progress>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start(direction: &'static str, enabled: bool) -> Self {
        let started = Instant::now();
        let progress = Arc::new(Progress::default());
        if !enabled {
            return Self {
                started,
                progress,
                stop: None,
                task: None,
            };
        }
        let task_progress = Arc::clone(&progress);
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let pid = sysinfo::get_current_pid().ok();
            let mut system = System::new();
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            let mut previous_at = Instant::now();
            let mut previous_bytes = 0_u64;

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let now = Instant::now();
                        let bytes = task_progress.bytes();
                        let seconds = now.duration_since(previous_at).as_secs_f64().max(f64::EPSILON);
                        let current_mbps = (bytes - previous_bytes) as f64 * 8.0 / seconds / 1_000_000.0;
                        let average_mbps = bytes as f64 * 8.0
                            / now.duration_since(started).as_secs_f64().max(f64::EPSILON)
                            / 1_000_000.0;
                        let (cpu, memory_mib) = process_metrics(&mut system, pid);
                        tracing::info!(
                            direction,
                            elapsed_seconds = now.duration_since(started).as_secs_f64(),
                            bytes,
                            current_mbps,
                            average_mbps,
                            cpu_percent = cpu,
                            memory_mib,
                            "transfer progress"
                        );
                        previous_at = now;
                        previous_bytes = bytes;
                    }
                    _ = &mut stopped => break,
                }
            }
        });

        Self {
            started,
            progress,
            stop: Some(stop),
            task: Some(task),
        }
    }

    pub fn progress(&self) -> Arc<Progress> {
        Arc::clone(&self.progress)
    }

    pub async fn finish(mut self) -> TransferStats {
        let elapsed = self.started.elapsed();
        let bytes = self.progress.bytes();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task {
            let _ = task.await;
        }
        TransferStats {
            elapsed,
            bytes,
            average_mbps: bytes as f64 * 8.0
                / elapsed.as_secs_f64().max(f64::EPSILON)
                / 1_000_000.0,
        }
    }
}

fn process_metrics(system: &mut System, pid: Option<sysinfo::Pid>) -> (f32, f64) {
    let Some(pid) = pid else {
        return (0.0, 0.0);
    };
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map_or((0.0, 0.0), |process| {
        (
            process.cpu_usage(),
            process.memory() as f64 / 1024.0 / 1024.0,
        )
    })
}
