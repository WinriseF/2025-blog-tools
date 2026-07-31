use super::SegmentTracker;

#[test]
fn segment_tracker_requires_exact_complete_coverage() {
    let tracker = SegmentTracker::new(70, 30).unwrap();
    let first = tracker.reserve(0, 30).unwrap();
    tracker.complete(first, 30).unwrap();
    let second = tracker.reserve(30, 30).unwrap();
    tracker.complete(second, 30).unwrap();
    assert!(!tracker.is_complete());
    let third = tracker.reserve(60, 10).unwrap();
    tracker.complete(third, 10).unwrap();
    assert!(tracker.is_complete());
}

#[test]
fn native_file_fixture_matches_rust_constants() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../protocol-fixtures/native-file-v1.json"
    ))
    .unwrap();
    assert_eq!(fixture["lanSessionVersion"], 12);
    assert_eq!(fixture["bridgeVersion"], 3);
    assert_eq!(fixture["fileVersion"], super::NATIVE_FILE_VERSION);
    assert_eq!(
        fixture["lnaHttp"]["segmentBytes"],
        super::FILE_HTTP_SEGMENT_BYTES
    );
    assert_eq!(
        fixture["lnaHttp"]["parallelism"],
        super::FILE_HTTP_PARALLELISM
    );
    assert_eq!(
        fixture["lnaHttp"]["ioBlockBytes"],
        super::FILE_IO_BLOCK_BYTES
    );
    assert_eq!(
        fixture["webTransport"]["connections"],
        super::FILE_WEBTRANSPORT_CONNECTIONS
    );
    assert_eq!(
        fixture["webTransport"]["lanesPerConnection"],
        super::FILE_WEBTRANSPORT_LANES_PER_CONNECTION
    );
    assert_eq!(
        fixture["webTransport"]["extentBytes"],
        super::FILE_WEBTRANSPORT_EXTENT_BYTES
    );
}
