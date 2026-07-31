use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Context;
use serde::Serialize;
use serde_json::{Value, json};
use winrisef_version_control::{
    ConflictPerspective, PreviewContent, RevisionRef, WorkingTreeGroup,
};

use crate::svn_cli::{SvnCli, SvnDiffSummaryEntry, SvnError, SvnRepositoryInfo, SvnStatusEntry};
use crate::svn_patch::{SvnPatchCache, SvnPatchSet, added_file_patch};

const PREVIEW_LIMIT: usize = 2 * 1024 * 1024;
const EXPORT_LIMIT: usize = 32 * 1024 * 1024;
const PREVIEW_CACHE_LIMIT: usize = 16 * 1024 * 1024;
const PREVIEW_CACHE_MAX_ITEMS: usize = 8;
const SOURCE_CACHE_LIMIT: usize = 32 * 1024 * 1024;
const SOURCE_CACHE_MAX_ITEMS: usize = 64;
const SVN_HISTORY_FETCH_LIMIT: usize = 32;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

type DiffSummaryCache = HashMap<(Option<u64>, Option<u64>), Vec<SvnDiffSummaryEntry>>;

#[derive(Clone)]
pub struct SvnRepository {
    pub cli: SvnCli,
    pub info: SvnRepositoryInfo,
    pub history_connected: bool,
    pub history_head_revision: Option<u64>,
    status_cache: Arc<Mutex<Option<Vec<SvnStatusEntry>>>>,
    diff_summary_cache: Arc<Mutex<DiffSummaryCache>>,
    patch_cache: Arc<Mutex<SvnPatchCache>>,
    source_cache: Arc<Mutex<SourceCache>>,
}

#[derive(Clone)]
enum SvnContentSource {
    Empty,
    Revision(u64),
    Working,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SvnRevision {
    Number(u64),
    Working,
}

#[derive(Clone, Copy)]
struct SvnDiffRange {
    old_revision: u64,
    new_revision: SvnRevision,
    local_base: bool,
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
    pub node_kind: String,
    #[serde(skip)]
    old_source: SvnContentSource,
    #[serde(skip)]
    new_source: SvnContentSource,
}

#[derive(Clone)]
pub struct SvnDiffSession {
    pub files: Vec<SvnDiffFile>,
    patch: Arc<SvnPatchSet>,
    preview_cache: Arc<Mutex<PreviewCache>>,
}

struct PreviewCache {
    items: HashMap<String, PreviewContent>,
    order: VecDeque<String>,
    bytes: usize,
}

struct SourceCache {
    items: HashMap<String, Vec<u8>>,
    order: VecDeque<String>,
    bytes: usize,
}

impl PreviewCache {
    fn new() -> Self {
        Self {
            items: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    fn get(&mut self, key: &str) -> Option<PreviewContent> {
        let value = self.items.get(key).cloned();
        if value.is_some() {
            self.order.retain(|item| item != key);
            self.order.push_back(key.to_owned());
        }
        value
    }

    fn insert(&mut self, key: String, value: PreviewContent) {
        let size = value.original.len().saturating_add(value.modified.len());
        if size > PREVIEW_CACHE_LIMIT {
            return;
        }
        if let Some(previous) = self.items.remove(&key) {
            self.bytes = self
                .bytes
                .saturating_sub(previous.original.len().saturating_add(previous.modified.len()));
            self.order.retain(|item| item != &key);
        }
        while self.items.len() >= PREVIEW_CACHE_MAX_ITEMS
            || self.bytes.saturating_add(size) > PREVIEW_CACHE_LIMIT
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(previous) = self.items.remove(&oldest) {
                self.bytes = self
                    .bytes
                    .saturating_sub(previous.original.len().saturating_add(previous.modified.len()));
            }
        }
        self.bytes = self.bytes.saturating_add(size);
        self.order.push_back(key.clone());
        self.items.insert(key, value);
    }
}

impl SourceCache {
    fn new() -> Self {
        Self {
            items: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    fn get(&mut self, key: &str) -> Option<Vec<u8>> {
        let value = self.items.get(key).cloned();
        if value.is_some() {
            self.order.retain(|item| item != key);
            self.order.push_back(key.to_owned());
        }
        value
    }

    fn insert(&mut self, key: String, value: Vec<u8>) {
        let size = value.len();
        if size > SOURCE_CACHE_LIMIT {
            return;
        }
        if let Some(previous) = self.items.remove(&key) {
            self.bytes = self.bytes.saturating_sub(previous.len());
            self.order.retain(|item| item != &key);
        }
        while self.items.len() >= SOURCE_CACHE_MAX_ITEMS
            || self.bytes.saturating_add(size) > SOURCE_CACHE_LIMIT
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(previous) = self.items.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(previous.len());
            }
        }
        self.bytes = self.bytes.saturating_add(size);
        self.order.push_back(key.clone());
        self.items.insert(key, value);
    }

