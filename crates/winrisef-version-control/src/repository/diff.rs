use std::{
    cell::RefCell,
    collections::HashMap,
    fs::{self, File},
    io::Read,
    path::Path,
};

use git2::{
    Delta, Diff, DiffFindOptions, DiffOptions, FileMode, Oid, Repository, Status, StatusOptions,
};

use crate::{
    ConflictPerspective, DiffFile, DiffSession, DiffSummary, EXPORT_SIDE_LIMIT, PREVIEW_SIDE_LIMIT,
    PreviewContent, RevisionRef, WorkingTreeGroup,
    models::{ConflictSources, ContentSource, DiffRecord},
};

use super::{VcsError, is_index_status, is_worktree_status};

#[derive(Clone, Copy, PartialEq, Eq)]
enum NewSide {
    Tree,
    Index,
    Workdir,
}

#[derive(Clone, Copy, Default)]
struct LineStats {
    additions: usize,
    deletions: usize,
}

pub(super) fn create_diff(
    repository: &Repository,
    workdir: Option<&Path>,
    old_revision: RevisionRef,
    new_revision: RevisionRef,
    group: WorkingTreeGroup,
) -> Result<DiffSession, VcsError> {
    if group == WorkingTreeGroup::Conflicted {
        return conflict_diff(repository, workdir, old_revision, new_revision);
    }
    if let RevisionRef::Stash { oid } = &new_revision {
        return stash_diff(repository, workdir, old_revision, new_revision.clone(), oid);
    }

    let (diff, new_side) = build_diff(repository, &old_revision, &new_revision, group)?;

    let mut records = records_from_diff(repository, workdir, &diff, new_side)?;
    if group == WorkingTreeGroup::Untracked {
        records.retain(|record| record.public.groups.contains(&WorkingTreeGroup::Untracked));
    } else if group == WorkingTreeGroup::Staged {
        records.retain(|record| record.public.groups.contains(&WorkingTreeGroup::Staged));
    } else if group == WorkingTreeGroup::Unstaged {
        records.retain(|record| record.public.groups.contains(&WorkingTreeGroup::Unstaged));
    }
    Ok(finish_session(old_revision, new_revision, group, records))
}

fn build_diff<'repo>(
    repository: &'repo Repository,
    old_revision: &RevisionRef,
    new_revision: &RevisionRef,
    group: WorkingTreeGroup,
) -> Result<(Diff<'repo>, NewSide), VcsError> {
    let mut options = diff_options();
    let (mut diff, new_side) = match (old_revision, new_revision, group) {
        (_, RevisionRef::WorkingTree, WorkingTreeGroup::Unstaged) => (
            repository.diff_index_to_workdir(None, Some(&mut options))?,
            NewSide::Workdir,
        ),
        (_, RevisionRef::WorkingTree, WorkingTreeGroup::Untracked) => (
            repository.diff_index_to_workdir(None, Some(&mut options))?,
            NewSide::Workdir,
        ),
        (old, RevisionRef::WorkingTree, WorkingTreeGroup::Staged) => {
            let old_tree = revision_tree(repository, old)?;
            (
                repository.diff_tree_to_index(old_tree.as_ref(), None, Some(&mut options))?,
                NewSide::Index,
            )
        }
        (old, RevisionRef::WorkingTree, _) => {
            let old_tree = revision_tree(repository, old)?;
            (
                repository
                    .diff_tree_to_workdir_with_index(old_tree.as_ref(), Some(&mut options))?,
                NewSide::Workdir,
            )
        }
        (old, RevisionRef::Index, _) => {
            let old_tree = revision_tree(repository, old)?;
            (
                repository.diff_tree_to_index(old_tree.as_ref(), None, Some(&mut options))?,
                NewSide::Index,
            )
        }
        (old, new, _) => {
            let old_tree = revision_tree(repository, old)?;
            let new_tree = revision_tree(repository, new)?;
            (
                repository.diff_tree_to_tree(
                    old_tree.as_ref(),
                    new_tree.as_ref(),
                    Some(&mut options),
                )?,
                NewSide::Tree,
            )
        }
    };
    let mut find = DiffFindOptions::new();
    find.renames(true).renames_from_rewrites(true);
    diff.find_similar(Some(&mut find))?;
    Ok((diff, new_side))
}

