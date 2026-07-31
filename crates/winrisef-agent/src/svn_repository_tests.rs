use super::{
    SOURCE_CACHE_MAX_ITEMS, SVN_HISTORY_FETCH_LIMIT, SourceCache, SvnRevision, looks_binary,
    svn_history_has_more, svn_history_page_limit, svn_revision,
};
use std::sync::Arc;
use winrisef_version_control::RevisionRef;

#[test]
fn svn_history_reserves_one_fetched_entry_for_has_more() {
    assert_eq!(svn_history_page_limit(1), 1);
    assert_eq!(svn_history_page_limit(48), 31);
    assert_eq!(svn_history_page_limit(48) + 1, SVN_HISTORY_FETCH_LIMIT);
    assert!(svn_history_has_more(32, 48));
    assert!(!svn_history_has_more(31, 48));
}

#[test]
fn source_cache_is_bounded_and_refresh_keeps_immutable_revisions() {
    let mut cache = SourceCache::new();
    for index in 0..SOURCE_CACHE_MAX_ITEMS {
        cache.insert(format!("r{index}:other.txt"), Arc::new(vec![0]));
    }
    cache.insert("working:file.txt".to_owned(), Arc::new(vec![1, 2]));
    cache.insert("r10:file.txt".to_owned(), Arc::new(vec![3, 4, 5]));

    assert!(cache.items.len() <= SOURCE_CACHE_MAX_ITEMS);
    let bytes_before_refresh = cache.bytes;
    cache.clear_working();
    assert!(cache.get("working:file.txt").is_none());
    assert_eq!(cache.get("r10:file.txt").as_deref(), Some(&vec![3, 4, 5]));
    assert_eq!(cache.bytes, bytes_before_refresh - 2);
}

#[test]
fn validates_and_normalizes_svn_revisions() {
    assert_eq!(
        svn_revision(&RevisionRef::Empty).unwrap(),
        SvnRevision::Number(0)
    );
    assert_eq!(
        svn_revision(&RevisionRef::Commit {
            oid: "r42".to_owned()
        })
        .unwrap(),
        SvnRevision::Number(42)
    );
    assert_eq!(
        svn_revision(&RevisionRef::WorkingTree).unwrap(),
        SvnRevision::Working
    );
    assert!(
        svn_revision(&RevisionRef::Commit {
            oid: "42".to_owned()
        })
        .is_err()
    );
    assert!(svn_revision(&RevisionRef::Index).is_err());
}

#[test]
fn detects_binary_content_without_rejecting_lossy_text() {
    assert!(looks_binary(b"text\0binary"));
    assert!(!looks_binary("中文".as_bytes()));
    assert!(!looks_binary(&[0xff]));
}