    fn clear_working(&mut self) {
        let working = self
            .items
            .keys()
            .filter(|key| key.starts_with("working:"))
            .cloned()
            .collect::<Vec<_>>();
        for key in working {
            if let Some(previous) = self.items.remove(&key) {
                self.bytes = self.bytes.saturating_sub(previous.len());
            }
            self.order.retain(|item| item != &key);
        }
    }
}

impl SvnRepository {
    pub fn discover(selected_path: &Path) -> anyhow::Result<Option<Self>> {
        let cli = match block_on(SvnCli::discover()) {
            Ok(cli) => cli,
            Err(SvnError::NotInstalled) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(info) = block_on(cli.discover_working_copy(selected_path))? else {
            return Ok(None);
        };
        Ok(Some(Self {
            cli,
            info,
            history_connected: false,
            history_head_revision: None,
            status_cache: Arc::new(Mutex::new(None)),
            diff_summary_cache: Arc::new(Mutex::new(HashMap::new())),
            patch_cache: Arc::new(Mutex::new(SvnPatchCache::new())),
            source_cache: Arc::new(Mutex::new(SourceCache::new())),
        }))
    }

    pub fn invalidate_status_cache(&self) {
        if let Ok(mut cache) = self.status_cache.lock() {
            *cache = None;
        }
        if let Ok(mut cache) = self.diff_summary_cache.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.patch_cache.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.source_cache.lock() {
            cache.clear_working();
        }
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
        let has_changes = status.iter().any(status_changed);
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
            .set_description(format!(
                "将读取 SVN 历史：{host}\n\n是否允许本次会话访问网络？"
            ))
            .set_buttons(rfd::MessageButtons::YesNo)
            .show();
        anyhow::ensure!(
            matches!(confirmed, rfd::MessageDialogResult::Yes),
            "SVN 历史连接已取消"
        );
        self.history_head_revision = Some(block_on(self.cli.head_revision(&self.info.root_path))?);
        self.history_connected = true;
        self.overview()
    }

    pub fn history(&self, query: Option<&str>, skip: usize, limit: usize) -> anyhow::Result<Value> {
        if !self.history_connected {
            return Ok(json!({ "items": [], "nextSkip": skip, "hasMore": false }));
        }
        let head_revision = self
            .history_head_revision
            .unwrap_or(self.info.working_revision);
        let start_revision = if skip == 0 {
            head_revision
        } else {
            skip as u64
        }
        .max(1);
        let page_limit = svn_history_page_limit(limit);
        let entries = block_on(self.cli.log(
            &self.info.root_path,
            start_revision,
            page_limit + 1,
        ))?;
        let query_terms = query
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        let has_more = svn_history_has_more(entries.len(), limit);
        let page_entries = entries.into_iter().take(page_limit).collect::<Vec<_>>();
        let next_cursor = page_entries
            .last()
            .map_or(0, |entry| entry.revision.saturating_sub(1));
        let mut matched = Vec::new();
        for entry in page_entries {
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
                "parentHashes": [],
                "refs": [],
                "isStash": false
            }));
        }
        for index in 0..matched.len() {
            let parent = matched
                .get(index + 1)
                .and_then(|item| item.get("hash"))
                .cloned()
                .or_else(|| {
                    (index + 1 == matched.len() && next_cursor > 0)
                        .then(|| json!(format!("r{next_cursor}")))
                });
            if let (Some(parent), Some(object)) = (parent, matched[index].as_object_mut()) {
                object.insert("parentHashes".to_owned(), json!([parent]));
            }
        }
        Ok(
            json!({ "items": matched, "nextSkip": next_cursor, "hasMore": has_more && next_cursor > 0 }),
        )
    }