fn records_from_diff(
    repository: &Repository,
    workdir: Option<&Path>,
    diff: &Diff<'_>,
    new_side: NewSide,
) -> Result<Vec<DiffRecord>, VcsError> {
    let line_stats = collect_line_stats(diff)?;
    let statuses = if new_side == NewSide::Tree {
        HashMap::new()
    } else {
        worktree_statuses(repository)?
    };
    let mut records = Vec::new();
    for delta in diff.deltas() {
        let old_file = delta.old_file();
        let new_file = delta.new_file();
        let path = new_file
            .path()
            .or_else(|| old_file.path())
            .ok_or(VcsError::MissingDiffFile)?
            .to_string_lossy()
            .into_owned();
        let old_path = (delta.status() == Delta::Renamed)
            .then(|| {
                old_file
                    .path()
                    .map(|value| value.to_string_lossy().into_owned())
            })
            .flatten();
        let original = tree_source(old_file.id(), old_file.mode());
        let modified = match new_side {
            NewSide::Workdir if delta.status() != Delta::Deleted => {
                ContentSource::Workdir(path.clone())
            }
            _ => tree_source(new_file.id(), new_file.mode()),
        };
        let (old_size, old_binary) = source_info(repository, workdir, &original)?;
        let (new_size, new_binary) = source_info(repository, workdir, &modified)?;
        let groups = worktree_groups(
            statuses.get(&path).copied().unwrap_or(Status::CURRENT),
            delta.status(),
        );
        let stats = line_stats.get(&path).copied().unwrap_or_default();
        records.push(DiffRecord {
            public: DiffFile {
                file_id: 0,
                path,
                old_path,
                status: delta_status(delta.status()).to_owned(),
                groups,
                additions: stats.additions,
                deletions: stats.deletions,
                is_binary: old_binary || new_binary,
                is_submodule: old_file.mode() == FileMode::Commit
                    || new_file.mode() == FileMode::Commit,
                preview_too_large: old_size > PREVIEW_SIDE_LIMIT || new_size > PREVIEW_SIDE_LIMIT,
                export_too_large: old_size > EXPORT_SIDE_LIMIT || new_size > EXPORT_SIDE_LIMIT,
                has_conflict_views: false,
            },
            original,
            modified,
            conflict: None,
        });
    }
    Ok(records)
}

fn finish_session(
    old_revision: RevisionRef,
    new_revision: RevisionRef,
    group: WorkingTreeGroup,
    mut records: Vec<DiffRecord>,
) -> DiffSession {
    records.sort_by(|left, right| left.public.path.cmp(&right.public.path));
    for (index, record) in records.iter_mut().enumerate() {
        record.public.file_id = index as u32;
    }
    let mut summary = DiffSummary::default();
    for record in &records {
        summary.insertions += record.public.additions;
        summary.deletions += record.public.deletions;
        match record.public.status.as_str() {
            "Added" => summary.files_added += 1,
            "Deleted" => summary.files_deleted += 1,
            "Renamed" => summary.files_renamed += 1,
            "Conflicted" => summary.files_conflicted += 1,
            _ => summary.files_modified += 1,
        }
    }
    summary.files_changed = records.len();
    DiffSession {
        old_revision,
        new_revision,
        group,
        summary,
        records,
    }
}

fn diff_options() -> DiffOptions {
    let mut options = DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .include_typechange(true);
    options
}

fn revision_tree<'repo>(
    repository: &'repo Repository,
    revision: &RevisionRef,
) -> Result<Option<git2::Tree<'repo>>, VcsError> {
    match revision {
        RevisionRef::Empty => Ok(None),
        RevisionRef::Commit { oid } | RevisionRef::Stash { oid } => {
            let oid = Oid::from_str(oid).map_err(|_| VcsError::InvalidRevision)?;
            Ok(Some(repository.find_commit(oid)?.tree()?))
        }
        RevisionRef::WorkingTree | RevisionRef::Index => Err(VcsError::InvalidRevision),
    }
}

fn tree_source(oid: Oid, mode: FileMode) -> ContentSource {
    if oid.is_zero() {
        ContentSource::Empty
    } else if mode == FileMode::Commit {
        ContentSource::Gitlink(oid)
    } else {
        ContentSource::Blob(oid)
    }
}

fn delta_status(status: Delta) -> &'static str {
    match status {
        Delta::Added | Delta::Untracked => "Added",
        Delta::Deleted => "Deleted",
        Delta::Renamed => "Renamed",
        _ => "Modified",
    }
}

