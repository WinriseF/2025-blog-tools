use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use web_transport_quinn::{RecvStream, SendStream, Session};
use winrisef_version_control::{
    ConflictPerspective, DiffSession, ExportFormat, ExportLayout, ExportOptions, RepositoryReader,
    RevisionRef, WorkingTreeGroup,
};

use crate::auth::{TicketAuthority, parse_hex};

mod file_selection;

use file_selection::FileSelection;

const VERSION: u16 = 1;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HISTORY_PAGE: usize = 64;
const MAX_FILE_PAGE: usize = 128;
const MAX_DIFF_SESSIONS: usize = 3;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum BridgeInput {
    Hello {
        version: u16,
        #[serde(rename = "launchToken")]
        launch_token: String,
    },
    SelectRepository {
        #[serde(rename = "requestId")]
        request_id: u32,
    },
    CloseRepository {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "repositoryId")]
        repository_id: String,
    },
    GetRepositoryOverview {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "repositoryId")]
        repository_id: String,
    },
    GetHistoryPage {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "repositoryId")]
        repository_id: String,
        query: Option<String>,
        skip: usize,
        limit: usize,
    },
    OpenDiff {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "repositoryId")]
        repository_id: String,
        #[serde(rename = "oldRevision")]
        old_revision: RevisionRef,
        #[serde(rename = "newRevision")]
        new_revision: RevisionRef,
        group: WorkingTreeGroup,
    },
    GetDiffFilesPage {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "repositoryId")]
        repository_id: String,
        #[serde(rename = "diffId")]
        diff_id: String,
        skip: usize,
        limit: usize,
    },
    OpenFilePreview {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "repositoryId")]
        repository_id: String,
        #[serde(rename = "diffId")]
        diff_id: String,
        #[serde(rename = "fileId")]
        file_id: u32,
        perspective: ConflictPerspective,
    },
    PrepareExport {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "repositoryId")]
        repository_id: String,
        #[serde(rename = "diffId")]
        diff_id: String,
        format: ExportFormat,
        layout: ExportLayout,
        #[serde(rename = "fileSelection")]
        file_selection: FileSelection,
    },
    ConfirmExport {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "exportTargetId")]
        export_target_id: String,
        #[serde(rename = "allowInsideRepository")]
        allow_inside_repository: bool,
    },
    CancelExport {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "exportTargetId")]
        export_target_id: String,
    },
    RefreshRepository {
        #[serde(rename = "requestId")]
        request_id: u32,
        #[serde(rename = "repositoryId")]
        repository_id: String,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum BridgeOutput<'a> {
    HelloAck {
        version: u16,
        accepted: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
    },
    Response {
        #[serde(rename = "requestId")]
        request_id: u32,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ExportEvent {
    #[serde(rename = "export-complete")]
    Complete {
        #[serde(rename = "exportTargetId")]
        export_target_id: String,
        #[serde(rename = "insideRepository")]
        inside_repository: bool,
    },
    #[serde(rename = "export-failed")]
    Failed {
        #[serde(rename = "exportTargetId")]
        export_target_id: String,
        error: String,
    },
    #[serde(rename = "export-cancelled")]
    Cancelled {
        #[serde(rename = "exportTargetId")]
        export_target_id: String,
    },
}

struct RepositoryState {
    id: String,
    reader: RepositoryReader,
    diffs: HashMap<String, Arc<DiffSession>>,
    diff_order: VecDeque<String>,
}

struct PendingExport {
    id: String,
    repository_id: String,
    diff: Arc<DiffSession>,
    path: PathBuf,
    inside_repository: bool,
    options: ExportOptions,
}

struct ActiveExport {
    id: String,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct ManagerState {
    repository: Option<RepositoryState>,
    pending_exports: HashMap<String, PendingExport>,
    active_export: Option<ActiveExport>,
}

pub struct VersionControlManager {
    state: Mutex<ManagerState>,
    events: broadcast::Sender<ExportEvent>,
}

impl VersionControlManager {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(16);
        Self {
            state: Mutex::new(ManagerState::default()),
            events,
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<ExportEvent> {
        self.events.subscribe()
    }

    fn select_repository(&self) -> anyhow::Result<Value> {
        let selected =
            crate::native_dialog::pick_folder(rfd::FileDialog::new().set_title("选择 Git 项目"));
        let Some(selected) = selected else {
            return Ok(serde_json::json!({ "cancelled": true }));
        };
        let reader = RepositoryReader::discover(&selected)?;
        let overview = reader.overview()?;
        let id = random_id()?;
        let mut state = self.lock()?;
        cancel_active_export(&mut state);
        state.repository = Some(RepositoryState {
            id: id.clone(),
            reader,
            diffs: HashMap::new(),
            diff_order: VecDeque::new(),
        });
        state.pending_exports.clear();
        Ok(serde_json::json!({ "cancelled": false, "repositoryId": id, "overview": overview }))
    }

    fn close_repository(&self, repository_id: &str) -> anyhow::Result<Value> {
        let mut state = self.lock()?;
        self.ensure_repository(&state, repository_id)?;
        cancel_active_export(&mut state);
        state.repository = None;
        state.pending_exports.clear();
        Ok(serde_json::json!({ "closed": true }))
    }

    fn overview(&self, repository_id: &str) -> anyhow::Result<Value> {
        let state = self.lock()?;
        let repository = self.ensure_repository(&state, repository_id)?;
        Ok(serde_json::to_value(repository.reader.overview()?)?)
    }

    fn history(
        &self,
        repository_id: &str,
        query: Option<&str>,
        skip: usize,
        limit: usize,
    ) -> anyhow::Result<Value> {
        let state = self.lock()?;
        let repository = self.ensure_repository(&state, repository_id)?;
        let limit = limit.clamp(1, MAX_HISTORY_PAGE);
        let mut commits = repository.reader.history(query, skip, limit + 1)?;
        let has_more = commits.len() > limit;
        commits.truncate(limit);
        let mut has_more = has_more;
        while response_size(
            &serde_json::json!({ "items": commits, "nextSkip": skip + commits.len(), "hasMore": has_more }),
        ) > 60 * 1024
        {
            anyhow::ensure!(
                commits.len() > 1,
                "history item exceeds the control frame budget"
            );
            commits.pop();
            has_more = true;
        }
        Ok(
            serde_json::json!({ "items": commits, "nextSkip": skip + commits.len(), "hasMore": has_more }),
        )
    }

    fn open_diff(
        &self,
        repository_id: &str,
        old: RevisionRef,
        new: RevisionRef,
        group: WorkingTreeGroup,
    ) -> anyhow::Result<Value> {
        let mut state = self.lock()?;
        let repository = self.ensure_repository_mut(&mut state, repository_id)?;
        let diff = Arc::new(repository.reader.create_diff(old, new, group)?);
        let id = random_id()?;
        let summary = diff.summary.clone();
        let total_files = diff.len();
        repository.diffs.insert(id.clone(), diff);
        repository.diff_order.push_back(id.clone());
        while repository.diff_order.len() > MAX_DIFF_SESSIONS {
            if let Some(expired) = repository.diff_order.pop_front() {
                repository.diffs.remove(&expired);
            }
        }
        Ok(serde_json::json!({ "diffId": id, "summary": summary, "totalFiles": total_files }))
    }

    fn diff_files(
        &self,
        repository_id: &str,
        diff_id: &str,
        skip: usize,
        limit: usize,
    ) -> anyhow::Result<Value> {
        let state = self.lock()?;
        let repository = self.ensure_repository(&state, repository_id)?;
        let diff = Arc::clone(
            repository
                .diffs
                .get(diff_id)
                .context("diff session is no longer available")?,
        );
        let limit = limit.clamp(1, MAX_FILE_PAGE);
        let end = skip.saturating_add(limit).min(diff.len());
        let mut items = if skip < end {
            diff.files().skip(skip).take(end - skip).cloned().collect()
        } else {
            Vec::new()
        };
        let mut next = end;
        while response_size(
            &serde_json::json!({ "items": items, "nextSkip": next, "hasMore": next < diff.len() }),
        ) > 60 * 1024
        {
            anyhow::ensure!(
                items.len() > 1,
                "diff file item exceeds the control frame budget"
            );
            items.pop();
            next -= 1;
        }
        Ok(serde_json::json!({ "items": items, "nextSkip": next, "hasMore": next < diff.len() }))
    }

    fn preview(
        &self,
        repository_id: &str,
        diff_id: &str,
        file_id: u32,
        perspective: ConflictPerspective,
    ) -> anyhow::Result<winrisef_version_control::PreviewContent> {
        let state = self.lock()?;
        let repository = self.ensure_repository(&state, repository_id)?;
        let diff = repository
            .diffs
            .get(diff_id)
            .context("diff session is no longer available")?;
        Ok(repository.reader.preview(diff, file_id, perspective)?)
    }

    fn prepare_export(
        &self,
        repository_id: &str,
        diff_id: &str,
        format: ExportFormat,
        layout: ExportLayout,
        file_selection: FileSelection,
    ) -> anyhow::Result<Value> {
        let mut state = self.lock()?;
        let repository = self.ensure_repository(&state, repository_id)?;
        let diff = Arc::clone(
            repository
                .diffs
                .get(diff_id)
                .context("diff session is no longer available")?,
        );
        let options = ExportOptions {
            format,
            layout,
            selected_file_ids: file_selection.resolve(diff.len())?,
        };
        anyhow::ensure!(!options.selected_file_ids.is_empty(), "no files selected");
        let selected_ids = options
            .selected_file_ids
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        anyhow::ensure!(
            selected_ids.len() == options.selected_file_ids.len(),
            "selection contains duplicate file ids"
        );
        let selectable = diff
            .files()
            .filter(|file| selected_ids.contains(&file.file_id))
            .collect::<Vec<_>>();
        anyhow::ensure!(
            selectable.len() == selected_ids.len()
                && selectable
                    .iter()
                    .all(|file| !file.is_binary && !file.export_too_large),
            "selection contains an unsupported file"
        );
        let extension = options.format.extension();
        let path = crate::native_dialog::save_file(
            rfd::FileDialog::new()
                .set_title("导出 Git 对比")
                .set_file_name(format!("git-diff.{extension}"))
                .add_filter(extension.to_uppercase(), &[extension]),
        );
        let Some(path) = path else {
            return Ok(serde_json::json!({ "cancelled": true }));
        };
        let inside_repository = is_inside(path.as_path(), repository.reader.root_path());
        let id = random_id()?;
        state.pending_exports.clear();
        state.pending_exports.insert(
            id.clone(),
            PendingExport {
                id: id.clone(),
                repository_id: repository_id.to_owned(),
                diff,
                path: path.clone(),
                inside_repository,
                options,
            },
        );
        Ok(
            serde_json::json!({ "cancelled": false, "exportTargetId": id, "insideRepository": inside_repository }),
        )
    }

    fn begin_export(&self, target_id: &str, allow_inside: bool) -> anyhow::Result<ExportJob> {
        let mut state = self.lock()?;
        anyhow::ensure!(
            state.active_export.is_none(),
            "another export is already running"
        );
        let pending = state
            .pending_exports
            .remove(target_id)
            .context("export target is no longer available")?;
        anyhow::ensure!(
            !pending.inside_repository || allow_inside,
            "export inside the repository requires confirmation"
        );
        let reader = {
            let repository = self.ensure_repository(&state, &pending.repository_id)?;
            repository.reader.clone()
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        state.active_export = Some(ActiveExport {
            id: pending.id.clone(),
            cancelled: Arc::clone(&cancelled),
        });
        Ok(ExportJob {
            target_id: pending.id,
            reader,
            diff: pending.diff,
            path: pending.path,
            inside_repository: pending.inside_repository,
            options: pending.options,
            cancelled,
        })
    }

    fn cancel_export(&self, target_id: &str) -> anyhow::Result<Value> {
        let mut state = self.lock()?;
        if state.pending_exports.remove(target_id).is_some() {
            return Ok(serde_json::json!({ "cancelled": true }));
        }
        if let Some(active) = &state.active_export
            && active.id == target_id
        {
            active.cancelled.store(true, Ordering::Release);
            return Ok(serde_json::json!({ "cancelled": true }));
        }
        Ok(serde_json::json!({ "cancelled": false }))
    }

    fn finish_export(&self, target_id: &str) {
        if let Ok(mut state) = self.state.lock()
            && state
                .active_export
                .as_ref()
                .is_some_and(|active| active.id == target_id)
        {
            state.active_export = None;
        }
    }

    fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            cancel_active_export(&mut state);
            state.repository = None;
            state.pending_exports.clear();
        }
    }

    fn ensure_repository<'a>(
        &self,
        state: &'a ManagerState,
        id: &str,
    ) -> anyhow::Result<&'a RepositoryState> {
        let repository = state.repository.as_ref().context("no repository is open")?;
        anyhow::ensure!(repository.id == id, "repository authorization is invalid");
        Ok(repository)
    }

    fn ensure_repository_mut<'a>(
        &self,
        state: &'a mut ManagerState,
        id: &str,
    ) -> anyhow::Result<&'a mut RepositoryState> {
        let repository = state.repository.as_mut().context("no repository is open")?;
        anyhow::ensure!(repository.id == id, "repository authorization is invalid");
        Ok(repository)
    }

    fn lock(&self) -> anyhow::Result<std::sync::MutexGuard<'_, ManagerState>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("version-control state is unavailable"))
    }
}

