use serde_json::{Value, json};
use crate::version_control_bridge::{DiffState, RepositoryBackend};

pub(super) fn candidate_json(id: &str, backend: &RepositoryBackend) -> Value {
    match backend {
        RepositoryBackend::Git(reader) => json!({
            "candidateId": id,
            "repositoryKind": "git",
            "displayName": reader.root_path().file_name().and_then(|name| name.to_str()).unwrap_or("Git repository")
        }),
        RepositoryBackend::Svn(repository) => json!({
            "candidateId": id,
            "repositoryKind": "svn",
            "displayName": repository.info.display_name,
            "relativeUrl": repository.info.relative_url
        }),
    }
}

pub(super) fn backend_overview(backend: &RepositoryBackend) -> anyhow::Result<Value> {
    let mut overview = match backend {
        RepositoryBackend::Git(reader) => serde_json::to_value(reader.overview()?)?,
        RepositoryBackend::Svn(repository) => return repository.overview(),
    };
    if let Some(object) = overview.as_object_mut() {
        object.insert("repositoryKind".to_owned(), Value::String("git".to_owned()));
        object.insert("capabilities".to_owned(), json!({ "canExport": true, "supportsStaging": true, "supportsHistory": true }));
    }
    Ok(overview)
}

pub(super) fn diff_len(diff: &DiffState) -> usize {
    match diff {
        DiffState::Git(diff) => diff.len(),
        DiffState::Svn(diff) => diff.files.len(),
    }
}

pub(super) fn diff_files_json(diff: &DiffState, skip: usize, limit: usize) -> Vec<Value> {
    match diff {
        DiffState::Git(diff) => diff.files().skip(skip).take(limit).filter_map(|file| serde_json::to_value(file).ok()).collect(),
        DiffState::Svn(diff) => diff.files.iter().skip(skip).take(limit).filter_map(|file| serde_json::to_value(file).ok()).collect(),
    }
}

pub(super) fn diff_summary(diff: &DiffState) -> (Value, usize) {
    if let DiffState::Git(diff) = diff {
        return (serde_json::to_value(&diff.summary).unwrap_or_default(), diff.len());
    }
    let DiffState::Svn(diff) = diff else { unreachable!() };
    let mut summary = json!({
        "filesChanged": diff.files.len(), "filesAdded": 0, "filesModified": 0,
        "filesDeleted": 0, "filesRenamed": 0, "filesConflicted": 0,
        "insertions": 0, "deletions": 0
    });
    for file in &diff.files {
        let key = match file.status.as_str() {
            "Added" | "Unversioned" => "filesAdded",
            "Deleted" | "Missing" => "filesDeleted",
            "Conflicted" => "filesConflicted",
            "Renamed" | "Copied" => "filesRenamed",
            _ => "filesModified",
        };
        if let Some(value) = summary.get_mut(key).and_then(|value| value.as_u64()) {
            summary[key] = Value::from(value + 1);
        }
    }
    (summary, diff.files.len())
}