fn worktree_statuses(repository: &Repository) -> Result<HashMap<String, Status>, VcsError> {
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true);
    Ok(repository
        .statuses(Some(&mut options))?
        .iter()
        .filter_map(|entry| entry.path().map(|path| (path.to_owned(), entry.status())))
        .collect())
}

fn worktree_groups(status: Status, delta: Delta) -> Vec<WorkingTreeGroup> {
    let mut groups = Vec::new();
    if is_index_status(status) {
        groups.push(WorkingTreeGroup::Staged);
    }
    if status.contains(Status::WT_NEW) || delta == Delta::Untracked {
        groups.push(WorkingTreeGroup::Untracked);
    } else if is_worktree_status(status) {
        groups.push(WorkingTreeGroup::Unstaged);
    }
    if status.contains(Status::CONFLICTED) {
        groups.push(WorkingTreeGroup::Conflicted);
    }
    groups
}

fn collect_line_stats(diff: &Diff<'_>) -> Result<HashMap<String, LineStats>, VcsError> {
    let current_path = RefCell::new(String::new());
    let stats = RefCell::new(HashMap::<String, LineStats>::new());
    diff.foreach(
        &mut |delta, _| {
            let path = delta.new_file().path().or_else(|| delta.old_file().path());
            let mut current = current_path.borrow_mut();
            current.clear();
            if let Some(path) = path {
                current.push_str(&path.to_string_lossy());
                stats.borrow_mut().entry(current.clone()).or_default();
            }
            true
        },
        None,
        None,
        Some(&mut |_delta, _hunk, line| {
            let path = current_path.borrow();
            if let Some(value) = stats.borrow_mut().get_mut(path.as_str()) {
                match line.origin() {
                    '+' => value.additions += 1,
                    '-' => value.deletions += 1,
                    _ => {}
                }
            }
            true
        }),
    )?;
    Ok(stats.into_inner())
}

fn source_info(
    repository: &Repository,
    workdir: Option<&Path>,
    source: &ContentSource,
) -> Result<(usize, bool), VcsError> {
    match source {
        ContentSource::Empty => Ok((0, false)),
        ContentSource::Gitlink(_) => Ok((40, false)),
        ContentSource::Blob(oid) => {
            let blob = repository.find_blob(*oid)?;
            let binary = blob.is_binary()
                || (blob.size() <= EXPORT_SIDE_LIMIT
                    && std::str::from_utf8(blob.content()).is_err());
            Ok((blob.size(), binary))
        }
        ContentSource::Workdir(relative_path) => {
            let root = workdir.ok_or(VcsError::InvalidRevision)?;
            let path = root.join(relative_path);
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(path)?;
                let bytes = target.to_string_lossy();
                return Ok((bytes.len(), false));
            }
            let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
            let binary = if size <= EXPORT_SIDE_LIMIT {
                invalid_workdir_text(&path)?
            } else {
                let mut sample = [0_u8; 8 * 1024];
                let count = File::open(path)?.read(&mut sample)?;
                binary_sample(&sample[..count])
            };
            Ok((size, binary))
        }
    }
}

fn invalid_workdir_text(path: &Path) -> Result<bool, VcsError> {
    let mut file = File::open(path)?;
    let mut chunk = [0_u8; 8 * 1024];
    let mut pending = Vec::with_capacity(chunk.len() + 3);
    loop {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            return Ok(!pending.is_empty());
        }
        if binary_sample(&chunk[..count]) {
            return Ok(true);
        }
        pending.extend_from_slice(&chunk[..count]);
        match std::str::from_utf8(&pending) {
            Ok(_) => pending.clear(),
            Err(error) if error.error_len().is_some() => return Ok(true),
            Err(error) => {
                let tail = pending.split_off(error.valid_up_to());
                pending = tail;
            }
        }
    }
}

