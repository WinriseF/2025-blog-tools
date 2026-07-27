use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use winrisef_version_control::{ConflictPerspective, PreviewContent, WorkingTreeGroup};

use crate::svn_cli::{SvnCli, SvnDiffSummaryEntry, SvnError, SvnRepositoryInfo, SvnStatusEntry};

const PREVIEW_LIMIT: usize = 2 * 1024 * 1024;
const EXPORT_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub struct SvnRepository {
    pub cli: SvnCli,
    pub info: SvnRepositoryInfo,
    pub history_connected: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SvnDiffFile {
    pub file_id: u32,
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    pub groups: Vec<String>,
    pub additions: usize,
    pub deletions: usize,
    pub is_binary: bool,
    pub is_submodule: bool,
    pub preview_too_large: bool,
    pub export_too_large: bool,
    pub has_conflict_views: bool,
    pub properties_changed: bool,
    #[serde(skip)]
    pub base_revision: Option<u64>,
    #[serde(skip)]
    pub new_revision: Option<u64>,
}

#[derive(Clone)]
pub struct SvnDiffSession {
    pub files: Vec<SvnDiffFile>,
}

impl SvnRepository {
    pub fn discover(selected_path: &Path) -> anyhow::Result<Option<Self>> {
        let cli = match block_on(SvnCli::discover()) {
            Ok(cli) => cli,
            Err(SvnError::NotInstalled) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut cancel = watch::channel(false).1;
        let Some(info) = block_on(cli.discover_working_copy(selected_path, &mut cancel))? else {
            return Ok(None);
        };
        Ok(Some(Self {
            cli,
            info,
            history_connected: false,
        }))
    }

    pub fn overview(&self) -> anyhow::Result<Value> {
        let status = self.status()?;
        let mixed_revision = status
            .iter()
            .filter_map(|entry| entry.revision)
            .any(|revision| revision != self.info.working_revision);
        let conflicted_count = status
            .iter()
            .filter(|entry| entry.tree_conflicted || entry.item == "conflicted")
            .count();
        let has_untracked = status.iter().any(|entry| entry.item == "unversioned");
        let has_changes = status.iter().any(|entry| entry.item != "normal" || entry.props != "none");
        Ok(json!({
            "repositoryKind": "svn",
            "displayName": self.info.display_name,
            "currentBranch": self.info.relative_url,
            "isDetachedHead": false,
            "isBare": false,
            "headHash": format!("r{}", self.info.working_revision),
            "headShortHash": format!("r{}", self.info.working_revision),
            "upstreamBranch": Value::Null,
            "ahead": 0,
            "behind": 0,
            "hasStagedChanges": false,
            "hasUnstagedChanges": has_changes,
            "hasUntrackedFiles": has_untracked,
            "conflictedCount": conflicted_count,
            "stashCount": 0,
            "capabilities": { "canExport": false, "supportsStaging": false, "supportsHistory": true },
            "svn": {
                "relativeUrl": self.info.relative_url,
                "workingRevision": self.info.working_revision,
                "mixedRevision": mixed_revision,
                "depth": self.info.depth,
                "historyConnected": self.history_connected,
                "cliVersion": self.cli.version(),
                "networkRequiredForHistory": !self.history_connected
            }
        }))
    }

    pub fn connect_history(&mut self) -> anyhow::Result<Value> {
        anyhow::ensure!(
            !self.info.repository_root_url.starts_with("svn+ssh://"),
            "SVN svn+ssh:// tunnels are disabled in the read-only bridge"
        );
        let host = repository_host(&self.info.repository_root_url);
        let confirmed = rfd::MessageDialog::new()
            .set_title("连接 SVN 历史")
            .set_description(format!("将读取 SVN 历史：{host}\n\n是否允许本次会话访问网络？"))
            .set_buttons(rfd::MessageButtons::YesNo)
            .show();
        anyhow::ensure!(matches!(confirmed, rfd::MessageDialogResult::Yes), "SVN 历史连接已取消");
        self.history_connected = true;
        Ok(self.overview()?)
    }

    pub fn history(&self, query: Option<&str>, skip: usize, limit: usize) -> anyhow::Result<Value> {
        if !self.history_connected {
            return Ok(json!({ "items": [], "nextSkip": skip, "hasMore": false }));
        }
        let mut cancel = watch::channel(false).1;
        let start_revision = self
            .info
            .working_revision
            .saturating_sub(skip as u64)
            .max(1);
        let entries = block_on(self.cli.log(
            &self.info.root_path.to_string_lossy(),
            start_revision,
            limit.saturating_add(1).clamp(1, 32),
            false,
            &mut cancel,
        ))?;
        let query_terms = query.unwrap_or_default().split_whitespace().map(str::to_lowercase).collect::<Vec<_>>();
        let mut matched = Vec::new();
        for entry in entries {
            let hash = format!("r{}", entry.revision);
            if !query_terms.iter().all(|term| {
                hash.to_lowercase().contains(term)
                    || entry.author.to_lowercase().contains(term)
                    || entry.message.to_lowercase().contains(term)
            }) {
                continue;
            }
            matched.push(json!({
                "hash": hash,
                "shortHash": format!("r{}", entry.revision),
                "author": truncate(&entry.author, 256),
                "timestampMs": parse_timestamp(&entry.date),
                "message": truncate(&entry.message, 1024),
                "parentHashes": if entry.revision > 0 { vec![format!("r{}", entry.revision - 1)] } else { Vec::new() },
                "refs": [],
                "isStash": false
            }));
        }
        let has_more = matched.len() > limit;
        let items = matched.into_iter().take(limit).collect::<Vec<_>>();
        Ok(json!({ "items": items, "nextSkip": skip + items.len(), "hasMore": has_more }))
    }

    pub fn open_diff(&self, old: &Value, new: &Value, group: WorkingTreeGroup) -> anyhow::Result<SvnDiffSession> {
        let old_revision = revision_number(old);
        let new_revision = revision_number(new);
        let mut cancel = watch::channel(false).1;
        let summary = block_on(self.cli.diff_summarize(
            &self.info.root_path.to_string_lossy(),
            old_revision,
            new_revision,
            &mut cancel,
        ))?;
        let statuses = self.status()?.into_iter().map(|entry| (normalize_path(&self.info.root_path, &entry.path), entry)).collect::<HashMap<_, _>>();
        let mut files = summary.into_iter().enumerate().map(|(index, entry)| {
            let key = normalize_path(&self.info.root_path, &entry.path);
            self.file_from_summary(index as u32, entry, statuses.get(&key), old_revision, new_revision)
        }).collect::<anyhow::Result<Vec<_>>>()?;
        if group != WorkingTreeGroup::All {
            let name = match group {
                WorkingTreeGroup::Untracked => "untracked",
                WorkingTreeGroup::Conflicted => "conflicted",
                WorkingTreeGroup::Staged => "staged",
                WorkingTreeGroup::Unstaged => "unstaged",
                WorkingTreeGroup::All => "all",
            };
            files.retain(|file| file.groups.iter().any(|value| value == name));
        }
        Ok(SvnDiffSession { files })
    }

    pub fn preview(&self, session: &SvnDiffSession, file_id: u32, _perspective: ConflictPerspective) -> anyhow::Result<PreviewContent> {
        let file = session.files.get(file_id as usize).context("SVN diff file is no longer available")?;
        anyhow::ensure!(!file.is_binary, "selected SVN file is binary");
        anyhow::ensure!(!file.preview_too_large, "selected SVN file is too large");
        let original = self.read_side(file, false)?;
        let modified = self.read_side(file, true)?;
        Ok(PreviewContent { original, modified, perspective: ConflictPerspective::HeadToWorking })
    }

    fn status(&self) -> anyhow::Result<Vec<SvnStatusEntry>> {
        let mut cancel = watch::channel(false).1;
        Ok(block_on(self.cli.status(&self.info.root_path, &mut cancel))?)
    }

    fn file_from_summary(&self, file_id: u32, entry: SvnDiffSummaryEntry, status: Option<&SvnStatusEntry>, old_revision: Option<u64>, new_revision: Option<u64>) -> anyhow::Result<SvnDiffFile> {
        let path = normalize_path(&self.info.root_path, &entry.path);
        let base_revision = status.and_then(|item| item.revision).or(old_revision);
        let old = self.read_bytes(&path, base_revision)?;
        let new = if new_revision.is_some() { self.read_bytes(&path, new_revision)? } else { self.read_working_bytes(&path)? };
        let old_size = old.as_ref().map_or(0, Vec::len);
        let new_size = new.as_ref().map_or(0, Vec::len);
        let is_binary = old.as_deref().is_some_and(is_binary) || new.as_deref().is_some_and(is_binary);
        let item = status.map(|item| item.item.as_str()).unwrap_or(entry.item.as_str());
        let status = display_status(item, &entry.props);
        let groups = if item == "unversioned" { vec!["all".to_owned(), "untracked".to_owned()] } else if item == "conflicted" || status == "Conflicted" { vec!["all".to_owned(), "conflicted".to_owned()] } else { vec!["all".to_owned(), "unstaged".to_owned()] };
        Ok(SvnDiffFile { file_id, path, old_path: None, status, groups, additions: 0, deletions: 0, is_binary, is_submodule: false, preview_too_large: old_size > PREVIEW_LIMIT || new_size > PREVIEW_LIMIT, export_too_large: old_size > EXPORT_LIMIT || new_size > EXPORT_LIMIT, has_conflict_views: false, properties_changed: entry.props != "none", base_revision, new_revision })
    }

    fn read_side(&self, file: &SvnDiffFile, modified: bool) -> anyhow::Result<String> {
        let bytes = if modified { self.read_bytes(&file.path, file.new_revision)?.or_else(|| self.read_working_bytes(&file.path).ok().flatten()).unwrap_or_default() } else { self.read_bytes(&file.path, file.base_revision)?.unwrap_or_default() };
        Ok(String::from_utf8(bytes).context("SVN file is not valid UTF-8")?.replace("\r\n", "\n"))
    }

    fn read_bytes(&self, path: &str, revision: Option<u64>) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(revision) = revision else { return Ok(None); };
        let mut cancel = watch::channel(false).1;
        match block_on(self.cli.cat(&self.path_for(path).to_string_lossy(), Some(revision), &mut cancel, EXPORT_LIMIT)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(SvnError::CommandFailed { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn read_working_bytes(&self, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let target = self.path_for(path);
        match fs::read(target) { Ok(bytes) => Ok(Some(bytes)), Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None), Err(error) => Err(error.into()) }
    }

    fn path_for(&self, relative: &str) -> PathBuf { self.info.root_path.join(relative) }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output { tokio::runtime::Handle::current().block_on(future) }

fn revision_number(value: &Value) -> Option<u64> { value.get("oid").and_then(Value::as_str).and_then(|value| value.strip_prefix('r')).and_then(|value| value.parse().ok()) }

fn normalize_path(root: &Path, value: &str) -> String {
    let path = PathBuf::from(value);
    path.strip_prefix(root).ok().or_else(|| path.strip_prefix(root.to_string_lossy().as_ref()).ok()).map(|path| path.to_string_lossy().replace('\\', "/")).unwrap_or_else(|| value.replace('\\', "/"))
}

fn display_status(item: &str, props: &str) -> String {
    match item { "added" => "Added", "deleted" => "Deleted", "replaced" => "Replaced", "conflicted" => "Conflicted", "unversioned" => "Unversioned", "missing" => "Missing", "obstructed" => "Obstructed", _ if props != "none" => "Modified", _ => "Modified" }.to_owned()
}

fn is_binary(bytes: &[u8]) -> bool { bytes.contains(&0) || std::str::from_utf8(bytes).is_err() }

fn repository_host(url: &str) -> &str { url.split("//").nth(1).unwrap_or(url).split('/').next().unwrap_or(url) }

fn parse_timestamp(value: &str) -> i64 { time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).map(|date| date.unix_timestamp().saturating_mul(1000)).unwrap_or(0) }

fn truncate(value: &str, limit: usize) -> String { value.chars().take(limit).collect() }
