use std::{
    fs,
    path::{Path, PathBuf},
};

use git2::{IndexAddOption, Repository, Signature};

use crate::{
    ConflictPerspective, ExportFormat, ExportLayout, ExportOptions, PREVIEW_SIDE_LIMIT,
    RepositoryReader, RevisionRef, WorkingTreeGroup,
};

struct TestRepository {
    path: PathBuf,
}

impl TestRepository {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "winrisef-vc-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("create temporary repository");
        Self { path }
    }

    fn repository(&self) -> Repository {
        Repository::open(&self.path).expect("open repository")
    }

    fn write(&self, relative: &str, bytes: impl AsRef<[u8]>) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create file parent");
        }
        fs::write(path, bytes).expect("write fixture");
    }

    fn commit_all(&self, message: &str) -> String {
        let repository = self.repository();
        let mut index = repository.index().expect("index");
        index
            .add_all(["*"], IndexAddOption::DEFAULT, None)
            .expect("stage files");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repository.find_tree(tree_id).expect("find tree");
        let signature =
            Signature::now("WinriseF Test", "test@winrisef.invalid").expect("signature");
        let parents = repository
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parents.iter().collect::<Vec<_>>();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parent_refs,
            )
            .expect("commit")
            .to_string()
    }
}

impl Drop for TestRepository {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn discovers_subdirectory_and_reads_history_and_worktree_diff() {
    let fixture = TestRepository::new("normal");
    Repository::init(&fixture.path).expect("init repository");
    fixture.write("src/demo.txt", "one\n");
    let head = fixture.commit_all("initial commit");
    fixture.write("src/demo.txt", "one\ntwo\n");

    let reader = RepositoryReader::discover(&fixture.path.join("src")).expect("discover");
    let overview = reader.overview().expect("overview");
    assert_eq!(overview.head_hash.as_deref(), Some(head.as_str()));
    assert!(overview.has_unstaged_changes);
    let history = reader.history(Some("initial"), 0, 10).expect("history");
    assert_eq!(history.len(), 1);