    pub fn open_diff(
        &self,
        old: &RevisionRef,
        new: &RevisionRef,
        group: WorkingTreeGroup,
    ) -> anyhow::Result<SvnDiffSession> {
        let SvnRevision::Number(old_revision) = svn_revision(old)? else {
            anyhow::bail!("SVN working tree must be the new revision");
        };
        let new_revision = svn_revision(new)?;
        let new_is_working = new_revision == SvnRevision::Working;
        let local_base = new_is_working && old_revision == self.info.working_revision;
        let range = SvnDiffRange {
            old_revision,
            new_revision,
            local_base,
        };
        let command_old_revision = if local_base {
            None
        } else {
            Some(old_revision)
        };
        let command_new_revision = match new_revision {
            SvnRevision::Number(revision) => Some(revision),
            SvnRevision::Working => None,
        };
        let statuses = if new_is_working {
            self.status()?
                .into_iter()
                .map(|entry| (normalize_path(&self.info.root_path, &entry.path), entry))
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
        };
        let summary = self.diff_summary(command_old_revision, command_new_revision)?;
        let patch = self.load_patch(command_old_revision, command_new_revision)?;
        let mut changes = summary
            .into_iter()
            .map(|entry| (normalize_path(&self.info.root_path, &entry.path), entry))
            .collect::<HashMap<_, _>>();
        if new_is_working {
            for (path, status) in &statuses {
                if !status_changed(status) || changes.contains_key(path) {
                    continue;
                }
                changes.insert(
                    path.clone(),
                    SvnDiffSummaryEntry {
                        path: path.clone(),
                        kind: self.node_kind(path),
                        item: status.item.clone(),
                        props: status.props.clone(),
                    },
                );
            }
        }
        let mut changes = changes.into_iter().collect::<Vec<_>>();
        changes.sort_by(|left, right| left.0.cmp(&right.0));
        let mut files = changes
            .into_iter()
            .enumerate()
            .map(|(index, (path, entry))| {
                let status = statuses.get(&path);
                let patch_metadata = patch
                    .file(&path)
                    .map(|file| (file.additions, file.deletions, file.is_binary));
                self.file_from_summary(
                    index as u32,
                    path,
                    entry,
                    status,
                    patch_metadata,
                    range,
                )
            })
            .collect::<Vec<_>>();
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
        for (file_id, file) in files.iter_mut().enumerate() {
            file.file_id = file_id as u32;
        }
        Ok(SvnDiffSession {
            files,
            patch,
            preview_cache: Arc::new(Mutex::new(PreviewCache::new())),
        })
    }

    pub fn preview(
        &self,
        session: &SvnDiffSession,
        file_id: u32,
        perspective: ConflictPerspective,
    ) -> anyhow::Result<PreviewContent> {
        let file = session
            .files
            .get(file_id as usize)
            .context("SVN diff file is no longer available")?;
        anyhow::ensure!(
            file.node_kind != "dir",
            "selected SVN change is a directory"
        );
        anyhow::ensure!(!file.is_binary, "selected SVN file is binary");
        anyhow::ensure!(!file.preview_too_large, "selected SVN file is too large");
        let cache_key = preview_cache_key("full", file_id, perspective);
        if let Ok(mut cache) = session.preview_cache.lock()
            && let Some(content) = cache.get(&cache_key)
        {
            return Ok(content);
        }
        let (original, modified) = block_on(async {
            tokio::try_join!(self.read_side(file, false), self.read_side(file, true))
        })?;
        let content = PreviewContent {
            original,
            modified,
            perspective: ConflictPerspective::HeadToWorking,
        };
        if let Ok(mut cache) = session.preview_cache.lock() {
            cache.insert(cache_key, content.clone());
        }
        Ok(content)
    }