fn binary_sample(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

pub(super) fn preview(
    repository: &Repository,
    workdir: Option<&Path>,
    session: &DiffSession,
    file_id: u32,
    perspective: ConflictPerspective,
) -> Result<PreviewContent, VcsError> {
    let record = session
        .records
        .get(file_id as usize)
        .filter(|record| record.public.file_id == file_id)
        .ok_or(VcsError::MissingDiffFile)?;
    if record.public.is_binary {
        return Err(VcsError::BinaryFile);
    }
    if record.public.preview_too_large {
        return Err(VcsError::FileTooLarge);
    }
    let (original, modified) = if let Some(conflict) = &record.conflict {
        match perspective {
            ConflictPerspective::BaseToOurs => (&conflict.base, &conflict.ours),
            ConflictPerspective::BaseToTheirs => (&conflict.base, &conflict.theirs),
            ConflictPerspective::OursToTheirs => (&conflict.ours, &conflict.theirs),
            ConflictPerspective::HeadToWorking => (&conflict.head, &conflict.working),
        }
    } else {
        (&record.original, &record.modified)
    };
    Ok(PreviewContent {
        original: load_text(repository, workdir, original, PREVIEW_SIDE_LIMIT)?,
        modified: load_text(repository, workdir, modified, PREVIEW_SIDE_LIMIT)?,
        perspective,
    })
}

pub(crate) fn load_text(
    repository: &Repository,
    workdir: Option<&Path>,
    source: &ContentSource,
    limit: usize,
) -> Result<String, VcsError> {
    let bytes = match source {
        ContentSource::Empty => Vec::new(),
        ContentSource::Gitlink(oid) => format!("Subproject commit {oid}\n").into_bytes(),
        ContentSource::Blob(oid) => {
            let blob = repository.find_blob(*oid)?;
            if blob.size() > limit {
                return Err(VcsError::FileTooLarge);
            }
            if blob.is_binary() {
                return Err(VcsError::BinaryFile);
            }
            blob.content().to_vec()
        }
        ContentSource::Workdir(relative_path) => {
            let root = workdir.ok_or(VcsError::InvalidRevision)?;
            let path = root.join(relative_path);
            let metadata = fs::symlink_metadata(&path)?;
            if usize::try_from(metadata.len()).unwrap_or(usize::MAX) > limit {
                return Err(VcsError::FileTooLarge);
            }
            if metadata.file_type().is_symlink() {
                fs::read_link(path)?
                    .to_string_lossy()
                    .into_owned()
                    .into_bytes()
            } else {
                fs::read(path)?
            }
        }
    };
    if bytes.len() > limit {
        return Err(VcsError::FileTooLarge);
    }
    if binary_sample(&bytes) {
        return Err(VcsError::BinaryFile);
    }
    let text = String::from_utf8(bytes).map_err(|_| VcsError::InvalidText)?;
    Ok(text.replace("\r\n", "\n"))
}

fn conflict_diff(
    repository: &Repository,
    workdir: Option<&Path>,
    old_revision: RevisionRef,
    new_revision: RevisionRef,
) -> Result<DiffSession, VcsError> {
    let index = repository.index()?;
    let mut records = Vec::new();
    let conflicts = index.conflicts()?;
    for conflict in conflicts {
        let conflict = conflict?;
        let path_bytes = conflict
            .our
            .as_ref()
            .or(conflict.their.as_ref())
            .or(conflict.ancestor.as_ref())
            .map(|entry| entry.path.as_slice())
            .ok_or(VcsError::MissingDiffFile)?;
        let path = String::from_utf8_lossy(path_bytes).into_owned();
        let base = index_entry_source(conflict.ancestor.as_ref());
        let ours = index_entry_source(conflict.our.as_ref());
        let theirs = index_entry_source(conflict.their.as_ref());
        let head = head_source(repository, &path);
        let working = ContentSource::Workdir(path.clone());
        let mut max_preview = false;
        let mut max_export = false;
        let mut binary = false;
        for source in [&base, &ours, &theirs, &head, &working] {
            let (size, is_binary) = source_info(repository, workdir, source)?;
            max_preview |= size > PREVIEW_SIDE_LIMIT;
            max_export |= size > EXPORT_SIDE_LIMIT;
            binary |= is_binary;
        }
        records.push(DiffRecord {
            public: DiffFile {
                file_id: 0,
                path,
                old_path: None,
                status: "Conflicted".to_owned(),
                groups: vec![WorkingTreeGroup::Conflicted],
                additions: 0,
                deletions: 0,
                is_binary: binary,
                is_submodule: false,
                preview_too_large: max_preview,
                export_too_large: max_export,
                has_conflict_views: true,
            },
            original: head.clone(),
            modified: working.clone(),
            conflict: Some(ConflictSources {
                base,
                ours,
                theirs,
                head,
                working,
            }),
        });
    }
    Ok(finish_session(
        old_revision,
        new_revision,
        WorkingTreeGroup::Conflicted,
        records,
    ))
}

pub(crate) struct NativePatchSet<'repo> {
    diffs: Vec<Diff<'repo>>,
    locations: HashMap<String, (usize, usize)>,
}

