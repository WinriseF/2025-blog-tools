use std::collections::{HashMap, HashSet};

use git2::{Oid, Reference, Repository, Sort};

use crate::{GitRef, GitRefKind, GraphCommit, VcsError};

use super::short_hash;

pub(super) fn read_history(
    repository: &Repository,
    query: Option<&str>,
    skip: usize,
    limit: usize,
) -> Result<Vec<GraphCommit>, VcsError> {
    let query_terms = query
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    let mut refs_by_oid: HashMap<Oid, Vec<GitRef>> = HashMap::new();
    let mut roots = HashSet::new();
    let mut live_branch_names = HashSet::new();

    if let Ok(head) = repository.head()
        && let Some(oid) = reference_commit_oid(&head)
    {
        let name = head.shorthand().unwrap_or("HEAD").to_owned();
        roots.insert(oid);
        live_branch_names.insert(name.clone());
        add_ref(
            &mut refs_by_oid,
            oid,
            GitRef {
                name,
                kind: GitRefKind::Head,
            },
        );
    }

    if let Ok(references) = repository.references() {
        for reference in references.flatten() {
            let Some(oid) = reference_commit_oid(&reference) else {
                continue;
            };
            let name = reference.shorthand().unwrap_or_default().to_owned();
            if name.is_empty() {
                continue;
            }
            let kind = if reference.is_remote() {
                GitRefKind::RemoteBranch
            } else if reference.is_tag() {
                GitRefKind::Tag
            } else if reference.is_branch() {
                GitRefKind::Branch
            } else {
                continue;
            };
            if kind == GitRefKind::Branch {
                live_branch_names.insert(name.clone());
            }
            roots.insert(oid);
            add_ref(&mut refs_by_oid, oid, GitRef { name, kind });
        }
    }

    let mut stash_repository = Repository::open(repository.path())?;
    let _ = stash_repository.stash_foreach(|index, _, oid| {
        roots.insert(*oid);
        add_ref(
            &mut refs_by_oid,
            *oid,
            GitRef {
                name: format!("stash@{{{index}}}"),
                kind: GitRefKind::Stash,
            },
        );
        true
    });

    add_deleted_branch_hints(repository, &live_branch_names, &mut roots, &mut refs_by_oid);

    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let mut walk = repository.revwalk()?;
    for oid in roots {
        walk.push(oid)?;
    }
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;

    let mut matched = 0;
    let mut result = Vec::with_capacity(limit);
    for oid in walk {
        let oid = oid?;
        let commit = repository.find_commit(oid)?;
        let mut refs = refs_by_oid.remove(&oid).unwrap_or_default();
        refs.sort_by(|left, right| {
            ref_priority(left.kind)
                .cmp(&ref_priority(right.kind))
                .then_with(|| left.name.cmp(&right.name))
        });
        let hash = oid.to_string();
        let author = commit.author().name().unwrap_or("Unknown").to_owned();
        let message = commit.summary().unwrap_or_default().to_owned();
        if !matches_query(&query_terms, &hash, &author, &message, &refs) {
            continue;
        }
        if matched < skip {
            matched += 1;
            continue;
        }
        matched += 1;
        let is_stash = refs.iter().any(|item| item.kind == GitRefKind::Stash);
        refs.truncate(64);
        for item in &mut refs {
            item.name = truncate_text(&item.name, 1024);
        }
        result.push(GraphCommit {
            short_hash: short_hash(&hash),
            hash,
            author: truncate_text(&author, 256),
            timestamp_ms: commit.time().seconds().saturating_mul(1000),
            message: truncate_text(&message, 4096),
            parent_hashes: commit
                .parent_ids()
                .map(|parent| parent.to_string())
                .collect(),
            refs,
            is_stash,
        });
        if result.len() >= limit {
            break;
        }
    }
    Ok(result)
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    value.chars().take(max_chars).collect::<String>() + "…"
}

fn add_deleted_branch_hints(
    repository: &Repository,
    live_names: &HashSet<String>,
    roots: &mut HashSet<Oid>,
    refs_by_oid: &mut HashMap<Oid, Vec<GitRef>>,
) {
    let Ok(reflog) = repository.reflog("HEAD") else {
        return;
    };
    let mut seen = HashSet::new();
    for entry in reflog.iter().take(400) {
        let Some(message) = entry.message() else {
            continue;
        };
        let Some(rest) = message.strip_prefix("checkout: moving from ") else {
            continue;
        };
        let Some((from, to)) = rest.split_once(" to ") else {
            continue;
        };
        for (name, oid) in [(from.trim(), entry.id_old()), (to.trim(), entry.id_new())] {
            if !valid_deleted_branch_name(name)
                || live_names.contains(name)
                || !seen.insert(name.to_owned())
                || repository.find_commit(oid).is_err()
            {
                continue;
            }
            roots.insert(oid);
            add_ref(
                refs_by_oid,
                oid,
                GitRef {
                    name: name.to_owned(),
                    kind: GitRefKind::DeletedBranch,
                },
            );
        }
    }
}

fn reference_commit_oid(reference: &Reference<'_>) -> Option<Oid> {
    reference
        .peel_to_commit()
        .ok()
        .map(|commit| commit.id())
        .or_else(|| reference.target())
}

fn add_ref(map: &mut HashMap<Oid, Vec<GitRef>>, oid: Oid, item: GitRef) {
    let refs = map.entry(oid).or_default();
    if !refs
        .iter()
        .any(|existing| existing.kind == item.kind && existing.name == item.name)
    {
        refs.push(item);
    }
}

fn ref_priority(kind: GitRefKind) -> u8 {
    match kind {
        GitRefKind::Head => 0,
        GitRefKind::Branch => 1,
        GitRefKind::DeletedBranch => 2,
        GitRefKind::Tag => 3,
        GitRefKind::Stash => 4,
        GitRefKind::RemoteBranch => 5,
    }
}

fn matches_query(
    terms: &[String],
    hash: &str,
    author: &str,
    message: &str,
    refs: &[GitRef],
) -> bool {
    if terms.is_empty() {
        return true;
    }
    let mut fields = vec![
        hash.to_lowercase(),
        author.to_lowercase(),
        message.to_lowercase(),
    ];
    fields.extend(refs.iter().map(|item| item.name.to_lowercase()));
    terms
        .iter()
        .all(|term| fields.iter().any(|field| field.contains(term)))
}

fn valid_deleted_branch_name(name: &str) -> bool {
    !name.is_empty()
        && name != "HEAD"
        && !name.starts_with('(')
        && !name.contains("detached")
        && !name.chars().all(|character| character.is_ascii_hexdigit())
        && !name
            .chars()
            .any(|character| matches!(character, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}