    pub fn patch_preview(
        &self,
        session: &SvnDiffSession,
        file_id: u32,
    ) -> anyhow::Result<PreviewContent> {
        let file = session
            .files
            .get(file_id as usize)
            .context("SVN diff file is no longer available")?;
        anyhow::ensure!(
            file.node_kind != "dir",
            "selected SVN change is a directory"
        );
        anyhow::ensure!(!file.is_binary, "selected SVN file is binary");
        let cache_key = format!("patch:{file_id}");
        if let Ok(mut cache) = session.preview_cache.lock()
            && let Some(content) = cache.get(&cache_key)
        {
            return Ok(content);
        }
        let mut patch = session
            .patch
            .file(&file.path)
            .map(|value| value.patch.clone())
            .unwrap_or_default();
        if patch.is_empty() && file.status == "Unversioned" {
            let bytes = self.read_working_bytes(&file.path)?;
            anyhow::ensure!(!looks_binary(&bytes), "selected SVN file is binary");
            patch = added_file_patch(&file.path, &bytes);
        }
        anyhow::ensure!(
            patch.len() <= PREVIEW_LIMIT,
            "selected SVN patch is too large"
        );
        let content = PreviewContent {
            original: patch,
            modified: String::new(),
            perspective: ConflictPerspective::HeadToWorking,
        };
        if let Ok(mut cache) = session.preview_cache.lock() {
            cache.insert(cache_key, content.clone());
        }
        Ok(content)
    }

    fn status(&self) -> anyhow::Result<Vec<SvnStatusEntry>> {
        if let Ok(cache) = self.status_cache.lock()
            && let Some(entries) = cache.as_ref()
        {
            return Ok(entries.clone());
        }
        let entries = block_on(self.cli.status(&self.info.root_path))?;
        if let Ok(mut cache) = self.status_cache.lock() {
            *cache = Some(entries.clone());
        }
        Ok(entries)
    }

    fn diff_summary(
        &self,
        old_revision: Option<u64>,
        new_revision: Option<u64>,
    ) -> anyhow::Result<Vec<SvnDiffSummaryEntry>> {
        let key = (old_revision, new_revision);
        if let Ok(cache) = self.diff_summary_cache.lock()
            && let Some(entries) = cache.get(&key)
        {
            return Ok(entries.clone());
        }
        let entries = block_on(self.cli.diff_summarize(
            &self.info.root_path,
            old_revision,
            new_revision,
        ))?;
        if let Ok(mut cache) = self.diff_summary_cache.lock() {
            cache.insert(key, entries.clone());
        }
        Ok(entries)
    }

    fn load_patch(
        &self,
        old_revision: Option<u64>,
        new_revision: Option<u64>,
    ) -> anyhow::Result<Arc<SvnPatchSet>> {
        let key = (old_revision, new_revision);
        if let Ok(mut cache) = self.patch_cache.lock()
            && let Some(patch) = cache.get(&key)
        {
            return Ok(patch);
        }
        let bytes = block_on(self.cli.diff_patch(
            &self.info.root_path,
            old_revision,
            new_revision,
        ))?;
        let patch = Arc::new(SvnPatchSet::parse(&bytes, |path| {
            normalize_path(&self.info.root_path, path)
        }));
        if let Ok(mut cache) = self.patch_cache.lock() {
            cache.insert(key, Arc::clone(&patch));
        }
        Ok(patch)
    }

