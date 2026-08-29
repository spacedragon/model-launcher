use model_launcher_core::{
    EngineLogFramer, LogFilter, LogLevel, LogRecord, LogSource, LogStore, LogStoreLimits, ModelId,
};
use uuid::Uuid;

fn model_id() -> ModelId {
    ModelId::from_uuid(Uuid::from_u128(7))
}

fn record(timestamp_ms: u64, message: &str) -> LogRecord {
    LogRecord {
        timestamp_ms,
        source: LogSource::Application,
        level: LogLevel::Info,
        generation: Some(3),
        model_id: Some(model_id()),
        message: message.to_owned(),
        truncated: false,
    }
}

#[test]
fn records_keep_all_structured_fields() {
    let store = LogStore::new(LogStoreLimits::new(4, 1_024, 4));

    store.append(record(1_725_000_123_456, "ready"));

    assert_eq!(store.snapshot(), vec![record(1_725_000_123_456, "ready")]);
}

#[test]
fn retention_enforces_record_and_utf8_byte_limits() {
    let by_count = LogStore::new(LogStoreLimits::new(2, 1_024, 4));
    by_count.append(record(1, "one"));
    by_count.append(record(2, "two"));
    by_count.append(record(3, "three"));
    assert_eq!(
        by_count
            .snapshot()
            .iter()
            .map(|record| record.message.as_str())
            .collect::<Vec<_>>(),
        ["two", "three"]
    );

    let by_bytes = LogStore::new(LogStoreLimits::new(10, 5, 4));
    by_bytes.append(record(1, "abc"));
    by_bytes.append(record(2, "éé"));
    assert_eq!(by_bytes.snapshot(), vec![record(2, "éé")]);
}

#[test]
fn filters_use_typed_source_and_minimum_level() {
    let store = LogStore::new(LogStoreLimits::new(8, 1_024, 4));
    store.append(record(1, "app info"));
    store.append(LogRecord {
        timestamp_ms: 2,
        source: LogSource::EngineStderr,
        level: LogLevel::Error,
        generation: Some(4),
        model_id: None,
        message: "engine error".into(),
        truncated: false,
    });

    let filtered = store.filtered_snapshot(LogFilter {
        source: Some(LogSource::EngineStderr),
        minimum_level: Some(LogLevel::Warn),
    });

    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].message, "engine error");
}

#[test]
fn export_is_stable_json_lines_in_snapshot_order() {
    let store = LogStore::new(LogStoreLimits::new(4, 1_024, 4));
    store.append(record(2, "second"));
    store.append(record(1, "first"));

    let mut output = Vec::new();
    store.export_json_lines(&mut output).expect("export logs");

    assert_eq!(
        String::from_utf8(output).unwrap(),
        concat!(
            "{\"timestamp_ms\":2,\"source\":\"application\",\"level\":\"info\",\"generation\":3,\"model_id\":\"00000000-0000-0000-0000-000000000007\",\"message\":\"second\",\"truncated\":false}\n",
            "{\"timestamp_ms\":1,\"source\":\"application\",\"level\":\"info\",\"generation\":3,\"model_id\":\"00000000-0000-0000-0000-000000000007\",\"message\":\"first\",\"truncated\":false}\n"
        )
    );
}

#[test]
fn authorization_and_bearer_secrets_are_redacted_before_storage_and_broadcast() {
    let store = LogStore::new(LogStoreLimits::new(8, 4_096, 4));
    let mut subscriber = store.subscribe();
    let secret = "super-secret-token";
    store.append(record(
        1,
        &format!(
            "Authorization : Bearer {secret}\r\naUtHoRiZaTiOn:\tBasic abc123 request Bearer loose-token"
        ),
    ));

    let broadcast = subscriber.try_recv().expect("broadcast record");
    let snapshot = store.snapshot();
    let mut export = Vec::new();
    store.export_json_lines(&mut export).expect("export logs");
    let combined = format!(
        "{broadcast:?}{snapshot:?}{}",
        String::from_utf8(export).unwrap()
    );

    assert!(!combined.contains(secret));
    assert!(!combined.contains("abc123"));
    assert!(!combined.contains("loose-token"));
    assert!(
        snapshot[0]
            .message
            .contains("Authorization : Bearer [REDACTED]")
    );
}

#[test]
fn bounded_broadcast_reports_lagged_record_count() {
    let store = LogStore::new(LogStoreLimits::new(8, 1_024, 2));
    let mut subscriber = store.subscribe();
    for timestamp in 0..5 {
        store.append(record(timestamp, "event"));
    }

    assert_eq!(subscriber.try_recv().unwrap_err().lagged(), Some(3));
}

#[test]
fn engine_bytes_frame_crlf_split_chunks_lossy_utf8_and_eof_partial() {
    let store = LogStore::new(LogStoreLimits::new(16, 4_096, 4));
    let mut stdout = EngineLogFramer::new(
        store.clone(),
        LogSource::EngineStdout,
        LogLevel::Info,
        Some(9),
        Some(model_id()),
        64,
    );
    stdout.push(10, b"hel");
    stdout.push(11, b"lo\r\nworld\nlossy:\xff");
    stdout.finish(12);

    let records = store.snapshot();
    assert_eq!(
        records
            .iter()
            .map(|record| record.message.as_str())
            .collect::<Vec<_>>(),
        ["hello", "world", "lossy:\u{fffd}"]
    );
    assert!(
        records
            .iter()
            .all(|record| record.source == LogSource::EngineStdout)
    );
    assert_eq!(records[0].timestamp_ms, 11);
    assert_eq!(records[2].timestamp_ms, 12);
}

#[test]
fn oversized_lines_are_truncated_and_framing_continues_with_bounded_pending_data() {
    let store = LogStore::new(LogStoreLimits::new(16, 4_096, 4));
    let mut stderr = EngineLogFramer::new(
        store.clone(),
        LogSource::EngineStderr,
        LogLevel::Error,
        None,
        None,
        5,
    );

    stderr.push(1, b"abcdefghij");
    assert!(stderr.pending_len() <= 5);
    stderr.push(2, b"kl\nok\n");

    let records = store.snapshot();
    assert_eq!(records[0].message, "abcde");
    assert!(records[0].truncated);
    assert_eq!(records[1].message, "ok");
    assert!(!records[1].truncated);
}
