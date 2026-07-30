use super::{
    SOURCE_CACHE_MAX_ITEMS, SVN_HISTORY_FETCH_LIMIT, SourceCache, svn_history_has_more,
    svn_history_page_limit, text_line_count,
};

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
        cache.insert(format!("r{index}:other.txt"), vec![0]);
    }
    cache.insert("working:file.txt".to_owned(), vec![1, 2]);
    cache.insert("r10:file.txt".to_owned(), vec![3, 4, 5]);

    assert!(cache.items.len() <= SOURCE_CACHE_MAX_ITEMS);
    let bytes_before_refresh = cache.bytes;
    cache.clear_working();
    assert!(cache.get("working:file.txt").is_none());
    assert_eq!(cache.get("r10:file.txt"), Some(vec![3, 4, 5]));
    assert_eq!(cache.bytes, bytes_before_refresh - 2);
}

#[test]
fn counts_untracked_text_lines_like_an_added_file() {
    assert_eq!(text_line_count(b""), 0);
    assert_eq!(text_line_count(b"one"), 1);
    assert_eq!(text_line_count(b"one\ntwo\n"), 2);
    assert_eq!(text_line_count(b"one\ntwo"), 2);
}