    fn file_from_summary(
        &self,
        file_id: u32,
        path: String,
        entry: SvnDiffSummaryEntry,
        status: Option<&SvnStatusEntry>,
        patch_metadata: Option<(usize, usize, bool)>,
        range: SvnDiffRange,
    ) -> SvnDiffFile {
        let status_item = status.filter(|value| status_changed(value));
        let item = status_item
            .map(|value| value.item.as_str())
            .unwrap_or(entry.item.as_str());
        let properties_changed =
            entry.props != "none" || status_item.is_some_and(|value| value.props != "none");
        let node_kind = if entry.kind == "unknown" {
            self.node_kind(&path)
        } else {
            entry.kind.clone()
        };
        let base_revision = if range.local_base {
            status
                .and_then(|value| value.revision)
                .unwrap_or(range.old_revision)
        } else {
            range.old_revision
        };
        let mut old_source = SvnContentSource::Revision(base_revision);
        let mut new_source = match range.new_revision {
            SvnRevision::Number(revision) => SvnContentSource::Revision(revision),
            SvnRevision::Working => SvnContentSource::Working,
        };
        match item {
            "added" | "unversioned" => old_source = SvnContentSource::Empty,
            "deleted" | "missing" => new_source = SvnContentSource::Empty,
            _ => {}
        }
        let old_size = self.source_size(&path, &old_source);
        let new_size = self.source_size(&path, &new_source);
        let (additions, deletions, patch_is_binary) = patch_metadata.unwrap_or_default();
        let is_binary = patch_is_binary
            || (item == "unversioned" && self.working_file_is_binary(&path));
        let display_status = display_status(item);
        let groups = if item == "unversioned" {
            vec!["all".to_owned(), "untracked".to_owned()]
        } else if item == "conflicted" || status.is_some_and(|value| value.tree_conflicted) {
            vec!["all".to_owned(), "conflicted".to_owned()]
        } else {
            vec!["all".to_owned(), "unstaged".to_owned()]
        };
        SvnDiffFile {
            file_id,
            path,
            old_path: None,
            status: display_status,
            groups,
            additions,
            deletions,
            is_binary,
            is_submodule: false,
            preview_too_large: old_size.is_some_and(|size| size > PREVIEW_LIMIT)
                || new_size.is_some_and(|size| size > PREVIEW_LIMIT),
            export_too_large: old_size.is_some_and(|size| size > EXPORT_LIMIT)
                || new_size.is_some_and(|size| size > EXPORT_LIMIT),
            has_conflict_views: false,
            properties_changed,
            node_kind,
            old_source,
            new_source,
        }
    }

    async fn read_side(&self, file: &SvnDiffFile, modified: bool) -> anyhow::Result<String> {
        let source = if modified {
            &file.new_source
        } else {
            &file.old_source
        };
        let bytes = self
            .read_source(&file.path, source)
            .await?
            .unwrap_or_default();
        anyhow::ensure!(!bytes.contains(&0), "SVN file is binary");
        Ok(String::from_utf8_lossy(&bytes).replace("\r\n", "\n"))
    }

    async fn read_source(
        &self,
        path: &str,
        source: &SvnContentSource,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let cache_key = source_cache_key(path, source);
        if let Some(key) = cache_key.as_deref()
            && let Ok(mut cache) = self.source_cache.lock()
            && let Some(bytes) = cache.get(key)
        {
            return Ok(Some(bytes));
        }
        let bytes = match source {
            SvnContentSource::Empty => None,
            SvnContentSource::Working => Some(self.read_working_bytes(path)?),
            SvnContentSource::Revision(revision) => {
                Some(self.read_revision_bytes(path, *revision).await?)
            }
        };
        if let (Some(key), Some(bytes)) = (cache_key, bytes.as_ref())
            && let Ok(mut cache) = self.source_cache.lock()
        {
            cache.insert(key, bytes.clone());
        }
        Ok(bytes)
    }

    async fn read_revision_bytes(
        &self,
        path: &str,
        revision: u64,
    ) -> anyhow::Result<Vec<u8>> {
        Ok(self
            .cli
            .cat(
                &self.info.root_path,
                path,
                Some(revision),
                PREVIEW_LIMIT,
            )
            .await?)
    }

    fn read_working_bytes(&self, path: &str) -> anyhow::Result<Vec<u8>> {
        let target = self.path_for(path);
        let metadata = fs::symlink_metadata(&target)?;
        if metadata.file_type().is_symlink() {
            let bytes = fs::read_link(target)?
                .to_string_lossy()
                .into_owned()
                .into_bytes();
            anyhow::ensure!(bytes.len() <= PREVIEW_LIMIT, "selected SVN file is too large");
            return Ok(bytes);
        }
        anyhow::ensure!(metadata.is_file(), "selected SVN path is not a file");
        let mut bytes = Vec::new();
        fs::File::open(target)?
            .take(PREVIEW_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= PREVIEW_LIMIT, "selected SVN file is too large");
        Ok(bytes)
    }

    fn node_kind(&self, relative: &str) -> String {
        self.path_for(relative)
            .symlink_metadata()
            .ok()
            .map(|metadata| {
                if metadata.file_type().is_symlink() {
                    "symlink"
                } else if metadata.is_dir() {
                    "dir"
                } else {
                    "file"
                }
            })
            .unwrap_or("file")
            .to_owned()
    }