    let diff = reader
        .create_diff(
            RevisionRef::Commit { oid: head },
            RevisionRef::WorkingTree,
            WorkingTreeGroup::All,
        )
        .expect("worktree diff");
    assert_eq!(diff.len(), 1);
    let file_id = diff.files().next().expect("diff file").file_id;
    let preview = reader
        .preview(&diff, file_id, ConflictPerspective::BaseToOurs)
        .expect("preview");
    assert_eq!(preview.original, "one\n");
    assert_eq!(preview.modified, "one\ntwo\n");
}

#[test]
fn separates_staged_unstaged_and_untracked_changes() {
    let fixture = TestRepository::new("groups");
    Repository::init(&fixture.path).expect("init repository");
    fixture.write("tracked.txt", "base\n");
    let head = fixture.commit_all("base");

    fixture.write("tracked.txt", "staged\n");
    let repository = fixture.repository();
    let mut index = repository.index().expect("index");
    index
        .add_path(Path::new("tracked.txt"))
        .expect("stage tracked");
    index.write().expect("write index");
    fixture.write("tracked.txt", "working\n");
    fixture.write("new.txt", "untracked\n");
    let reader = RepositoryReader::discover(&fixture.path).expect("discover");

    for group in [WorkingTreeGroup::Staged, WorkingTreeGroup::Unstaged] {
        let diff = reader
            .create_diff(
                RevisionRef::Commit { oid: head.clone() },
                if group == WorkingTreeGroup::Staged {
                    RevisionRef::Index
                } else {
                    RevisionRef::WorkingTree
                },
                group,
            )
            .expect("group diff");
        assert!(diff.files().any(|file| file.path == "tracked.txt"));
    }
    let untracked = reader
        .create_diff(
            RevisionRef::Empty,
            RevisionRef::WorkingTree,
            WorkingTreeGroup::Untracked,
        )
        .expect("untracked diff");
    assert!(untracked.files().any(|file| file.path == "new.txt"));
}

#[test]
fn marks_preview_limit_and_exports_all_format_layout_pairs() {
    let fixture = TestRepository::new("limits-export");
    Repository::init(&fixture.path).expect("init repository");
    fixture.write("demo.txt", "before\n");
    let head = fixture.commit_all("base");
    fixture.write("demo.txt", "after\n");
    let reader = RepositoryReader::discover(&fixture.path).expect("discover");
    let diff = reader
        .create_diff(
            RevisionRef::Commit { oid: head.clone() },
            RevisionRef::WorkingTree,
            WorkingTreeGroup::All,
        )
        .expect("diff");
    let file_id = diff.files().next().expect("diff file").file_id;
    for format in [
        ExportFormat::Markdown,
        ExportFormat::Json,
        ExportFormat::Xml,
        ExportFormat::Txt,
    ] {
        for layout in [
            ExportLayout::Split,
            ExportLayout::Unified,
            ExportLayout::GitPatch,
        ] {
            let mut output = Vec::new();
            reader
                .write_export(
                    &diff,
                    &ExportOptions {
                        format,
                        layout,
                        selected_file_ids: vec![file_id],
                    },
                    &mut output,
                )
                .expect("export");
            assert!(!output.is_empty());
        }
    }

    fixture.write("demo.txt", vec![b'x'; PREVIEW_SIDE_LIMIT + 1]);
    let large = reader
        .create_diff(
            RevisionRef::Commit { oid: head },
            RevisionRef::WorkingTree,
            WorkingTreeGroup::All,
        )
        .expect("large diff");
    let large_file = large.files().next().expect("large diff file");
    assert!(large_file.preview_too_large);
    assert!(!large_file.export_too_large);
}

#[test]
fn generates_native_rename_patch_only_when_exporting() {
    let fixture = TestRepository::new("rename-export");
    Repository::init(&fixture.path).expect("init repository");
    fixture.write("before.txt", "same content\n");
    let before = fixture.commit_all("before rename");
    fs::rename(
        fixture.path.join("before.txt"),
        fixture.path.join("after.txt"),
    )
    .expect("rename fixture");
    let after = fixture.commit_all("after rename");
    let reader = RepositoryReader::discover(&fixture.path).expect("discover");
    let diff = reader
        .create_diff(
            RevisionRef::Commit { oid: before },
            RevisionRef::Commit { oid: after },
            WorkingTreeGroup::All,
        )
        .expect("rename diff");
    assert_eq!(diff.len(), 1);
    let file = diff.files().next().expect("renamed file");
    assert_eq!(file.status, "Renamed");
    let file_id = file.file_id;

    let mut output = Vec::new();
    reader
        .write_export(
            &diff,
            &ExportOptions {
                format: ExportFormat::Txt,
                layout: ExportLayout::GitPatch,
                selected_file_ids: vec![file_id],
            },
            &mut output,
        )
        .expect("native rename patch");
    let output = String::from_utf8(output).expect("utf-8 export");
    assert!(output.contains("rename from before.txt"));
    assert!(output.contains("rename to after.txt"));
}

#[test]
fn handles_large_diff_as_metadata_without_preview_payload() {
    let fixture = TestRepository::new("large-diff");
    Repository::init(&fixture.path).expect("init repository");
    let before = (0..150_000)
        .map(|line| format!("before-{line:06}-payload\n"))
        .collect::<String>();
    fixture.write("large.txt", before);
    let before = fixture.commit_all("large before");
    let after = (0..150_000)
        .map(|line| format!("after-{line:06}-payload\n"))
        .collect::<String>();
    fixture.write("large.txt", after);
    let after = fixture.commit_all("large after");

    let reader = RepositoryReader::discover(&fixture.path).expect("discover");
    let diff = reader
        .create_diff(
            RevisionRef::Commit { oid: before },
            RevisionRef::Commit { oid: after },
            WorkingTreeGroup::All,
        )
        .expect("large diff");
    assert_eq!(diff.len(), 1);
    assert!(
        diff.files()
            .next()
            .expect("large diff file")
            .preview_too_large
    );
    assert_eq!(diff.summary.insertions, 150_000);
    assert_eq!(diff.summary.deletions, 150_000);
}

#[test]
fn historical_diff_does_not_inherit_worktree_groups() {
    let fixture = TestRepository::new("historical-groups");
    Repository::init(&fixture.path).expect("init repository");
    fixture.write("tracked.txt", "one\n");
    let before = fixture.commit_all("before");
    fixture.write("tracked.txt", "two\n");
    let after = fixture.commit_all("after");
    fixture.write("tracked.txt", "working\n");

    let reader = RepositoryReader::discover(&fixture.path).expect("discover");
    let diff = reader
        .create_diff(
            RevisionRef::Commit { oid: before },
            RevisionRef::Commit { oid: after },
            WorkingTreeGroup::All,
        )
        .expect("historical diff");
    assert!(diff.files().all(|file| file.groups.is_empty()));
}

#[test]
fn opens_bare_repository_without_worktree_state() {
    let fixture = TestRepository::new("bare");
    Repository::init_bare(&fixture.path).expect("init bare repository");
    let reader = RepositoryReader::discover(&fixture.path).expect("discover bare");
    let overview = reader.overview().expect("bare overview");
    assert!(overview.is_bare);
    assert!(!overview.has_staged_changes);
    assert!(!overview.has_unstaged_changes);
}