fn cancel_active_export(state: &mut ManagerState) {
    if let Some(active) = &state.active_export {
        active.cancelled.store(true, Ordering::Release);
    }
}

struct SessionGuard(Arc<VersionControlManager>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

struct ExportJob {
    target_id: String,
    reader: RepositoryReader,
    diff: Arc<DiffSession>,
    path: PathBuf,
    inside_repository: bool,
    options: ExportOptions,
    cancelled: Arc<AtomicBool>,
}

pub async fn run_session(
    session: Session,
    authority: TicketAuthority,
    manager: Arc<VersionControlManager>,
) -> anyhow::Result<()> {
    let _guard = SessionGuard(Arc::clone(&manager));
    let (mut send, mut recv) = tokio::time::timeout(AUTH_TIMEOUT, session.accept_bi())
        .await
        .context("timed out waiting for version-control stream")?
        .context("failed to accept version-control stream")?;
    let hello = tokio::time::timeout(AUTH_TIMEOUT, read_frame(&mut recv))
        .await
        .context("timed out waiting for version-control authentication")??
        .context("version-control stream ended before hello")?;
    let BridgeInput::Hello {
        version,
        launch_token,
    } = hello
    else {
        anyhow::bail!("version-control hello is required");
    };
    let token = parse_hex::<16>(&launch_token, "version-control launch token")?;
    let accepted = version == VERSION && authority.consume_launch_token(&token)?;
    write_frame(
        &mut send,
        &BridgeOutput::HelloAck {
            version: VERSION,
            accepted,
            error: (!accepted).then_some("Version or launch token was rejected"),
        },
    )
    .await?;
    anyhow::ensure!(accepted, "version-control authentication was rejected");

    let mut events = manager.subscribe();
    loop {
        tokio::select! {
            input = read_frame(&mut recv) => {
                let Some(input) = input? else { break };
                let (request_id, result, preview) = handle_input(input, Arc::clone(&manager)).await?;
                let output = match result {
                    Ok(value) => BridgeOutput::Response { request_id, ok: true, result: Some(value), error: None },
                    Err(error) => BridgeOutput::Response { request_id, ok: false, result: None, error: Some(error.to_string()) },
                };
                write_frame(&mut send, &output).await?;
                if let Some(content) = preview { write_preview_stream(&session, request_id, content).await?; }
            }
            event = events.recv() => match event {
                Ok(event) => write_frame(&mut send, &event).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    session.close(0, b"version-control bridge closed");
    Ok(())
}

async fn handle_input(
    input: BridgeInput,
    manager: Arc<VersionControlManager>,
) -> anyhow::Result<(
    u32,
    anyhow::Result<Value>,
    Option<winrisef_version_control::PreviewContent>,
)> {
    let (request_id, task) = match input {
        BridgeInput::Hello { .. } => anyhow::bail!("hello cannot be repeated"),
        BridgeInput::SelectRepository { request_id } => (request_id, BlockingTask::Select),
        BridgeInput::CloseRepository {
            request_id,
            repository_id,
        } => (request_id, BlockingTask::Close(repository_id)),
        BridgeInput::GetRepositoryOverview {
            request_id,
            repository_id,
        }
        | BridgeInput::RefreshRepository {
            request_id,
            repository_id,
        } => (request_id, BlockingTask::Overview(repository_id)),
        BridgeInput::GetHistoryPage {
            request_id,
            repository_id,
            query,
            skip,
            limit,
        } => (
            request_id,
            BlockingTask::History(repository_id, query, skip, limit),
        ),
        BridgeInput::OpenDiff {
            request_id,
            repository_id,
            old_revision,
            new_revision,
            group,
        } => (
            request_id,
            BlockingTask::OpenDiff(repository_id, old_revision, new_revision, group),
        ),
        BridgeInput::GetDiffFilesPage {
            request_id,
            repository_id,
            diff_id,
            skip,
            limit,
        } => (
            request_id,
            BlockingTask::Files(repository_id, diff_id, skip, limit),
        ),
        BridgeInput::OpenFilePreview {
            request_id,
            repository_id,
            diff_id,
            file_id,
            perspective,
        } => (
            request_id,
            BlockingTask::Preview(repository_id, diff_id, file_id, perspective),
        ),
        BridgeInput::PrepareExport {
            request_id,
            repository_id,
            diff_id,
            format,
            layout,
            file_selection,
        } => (
            request_id,
            BlockingTask::PrepareExport(repository_id, diff_id, format, layout, file_selection),
        ),
        BridgeInput::ConfirmExport {
            request_id,
            export_target_id,
            allow_inside_repository,
        } => {
            let job = manager.begin_export(&export_target_id, allow_inside_repository);
            match job {
                Ok(job) => {
                    spawn_export(Arc::clone(&manager), job);
                    return Ok((request_id, Ok(serde_json::json!({ "started": true })), None));
                }
                Err(error) => return Ok((request_id, Err(error), None)),
            }
        }
        BridgeInput::CancelExport {
            request_id,
            export_target_id,
        } => (request_id, BlockingTask::CancelExport(export_target_id)),
    };
    let result = tokio::task::spawn_blocking(move || run_task(&manager, task))
        .await
        .context("version-control worker panicked")?;
    match result {
        Ok(TaskResult::Value(value)) => Ok((request_id, Ok(value), None)),
        Ok(TaskResult::Preview(content)) => {
            let metadata = serde_json::json!({ "stream": true, "originalBytes": content.original.len(), "modifiedBytes": content.modified.len() });
            Ok((request_id, Ok(metadata), Some(content)))
        }
        Err(error) => Ok((request_id, Err(error), None)),
    }
}

enum BlockingTask {
    Select,
    Close(String),
    Overview(String),
    History(String, Option<String>, usize, usize),
    OpenDiff(String, RevisionRef, RevisionRef, WorkingTreeGroup),
    Files(String, String, usize, usize),
    Preview(String, String, u32, ConflictPerspective),
    PrepareExport(String, String, ExportFormat, ExportLayout, FileSelection),
    CancelExport(String),
}
enum TaskResult {
    Value(Value),
    Preview(winrisef_version_control::PreviewContent),
}

fn run_task(manager: &VersionControlManager, task: BlockingTask) -> anyhow::Result<TaskResult> {
    let value = match task {
        BlockingTask::Select => manager.select_repository()?,
        BlockingTask::Close(id) => manager.close_repository(&id)?,
        BlockingTask::Overview(id) => manager.overview(&id)?,
        BlockingTask::History(id, query, skip, limit) => {
            manager.history(&id, query.as_deref(), skip, limit)?
        }
        BlockingTask::OpenDiff(id, old, new, group) => manager.open_diff(&id, old, new, group)?,
        BlockingTask::Files(id, diff, skip, limit) => {
            manager.diff_files(&id, &diff, skip, limit)?
        }
        BlockingTask::Preview(id, diff, file, perspective) => {
            return Ok(TaskResult::Preview(manager.preview(
                &id,
                &diff,
                file,
                perspective,
            )?));
        }
        BlockingTask::PrepareExport(id, diff, format, layout, selection) => {
            manager.prepare_export(&id, &diff, format, layout, selection)?
        }
        BlockingTask::CancelExport(id) => manager.cancel_export(&id)?,
    };
    Ok(TaskResult::Value(value))
}

fn spawn_export(manager: Arc<VersionControlManager>, job: ExportJob) {
    tokio::spawn(async move {
        let target_id = job.target_id.clone();
        let event = tokio::task::spawn_blocking(move || run_export(job)).await;
        manager.finish_export(&target_id);
        let event = match event {
            Ok(Ok(event)) => event,
            Ok(Err(error)) => ExportEvent::Failed {
                export_target_id: target_id,
                error: error.to_string(),
            },
            Err(error) => ExportEvent::Failed {
                export_target_id: target_id,
                error: error.to_string(),
            },
        };
        let _ = manager.events.send(event);
    });
}

fn run_export(job: ExportJob) -> anyhow::Result<ExportEvent> {
    let file_name = job
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("git-diff");
    let temp = job
        .path
        .with_file_name(format!(".{file_name}.{}.tmp", job.target_id));
    let result = (|| -> anyhow::Result<()> {
        let file = File::create(&temp)?;
        let mut writer = CancellableWriter {
            inner: BufWriter::new(file),
            cancelled: Arc::clone(&job.cancelled),
        };
        job.reader
            .write_export(&job.diff, &job.options, &mut writer)?;
        writer.flush()?;
        writer.inner.get_ref().sync_all()?;
        anyhow::ensure!(!job.cancelled.load(Ordering::Acquire), "export cancelled");
        winrisef_platform::atomic_replace(&temp, &job.path)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        if job.cancelled.load(Ordering::Acquire) {
            return Ok(ExportEvent::Cancelled {
                export_target_id: job.target_id,
            });
        }
        return Err(error);
    }
    Ok(ExportEvent::Complete {
        export_target_id: job.target_id,
        inside_repository: job.inside_repository,
    })
}

struct CancellableWriter<W> {
    inner: W,
    cancelled: Arc<AtomicBool>,
}
impl<W: Write> Write for CancellableWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "export cancelled",
            ));
        }
        self.inner.write(buffer)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

async fn write_preview_stream(
    session: &Session,
    request_id: u32,
    content: winrisef_version_control::PreviewContent,
) -> anyhow::Result<()> {
    let perspective = content.perspective;
    let original = content.original.into_bytes();
    let modified = content.modified.into_bytes();
    let metadata = serde_json::to_vec(&serde_json::json!({
        "requestId": request_id,
        "originalBytes": original.len(),
        "modifiedBytes": modified.len(),
        "contentType": "text/plain; charset=utf-8",
        "perspective": perspective
    }))?;
    let mut send = session
        .open_uni()
        .await
        .context("failed to open preview stream")?;
    send.write_all(&(metadata.len() as u32).to_be_bytes())
        .await?;
    send.write_all(&metadata).await?;
    send.write_all(&original).await?;
    send.write_all(&modified).await?;
    send.finish().context("failed to finish preview stream")?;
    Ok(())
}

async fn read_frame(recv: &mut RecvStream) -> anyhow::Result<Option<BridgeInput>> {
    let mut prefix = [0_u8; 4];
    let Some(first) = recv.read(&mut prefix[..1]).await? else {
        return Ok(None);
    };
    anyhow::ensure!(first == 1, "invalid version-control frame prefix");
    recv.read_exact(&mut prefix[1..]).await?;
    let len = u32::from_be_bytes(prefix) as usize;
    anyhow::ensure!(
        len > 0 && len <= MAX_FRAME_BYTES,
        "invalid version-control frame length"
    );
    let mut bytes = vec![0_u8; len];
    recv.read_exact(&mut bytes).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

async fn write_frame<T: Serialize>(send: &mut SendStream, value: &T) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_FRAME_BYTES,
        "version-control response is too large"
    );
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

fn random_id() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("failed to create opaque id: {error}"))?;
    Ok(crate::certificate::format_hex(&bytes))
}

fn is_inside(path: &Path, root: &Path) -> bool {
    let parent = path.parent().unwrap_or(path);
    let normalized_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let normalized_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    normalized_parent.starts_with(normalized_root)
}

fn response_size(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}