    fn source_size(&self, path: &str, source: &SvnContentSource) -> Option<usize> {
        match source {
            SvnContentSource::Empty => Some(0),
            SvnContentSource::Revision(_) => None,
            SvnContentSource::Working => {
                let target = self.path_for(path);
                let metadata = fs::symlink_metadata(&target).ok()?;
                if metadata.file_type().is_symlink() {
                    fs::read_link(target)
                        .ok()
                        .map(|link| link.to_string_lossy().len())
                } else if metadata.is_file() {
                    Some(usize::try_from(metadata.len()).unwrap_or(usize::MAX))
                } else {
                    Some(0)
                }
            }
        }
    }

    fn working_file_is_binary(&self, path: &str) -> bool {
        let target = self.path_for(path);
        let Ok(metadata) = fs::symlink_metadata(&target) else {
            return false;
        };
        if !metadata.file_type().is_file() {
            return false;
        }
        let Ok(mut file) = fs::File::open(target) else {
            return false;
        };
        let mut sample = [0; BINARY_SNIFF_BYTES];
        file.read(&mut sample)
            .is_ok_and(|count| looks_binary(&sample[..count]))
    }

    fn path_for(&self, relative: &str) -> PathBuf {
        self.info.root_path.join(relative)
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Handle::current().block_on(future)
}

fn svn_revision(value: &RevisionRef) -> anyhow::Result<SvnRevision> {
    match value {
        RevisionRef::Empty => Ok(SvnRevision::Number(0)),
        RevisionRef::Commit { oid } => {
            let revision = oid
                .strip_prefix('r')
                .and_then(|value| value.parse::<u64>().ok())
                .context("invalid SVN revision")?;
            Ok(SvnRevision::Number(revision))
        }
        RevisionRef::WorkingTree => Ok(SvnRevision::Working),
        RevisionRef::Stash { .. } | RevisionRef::Index => {
            anyhow::bail!("revision kind is not supported by SVN")
        }
    }
}

fn status_changed(value: &SvnStatusEntry) -> bool {
    value.item != "normal" || value.props != "none" || value.tree_conflicted
}

fn normalize_path(root: &Path, value: &str) -> String {
    let normalized = value.replace('\\', "/");
    if normalized == "." {
        return ".".to_owned();
    }
    let root = root
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned();
    normalized
        .strip_prefix(&root)
        .map(|path| path.trim_start_matches('/').to_owned())
        .unwrap_or(normalized)
}

fn display_status(item: &str) -> String {
    match item {
        "added" => "Added",
        "deleted" => "Deleted",
        "replaced" => "Replaced",
        "conflicted" => "Conflicted",
        "unversioned" => "Unversioned",
        "missing" => "Missing",
        "obstructed" => "Obstructed",
        _ => "Modified",
    }
    .to_owned()
}

fn repository_host(url: &str) -> &str {
    url.split("//")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
}

fn parse_timestamp(value: &str) -> i64 {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map(|date| date.unix_timestamp().saturating_mul(1000))
        .unwrap_or(0)
}

fn preview_cache_key(mode: &str, file_id: u32, perspective: ConflictPerspective) -> String {
    let perspective = match perspective {
        ConflictPerspective::BaseToOurs => "base-to-ours",
        ConflictPerspective::BaseToTheirs => "base-to-theirs",
        ConflictPerspective::OursToTheirs => "ours-to-theirs",
        ConflictPerspective::HeadToWorking => "head-to-working",
    };
    format!("{mode}:{file_id}:{perspective}")
}

fn source_cache_key(path: &str, source: &SvnContentSource) -> Option<String> {
    match source {
        SvnContentSource::Empty => None,
        SvnContentSource::Working => Some(format!("working:{path}")),
        SvnContentSource::Revision(revision) => Some(format!("r{revision}:{path}")),
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn svn_history_page_limit(requested: usize) -> usize {
    requested.clamp(1, SVN_HISTORY_FETCH_LIMIT - 1)
}

fn svn_history_has_more(fetched: usize, requested: usize) -> bool {
    fetched > svn_history_page_limit(requested)
}

#[cfg(test)]
#[path = "svn_repository_tests.rs"]
mod tests;
