use git2::Oid;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RevisionRef {
    Empty,
    Commit { oid: String },
    Stash { oid: String },
    WorkingTree,
    Index,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WorkingTreeGroup {
    #[default]
    All,
    Staged,
    Unstaged,
    Untracked,
    Conflicted,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPerspective {
    #[default]
    BaseToOurs,
    BaseToTheirs,
    OursToTheirs,
    HeadToWorking,
}

#[derive(Clone, Debug, Serialize)]
pub struct GitRef {
    pub name: String,
    pub kind: GitRefKind,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GitRefKind {
    Head,
    Branch,
    RemoteBranch,
    Tag,
    Stash,
    DeletedBranch,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphCommit {
    pub hash: String,
    pub short_hash: String,
    pub author: String,
    pub timestamp_ms: i64,
    pub message: String,
    pub parent_hashes: Vec<String>,
    pub refs: Vec<GitRef>,
    pub is_stash: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryOverview {
    pub display_name: String,
    pub current_branch: Option<String>,
    pub is_detached_head: bool,
    pub is_bare: bool,
    pub head_hash: Option<String>,
    pub head_short_hash: Option<String>,
    pub upstream_branch: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub has_staged_changes: bool,
    pub has_unstaged_changes: bool,
    pub has_untracked_files: bool,
    pub conflicted_count: usize,
    pub stash_count: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSummary {
    pub files_changed: usize,
    pub files_added: usize,
    pub files_modified: usize,
    pub files_deleted: usize,
    pub files_renamed: usize,
    pub files_conflicted: usize,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffFile {
    pub file_id: u32,
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    pub groups: Vec<WorkingTreeGroup>,
    pub additions: usize,
    pub deletions: usize,
    pub is_binary: bool,
    pub is_submodule: bool,
    pub preview_too_large: bool,
    pub export_too_large: bool,
    pub has_conflict_views: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewContent {
    pub original: String,
    pub modified: String,
    pub perspective: ConflictPerspective,
}

#[derive(Clone, Debug)]
pub(crate) enum ContentSource {
    Empty,
    Blob(Oid),
    Workdir(String),
    Gitlink(Oid),
}

#[derive(Clone, Debug)]
pub(crate) struct ConflictSources {
    pub base: ContentSource,
    pub ours: ContentSource,
    pub theirs: ContentSource,
    pub head: ContentSource,
    pub working: ContentSource,
}

#[derive(Clone, Debug)]
pub(crate) struct DiffRecord {
    pub public: DiffFile,
    pub original: ContentSource,
    pub modified: ContentSource,
    pub conflict: Option<ConflictSources>,
}

#[derive(Clone, Debug)]
pub struct DiffSession {
    pub old_revision: RevisionRef,
    pub new_revision: RevisionRef,
    pub group: WorkingTreeGroup,
    pub summary: DiffSummary,
    pub(crate) records: Vec<DiffRecord>,
}

impl DiffSession {
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn files(&self) -> impl ExactSizeIterator<Item = &DiffFile> {
        self.records.iter().map(|record| &record.public)
    }
}