impl NativePatchSet<'_> {
    pub(crate) fn patch_for(&self, record: &DiffRecord) -> Result<Option<String>, VcsError> {
        let Some(&(diff_index, delta_index)) = self.locations.get(&record.public.path) else {
            return Ok(None);
        };
        let Some(mut patch) = git2::Patch::from_diff(&self.diffs[diff_index], delta_index)? else {
            return Ok(None);
        };
        let bytes = patch.to_buf()?;
        String::from_utf8(bytes.as_ref().to_vec())
            .map(Some)
            .map_err(|_| VcsError::InvalidText)
    }
}

pub(crate) fn native_patches<'repo>(
    repository: &'repo Repository,
    session: &DiffSession,
) -> Result<NativePatchSet<'repo>, VcsError> {
    let mut diffs = Vec::new();
    if let RevisionRef::Stash { oid } = &session.new_revision {
        let stash_oid = Oid::from_str(oid).map_err(|_| VcsError::InvalidRevision)?;
        let stash = repository.find_commit(stash_oid)?;
        if stash.parent_count() > 2 {
            let untracked = RevisionRef::Commit {
                oid: stash.parent_id(2)?.to_string(),
            };
            diffs.push(
                build_diff(
                    repository,
                    &RevisionRef::Empty,
                    &untracked,
                    WorkingTreeGroup::All,
                )?
                .0,
            );
        }
        let base = RevisionRef::Commit {
            oid: stash.parent_id(0)?.to_string(),
        };
        let tracked = RevisionRef::Commit {
            oid: stash_oid.to_string(),
        };
        diffs.push(build_diff(repository, &base, &tracked, WorkingTreeGroup::All)?.0);
    } else if session.group != WorkingTreeGroup::Conflicted {
        diffs.push(
            build_diff(
                repository,
                &session.old_revision,
                &session.new_revision,
                session.group,
            )?
            .0,
        );
    }

    let mut locations = HashMap::new();
    for (diff_index, diff) in diffs.iter().enumerate() {
        for (delta_index, delta) in diff.deltas().enumerate() {
            if let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                locations
                    .entry(path.to_string_lossy().into_owned())
                    .or_insert((diff_index, delta_index));
            }
        }
    }
    Ok(NativePatchSet { diffs, locations })
}

fn index_entry_source(entry: Option<&git2::IndexEntry>) -> ContentSource {
    entry
        .filter(|entry| !entry.id.is_zero())
        .map(|entry| ContentSource::Blob(entry.id))
        .unwrap_or(ContentSource::Empty)
}

fn head_source(repository: &Repository, path: &str) -> ContentSource {
    repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_tree().ok())
        .and_then(|tree| tree.get_path(Path::new(path)).ok().map(|entry| entry.id()))
        .map(ContentSource::Blob)
        .unwrap_or(ContentSource::Empty)
}

fn stash_diff(
    repository: &Repository,
    workdir: Option<&Path>,
    old_revision: RevisionRef,
    new_revision: RevisionRef,
    stash_oid: &str,
) -> Result<DiffSession, VcsError> {
    let stash_oid = Oid::from_str(stash_oid).map_err(|_| VcsError::InvalidRevision)?;
    let stash = repository.find_commit(stash_oid)?;
    let base = stash.parent_id(0)?;
    let mut tracked = create_diff(
        repository,
        workdir,
        RevisionRef::Commit {
            oid: base.to_string(),
        },
        RevisionRef::Commit {
            oid: stash_oid.to_string(),
        },
        WorkingTreeGroup::All,
    )?
    .records;
    if stash.parent_count() > 2 {
        let untracked_oid = stash.parent_id(2)?;
        let untracked = create_diff(
            repository,
            workdir,
            RevisionRef::Empty,
            RevisionRef::Commit {
                oid: untracked_oid.to_string(),
            },
            WorkingTreeGroup::All,
        )?;
        let mut by_path = tracked
            .drain(..)
            .map(|record| (record.public.path.clone(), record))
            .collect::<HashMap<_, _>>();
        for record in untracked.records {
            by_path.insert(record.public.path.clone(), record);
        }
        tracked = by_path.into_values().collect();
    }
    Ok(finish_session(
        old_revision,
        new_revision,
        WorkingTreeGroup::All,
        tracked,
    ))
}
