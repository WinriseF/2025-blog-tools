pub(crate) mod diff;
mod history;

use std::path::{Path, PathBuf};

use git2::{BranchType, Repository, Status, StatusOptions};
use thiserror::Error;

use crate::{
    ConflictPerspective, DiffSession, ExportOptions, GraphCommit, PreviewContent,
    RepositoryOverview, RevisionRef, WorkingTreeGroup,
};

#[derive(Debug, Error)]
pub enum VcsError {
    #[error("Git repository could not be opened: {0}")]
    Git(#[from] git2::Error),
    #[error("File operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("The selected revision is invalid")]
    InvalidRevision,
    #[error("The selected diff or file is no longer available")]
    MissingDiffFile,
    #[error("The selected file is binary")]
    BinaryFile,
    #[error("The selected file is larger than the allowed limit")]
    FileTooLarge,
    #[error("The selected file is not valid UTF-8 text")]
    InvalidText,
    #[error("No files were selected for export")]
    NoFilesSelected,
    #[error("The selected export contains an unsupported file")]
    UnsupportedExportFile,
    #[error("Git data could not be serialized: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Clone, Debug)]
pub struct RepositoryReader {
    repository_path: PathBuf,
    workdir: Option<PathBuf>,
    root_path: PathBuf,
}

impl RepositoryReader {
    pub fn discover(selected_path: &Path) -> Result<Self, VcsError> {
        let repository = Repository::discover(selected_path)?;
        let repository_path = repository.path().to_path_buf();
        let workdir = repository.workdir().map(Path::to_path_buf);
        let root_path = workdir.clone().unwrap_or_else(|| repository_path.clone());
        Ok(Self {
            repository_path,
            workdir,
            root_path,
        })
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn overview(&self) -> Result<RepositoryOverview, VcsError> {
        let repository = self.open()?;
        let head = repository.head().ok();
        let head_hash = head
            .as_ref()
            .and_then(|reference| reference.peel_to_commit().ok())
            .map(|commit| commit.id().to_string());
        let current_branch = head
            .as_ref()
            .filter(|reference| reference.is_branch())
            .and_then(|reference| reference.shorthand())
            .map(str::to_owned);
        let is_detached_head = head
            .as_ref()
            .is_some_and(|reference| !reference.is_branch());

        let mut upstream_branch = None;
        let mut ahead = 0;
        let mut behind = 0;
        if let Some(branch_name) = current_branch.as_deref()
            && let Ok(branch) = repository.find_branch(branch_name, BranchType::Local)
            && let Ok(upstream) = branch.upstream()
        {
            upstream_branch = upstream.name().ok().flatten().map(str::to_owned);
            if let (Some(local), Some(remote)) = (branch.get().target(), upstream.get().target())
                && let Ok((local_ahead, local_behind)) =
                    repository.graph_ahead_behind(local, remote)
            {
                ahead = local_ahead;
                behind = local_behind;
            }
        }

        let mut has_staged_changes = false;
        let mut has_unstaged_changes = false;
        let mut has_untracked_files = false;
        let mut conflicted_count = 0;
        if !repository.is_bare() {
            let mut options = StatusOptions::new();
            options
                .include_untracked(true)
                .recurse_untracked_dirs(true)
                .renames_head_to_index(true)
                .renames_index_to_workdir(true);
            for entry in repository.statuses(Some(&mut options))?.iter() {
                let status = entry.status();
                has_staged_changes |= is_index_status(status);
                has_unstaged_changes |= is_worktree_status(status);
                has_untracked_files |= status.contains(Status::WT_NEW);
                conflicted_count += usize::from(status.contains(Status::CONFLICTED));
            }
        }

        let mut stash_count = 0;
        let mut stash_repository = self.open()?;
        let _ = stash_repository.stash_foreach(|_, _, _| {
            stash_count += 1;
            true
        });

        Ok(RepositoryOverview {
            display_name: self
                .root_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Git repository")
                .to_owned(),
            current_branch,
            is_detached_head,
            is_bare: repository.is_bare(),
            head_short_hash: head_hash.as_deref().map(short_hash),
            head_hash,
            upstream_branch,
            ahead,
            behind,
            has_staged_changes,
            has_unstaged_changes,
            has_untracked_files,
            conflicted_count,
            stash_count,
        })
    }

    pub fn history(
        &self,
        query: Option<&str>,
        skip: usize,
        limit: usize,
    ) -> Result<Vec<GraphCommit>, VcsError> {
        history::read_history(&self.open()?, query, skip, limit)
    }

    pub fn create_diff(
        &self,
        old_revision: RevisionRef,
        new_revision: RevisionRef,
        group: WorkingTreeGroup,
    ) -> Result<DiffSession, VcsError> {
        diff::create_diff(
            &self.open()?,
            self.workdir.as_deref(),
            old_revision,
            new_revision,
            group,
        )
    }

    pub fn preview(
        &self,
        session: &DiffSession,
        file_id: u32,
        perspective: ConflictPerspective,
    ) -> Result<PreviewContent, VcsError> {
        diff::preview(
            &self.open()?,
            self.workdir.as_deref(),
            session,
            file_id,
            perspective,
        )
    }

    pub fn write_export(
        &self,
        session: &DiffSession,
        options: &ExportOptions,
        writer: impl std::io::Write,
    ) -> Result<(), VcsError> {
        crate::export::write_export(
            &self.open()?,
            self.workdir.as_deref(),
            session,
            options,
            writer,
        )
    }

    fn open(&self) -> Result<Repository, VcsError> {
        Ok(Repository::open(&self.repository_path)?)
    }
}

pub(crate) fn short_hash(hash: &str) -> String {
    hash.chars().take(7).collect()
}

pub(crate) fn is_index_status(status: Status) -> bool {
    status.intersects(
        Status::INDEX_NEW
            | Status::INDEX_MODIFIED
            | Status::INDEX_DELETED
            | Status::INDEX_RENAMED
            | Status::INDEX_TYPECHANGE,
    )
}

pub(crate) fn is_worktree_status(status: Status) -> bool {
    status.intersects(
        Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_RENAMED | Status::WT_TYPECHANGE,
    )
}
