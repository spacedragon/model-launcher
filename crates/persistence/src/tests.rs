//! Deterministic, offline tests: temp SQLite fixture (`test_support`), WAL +
//! FK PRAGMA assertions, migration idempotency, repository round-trips,
//! transaction commit/rollback, the §8 restart-recovery flow, the §9
//! three-table atomic write, and rejection of out-of-vocabulary state tokens
//! (the schema's named `CHECK` constraints fence the state columns).
//!
//! Everything uses `std::path` / `tempfile`, so the suite passes unchanged
//! on Windows and Linux/WSL2.

use chrono::Utc;
use model_serving_domain::error::{DomainError, ErrorCode};
use model_serving_domain::model::{
    Capabilities, FailureClass, Instance, InstanceFailure, InstanceHealth, InstanceState,
    LoadConfig, Model, Operation, OperationError, OperationKind, OperationState, Runtime,
    RuntimeKind,
};

use crate::SqliteStore;
use crate::repos::{
    AuditEvent, AuditKind, AuditRepo, InstanceQuery, InstancesRepo, ModelRoot, ModelRootsRepo,
    ModelsRepo, OperationQuery, OperationsRepo, RuntimeRepo, RuntimeWithProbe, Setting,
    SettingsRepo,
};
use crate::test_support::Fixture;

/// RFC 3339 UTC timestamps, distinct and deterministic.
const T0: &str = "2025-01-01T00:00:00Z";
const T1: &str = "2025-01-02T00:00:00Z";
const T2: &str = "2025-01-03T00:00:00Z";

fn ts(value: &str) -> chrono::DateTime<Utc> {
    value.parse().expect("test timestamps are valid RFC 3339")
}

fn model_fixture(key: &str) -> Model {
    Model {
        id: format!("{key}-id"),
        key: key.to_owned(),
        path: format!("/models/{key}.gguf"),
        artifact_kind: model_serving_domain::model::ArtifactKind::Gguf,
        size_bytes: 1024,
        mtime: ts(T0),
        display_name: Some(format!("{key} display")),
        default_runtime_id: None,
        default_load_config: None,
        metadata: None,
        deleted: false,
    }
}

fn load_config() -> LoadConfig {
    LoadConfig {
        context_length: 4096,
        max_concurrency: None,
        eval_batch_size: None,
        flash_attention: None,
        offload_kv_cache_to_gpu: None,
        n_gpu_layers: None,
        engine_config: None,
    }
}

fn runtime_fixture(id: &str, kind: RuntimeKind) -> Runtime {
    Runtime {
        id: id.to_owned(),
        kind,
        executable_path: format!("/bin/{id}"),
        enabled: true,
        version_text: Some(format!("{id} 0.1.0")),
        capabilities: Capabilities {
            supports_chat_completions: true,
            ..Capabilities::default()
        },
        fixed_args: vec!["--ctx-size".into(), "4096".into()],
    }
}

fn runtime_record(id: &str, kind: RuntimeKind) -> RuntimeWithProbe {
    RuntimeWithProbe {
        runtime: runtime_fixture(id, kind),
        last_probe_ok: Some(true),
        last_probed_at: Some(ts(T1)),
    }
}

/// A valid non-terminal instance in `state` for `model` / `runtime`.
fn non_terminal_instance(
    instance_id: &str,
    model: &Model,
    runtime: &str,
    state: InstanceState,
) -> Instance {
    let mut instance = Instance::new(
        instance_id.to_owned(),
        model.id.clone(),
        runtime.to_owned(),
        load_config(),
    );
    let path = match state {
        InstanceState::Queued => &[InstanceState::Queued] as &[InstanceState],
        InstanceState::Loading => &[InstanceState::Queued, InstanceState::Loading],
        InstanceState::Ready => &[
            InstanceState::Queued,
            InstanceState::Loading,
            InstanceState::Ready,
        ],
        InstanceState::Draining => &[
            InstanceState::Queued,
            InstanceState::Loading,
            InstanceState::Ready,
            InstanceState::Draining,
        ],
        InstanceState::Unloading => &[
            InstanceState::Queued,
            InstanceState::Loading,
            InstanceState::Unloading,
        ],
        _ => panic!("test needs a non-terminal state"),
    };
    for step in path {
        instance = instance.with_state(*step).expect("test path is legal");
    }
    instance
}

/// An instance in the terminal state `state` for `model` / `runtime`.
fn terminal_instance(
    instance_id: &str,
    model: &Model,
    runtime: &str,
    state: InstanceState,
) -> Instance {
    match state {
        InstanceState::Unloaded => Instance::new(
            instance_id.to_owned(),
            model.id.clone(),
            runtime.to_owned(),
            load_config(),
        ),
        InstanceState::Failed => {
            let instance =
                non_terminal_instance(instance_id, model, runtime, InstanceState::Queued);
            instance
                .with_state(InstanceState::Failed)
                .expect("queued -> failed is legal")
        }
        _ => panic!("test needs a terminal state"),
    }
}

/// The `store` half of the §9 atomic write inside an open transaction: the
/// trigger operation, the instance desired-state drive, and the audit event.
async fn drive_desired_state_in_tx(
    tx: &mut sqlx::SqliteTransaction<'static>,
    instance: &Instance,
    operation: &Operation,
    desired: InstanceState,
) {
    // sqlx 0.8 implements `Executor` for `&mut SqliteConnection`, not for
    // `&mut Transaction` (sqlx-core#3857): deref the transaction down to its
    // connection before handing it to the repositories.
    let connection = &mut **tx;
    let at = Utc::now();
    OperationsRepo::create(&mut *connection, operation)
        .await
        .expect("operation insert in tx");
    InstancesRepo::set_desired_state(&mut *connection, &instance.instance_id, desired)
        .await
        .expect("desired state update in tx");
    AuditRepo::append(
        &mut *connection,
        &AuditEvent::new(
            AuditKind::InstanceDesiredChanged,
            Some("instance".into()),
            Some(instance.instance_id.clone()),
            Some(operation.operation_id.clone()),
            serde_json::json!({ "state": desired }),
            at,
        ),
    )
    .await
    .expect("audit append in tx");
}

/// The §9 three-table consistency write in one transaction: operation +
/// instance desired state + audit event.
async fn three_table_write(
    store: &SqliteStore,
    instance: &Instance,
    operation: &Operation,
    desired: InstanceState,
) {
    let mut tx = store.transaction().await.expect("begin tx");
    drive_desired_state_in_tx(&mut tx, instance, operation, desired).await;
    tx.commit().await.expect("commit tx");
}

/// Seed one model, one runtime and one instance in `state`.
async fn seed(store: &SqliteStore, key: &str, instance_id: &str, state: InstanceState) {
    let model = model_fixture(key);
    ModelsRepo::upsert(store.pool(), &model)
        .await
        .expect("seed model");
    RuntimeRepo::upsert(store.pool(), &runtime_record("rt", RuntimeKind::LlamaCpp))
        .await
        .expect("seed runtime");
    let instance = non_terminal_instance(instance_id, &model, "rt", state);
    InstancesRepo::upsert(store.pool(), &instance)
        .await
        .expect("seed instance");
}

/// Assert the storage error carries the expected domain code.
fn assert_code(err: &DomainError, expected: ErrorCode) {
    assert_eq!(err.code, expected, "expected {expected:?}, got {err:?}");
}

#[tokio::test]
async fn pragmas_are_enforced_on_every_connection() {
    let fixture = Fixture::new().await.expect("fixture");
    let pool = fixture.store.pool();

    for _ in 0..3 {
        let mut connection = pool.acquire().await.expect("acquire connection");
        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut *connection)
            .await
            .expect("journal mode query");
        assert_eq!(
            journal, "wal",
            "every pooled connection must be in WAL mode"
        );
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *connection)
            .await
            .expect("foreign keys query");
        assert_eq!(foreign_keys, 1, "foreign keys must be ON");
        let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&mut *connection)
            .await
            .expect("busy timeout query");
        assert_eq!(busy_timeout, 10_000);
        drop(connection);
    }
}

#[tokio::test]
async fn wal_files_exist_and_persist_across_reopen() {
    let fixture = Fixture::new().await.expect("fixture");
    // A commit must happen so the WAL side file is created.
    ModelsRepo::upsert(fixture.store.pool(), &model_fixture("m-wal"))
        .await
        .expect("insert");
    let wal_path = fixture.db_path.with_extension("sqlite-wal");
    assert!(
        wal_path.exists(),
        "WAL side file expected at {}",
        wal_path.display()
    );

    // Reopen the same file in a second store: the database is readable and
    // WAL mode persists (it is a file property).
    let reopened = SqliteStore::open(&fixture.db_path).await.expect("reopen");
    reopened.migrate().await.expect("idempotent migrate");
    let models = ModelsRepo::list(reopened.pool(), false)
        .await
        .expect("list");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].key, "m-wal");
}

#[tokio::test]
async fn migrate_is_idempotent() {
    let fixture = Fixture::new().await.expect("fixture");
    // A second migrate on the same store is a no-op.
    fixture.store.migrate().await.expect("second migrate");
    // ... and so is a migrate on a freshly opened store over the same file.
    let reopened = SqliteStore::open(&fixture.db_path).await.expect("reopen");
    reopened.migrate().await.expect("migrate over existing db");
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(reopened.pool())
            .await
            .expect("migrations table");
    assert_eq!(versions, vec![1], "exactly one applied migration");
}

#[tokio::test]
async fn model_round_trip_and_soft_delete() {
    let model_fixture_with_rt = |key: &str| -> Model {
        let mut m = model_fixture(key);
        m.default_runtime_id = Some("rt-1".into());
        m
    };
    let fixture2 = Fixture::new().await.expect("fixture");
    // Seed the runtime the model's `default_runtime_id` points at so the FK
    // on `models.default_runtime_id` is satisfied.
    RuntimeRepo::upsert(
        fixture2.store.pool(),
        &runtime_record("rt-1", RuntimeKind::LlamaCpp),
    )
    .await
    .expect("seed default runtime");
    let pool = fixture2.store.pool();
    let mut model = model_fixture_with_rt("mistral-7b");
    model.default_load_config = Some(LoadConfig {
        context_length: 8192,
        ..load_config()
    });
    model.metadata = Some(serde_json::json!({ "params": "7.2B" }));

    ModelsRepo::upsert(pool, &model)
        .await
        .expect("upsert model");

    let stored = ModelsRepo::get_by_key(pool, "mistral-7b")
        .await
        .expect("get by key")
        .expect("row exists");
    assert_eq!(stored, model);

    // Soft delete hides the row from the default list but not from the
    // full list; a re-upsert with deleted = false restores it.
    let hidden = ModelsRepo::set_deleted(pool, "mistral-7b", true)
        .await
        .expect("soft delete");
    assert!(hidden);
    let visible = ModelsRepo::list(pool, false).await.expect("visible list");
    assert!(visible.is_empty());
    let all = ModelsRepo::list(pool, true).await.expect("full list");
    assert_eq!(all.len(), 1);
    assert!(all[0].deleted);

    let mut restored = stored.clone();
    restored.deleted = false;
    ModelsRepo::upsert(pool, &restored)
        .await
        .expect("restore upsert");
    let back = ModelsRepo::get_by_key(pool, "mistral-7b")
        .await
        .expect("get")
        .expect("row present");
    assert!(!back.deleted);

    assert!(
        !ModelsRepo::set_deleted(pool, "no-such-key", true)
            .await
            .expect("missing delete")
    );
}

#[tokio::test]
async fn model_root_round_trip_and_path_lookup() {
    let fixture = Fixture::new().await.expect("fixture");
    let pool = fixture.store.pool();
    let mut root = ModelRoot {
        id: "root-a".into(),
        path: "/mnt/models".into(),
        enabled: true,
        last_scan_at: None,
        last_scan_result: None,
    };
    ModelRootsRepo::upsert(pool, &root)
        .await
        .expect("upsert root");

    let found = ModelRootsRepo::get(pool, "root-a")
        .await
        .expect("get")
        .expect("row present");
    assert_eq!(found, root);
    let by_path = ModelRootsRepo::find_by_path(pool, "/mnt/models")
        .await
        .expect("find by path")
        .expect("row present");
    assert_eq!(by_path.id, "root-a");

    root.last_scan_at = Some(ts(T1));
    root.last_scan_result = Some(serde_json::json!({ "new": 0, "removed": 1 }));
    ModelRootsRepo::upsert(pool, &root)
        .await
        .expect("update root");
    let updated = ModelRootsRepo::find_by_path(pool, "/mnt/models")
        .await
        .expect("refind")
        .expect("row present");
    assert_eq!(updated.last_scan_at, Some(ts(T1)));
    assert_eq!(
        updated.last_scan_result,
        Some(serde_json::json!({ "new": 0, "removed": 1 }))
    );

    // Unique path: registering the same path under another id violates the
    // constraint (surfaced as the `InvalidRequest` domain error).
    let duplicate = ModelRoot {
        id: "root-b".into(),
        path: "/mnt/models".into(),
        enabled: false,
        last_scan_at: None,
        last_scan_result: None,
    };
    let err = ModelRootsRepo::upsert(pool, &duplicate)
        .await
        .expect_err("duplicate path must fail");
    assert_code(&err, ErrorCode::InvalidRequest);
    assert!(
        ModelRootsRepo::delete(pool, "root-a")
            .await
            .expect("delete")
    );
    assert!(
        !ModelRootsRepo::delete(pool, "root-a")
            .await
            .expect("delete missing")
    );
}

#[tokio::test]
async fn runtime_round_trip_keeps_probe_columns() {
    let fixture = Fixture::new().await.expect("fixture");
    let pool = fixture.store.pool();
    let record = runtime_record("llama-1", RuntimeKind::LlamaCpp);
    RuntimeRepo::upsert(pool, &record)
        .await
        .expect("upsert runtime");

    let found = RuntimeRepo::get(pool, "llama-1")
        .await
        .expect("get")
        .expect("row present");
    assert_eq!(found, record);

    // A CRUD update (no new probe) preserves the probe columns and the rest
    // of the row.
    let mut updated = record.clone();
    updated.runtime.version_text = Some("llama-1 0.2.0".into());
    RuntimeRepo::upsert(pool, &updated)
        .await
        .expect("update runtime");
    let again = RuntimeRepo::get(pool, "llama-1")
        .await
        .expect("get")
        .expect("row present");
    assert_eq!(again, updated);
    assert_eq!(again.last_probe_ok, Some(true));

    assert_eq!(RuntimeRepo::list(pool).await.expect("list").len(), 1);
    assert!(RuntimeRepo::delete(pool, "llama-1").await.expect("delete"));
    assert!(
        RuntimeRepo::get(pool, "llama-1")
            .await
            .expect("get")
            .is_none()
    );
}

#[tokio::test]
async fn instance_round_trip_and_queries() {
    let fixture = Fixture::new().await.expect("fixture");
    let pool = fixture.store.pool();
    let model = model_fixture("m1");
    ModelsRepo::upsert(pool, &model).await.expect("model");
    RuntimeRepo::upsert(pool, &runtime_record("rt", RuntimeKind::LlamaCpp))
        .await
        .expect("runtime");

    let mut ready = non_terminal_instance("i-ready", &model, "rt", InstanceState::Ready);
    ready.pid = Some(4242);
    ready.port = Some(1234);
    ready.device_ids = vec![1];
    ready.started_at = Some(ts(T1));
    ready.last_used_at = Some(ts(T2));
    ready.active_requests = 3;
    ready.health = Some(InstanceHealth {
        ok: true,
        latency_ms: Some(5),
        checked_at: Some(ts(T1)),
    });
    InstancesRepo::upsert(pool, &ready)
        .await
        .expect("upsert ready");

    let stored = InstancesRepo::get(pool, "i-ready")
        .await
        .expect("get")
        .expect("row present");
    assert_eq!(stored, ready);
    // `upsert` initializes `desired_state` to the actual state; from here on
    // only `set_desired_state` moves it.
    assert_eq!(
        InstancesRepo::desired_state(pool, "i-ready")
            .await
            .expect("desired"),
        Some(InstanceState::Ready)
    );

    // Desired-state drive (schema-fenced).
    InstancesRepo::set_desired_state(pool, "i-ready", InstanceState::Draining)
        .await
        .expect("drive desired");
    assert_eq!(
        InstancesRepo::desired_state(pool, "i-ready")
            .await
            .expect("desired"),
        Some(InstanceState::Draining)
    );

    // Filtered lists.
    let by_state = InstancesRepo::list(
        pool,
        InstanceQuery {
            state: Some(InstanceState::Ready),
            ..Default::default()
        },
    )
    .await
    .expect("by state");
    assert_eq!(by_state.len(), 1);
    let by_model = InstancesRepo::list(
        pool,
        InstanceQuery {
            model_id: Some(model.id.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("by model");
    assert_eq!(by_model.len(), 1);

    // Out-of-vocabulary actual state: the `instance_state_valid` CHECK
    // rejects the write (surfaced as `InvalidStateTransition`).
    let err = sqlx::query("UPDATE instances SET state = 'sideways' WHERE instance_id = ?1")
        .bind("i-ready")
        .execute(pool)
        .await
        .expect_err("CHECK constraint must reject out-of-vocabulary state");
    assert_code(
        &crate::mapping::storage_error(&err, "write state"),
        ErrorCode::InvalidStateTransition,
    );

    // Out-of-vocabulary desired state: the `instance_desired_state_valid`
    // CHECK rejects it too.
    let err = sqlx::query("UPDATE instances SET desired_state = 'sideways' WHERE instance_id = ?1")
        .bind("i-ready")
        .execute(pool)
        .await
        .expect_err("CHECK constraint must reject out-of-vocabulary desired state");
    assert_code(
        &crate::mapping::storage_error(&err, "write desired state"),
        ErrorCode::InvalidStateTransition,
    );
}

#[tokio::test]
async fn write_state_with_failure_round_trips() {
    let fixture = Fixture::new().await.expect("fixture");
    let pool = fixture.store.pool();
    seed(&fixture.store, "m-w", "i-w", InstanceState::Queued).await;

    let failure = InstanceFailure {
        class: FailureClass::ProcessCrash,
        exit_code: Some(137),
        stderr_tail: None,
        message: Some("child exited 137".into()),
    };
    InstancesRepo::write_state(
        pool,
        "i-w",
        InstanceState::Crashed,
        &Some(failure.clone()),
        ts(T2),
    )
    .await
    .expect("write state");

    let stored = InstancesRepo::get(pool, "i-w")
        .await
        .expect("get")
        .expect("row present");
    assert_eq!(stored.state(), InstanceState::Crashed);
    assert_eq!(stored.failure.as_ref(), Some(&failure));
    assert_eq!(
        InstancesRepo::desired_state(pool, "i-w")
            .await
            .expect("desired"),
        Some(InstanceState::Queued),
        "write_state must not touch the stored desired state"
    );
}

#[tokio::test]
async fn foreign_keys_are_enforced() {
    let fixture = Fixture::new().await.expect("fixture");
    let pool = fixture.store.pool();

    RuntimeRepo::upsert(pool, &runtime_record("rt", RuntimeKind::LlamaCpp))
        .await
        .expect("runtime");

    // Instance referencing an unknown model: rejected.
    let orphan = Instance::new("i-orphan", "missing-model", "rt", load_config());
    let err = InstancesRepo::upsert(pool, &orphan)
        .await
        .expect_err("FK must reject unknown model");
    assert_code(&err, ErrorCode::InvalidRequest);

    // Operation referencing an unknown instance: rejected.
    let mut operation = Operation::new("op-orphan", OperationKind::Load);
    operation.instance_id = Some("missing-instance".into());
    let err = OperationsRepo::create(pool, &operation)
        .await
        .expect_err("FK must reject unknown instance");
    assert_code(&err, ErrorCode::InvalidRequest);
}

#[tokio::test]
async fn operation_round_trip_and_terminal_advance() {
    let fixture = Fixture::new().await.expect("fixture");
    let store = &fixture.store;
    seed(store, "m-op", "i-op", InstanceState::Queued).await;
    let pool = store.pool();

    let mut operation = Operation::new("op-1", OperationKind::Load);
    operation.instance_id = Some("i-op".into());
    operation.model_id = Some(model_fixture("m-op").id);
    OperationsRepo::create(pool, &operation)
        .await
        .expect("create");

    let fetched = OperationsRepo::get(pool, "op-1")
        .await
        .expect("get")
        .expect("row present");
    assert_eq!(fetched.state(), OperationState::Queued);
    assert_eq!(fetched.instance_id.as_deref(), Some("i-op"));

    // Advance to a terminal failed state; the row now carries the
    // structured error + finish time.
    let mut failed = fetched.clone();
    failed = failed
        .with_state(OperationState::Running)
        .expect("queued -> running");
    failed = failed
        .with_state(OperationState::Failed)
        .expect("running -> failed");
    failed.finished_at = Some(ts(T2));
    failed.error = Some(OperationError {
        code: ErrorCode::ProcessCrash,
        message: "child exited 137".into(),
    });
    assert!(
        OperationsRepo::advance(pool, &failed)
            .await
            .expect("advance")
    );
    let stored = OperationsRepo::get(pool, "op-1")
        .await
        .expect("get")
        .expect("row present");
    assert_eq!(stored.state(), OperationState::Failed);
    assert!(stored.finished_at.is_some());
    assert!(stored.error.is_some());

    // The filtered list agrees.
    let failures = OperationsRepo::list(
        pool,
        OperationQuery {
            state: Some(OperationState::Failed),
            ..Default::default()
        },
    )
    .await
    .expect("list failed");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].operation_id, "op-1");
}

#[tokio::test]
async fn out_of_vocabulary_operation_state_is_rejected() {
    let fixture = Fixture::new().await.expect("fixture");
    let store = &fixture.store;
    seed(store, "m-x", "i-x", InstanceState::Queued).await;
    let pool = store.pool();
    let operation = Operation::new("op-x", OperationKind::Load);
    OperationsRepo::create(pool, &operation)
        .await
        .expect("create");

    let err = sqlx::query("UPDATE operations SET state = 'exploded' WHERE operation_id = ?1")
        .bind("op-x")
        .execute(pool)
        .await
        .expect_err("CHECK constraint must reject out-of-vocabulary operation state");
    assert_code(
        &crate::mapping::storage_error(&err, "write operation state"),
        ErrorCode::InvalidStateTransition,
    );
}

#[tokio::test]
async fn transaction_rollback_discards_every_table() {
    let fixture = Fixture::new().await.expect("fixture");
    let store = &fixture.store;
    seed(store, "m-tx", "i-tx", InstanceState::Queued).await;
    let instance = InstancesRepo::get(store.pool(), "i-tx")
        .await
        .expect("get")
        .expect("row present");

    let mut operation = Operation::new("op-tx", OperationKind::Load);
    operation.instance_id = Some("i-tx".into());
    operation = operation
        .with_state(OperationState::Running)
        .expect("queued -> running");

    // Three-table write inside a transaction, then rollback.
    let mut tx = store.transaction().await.expect("begin");
    drive_desired_state_in_tx(&mut tx, &instance, &operation, InstanceState::Loading).await;
    tx.rollback().await.expect("rollback");

    // Nothing committed: operation gone, desired state back to the seeded
    // value (upsert initialized desired = queued), audit empty.
    assert!(
        OperationsRepo::get(store.pool(), "op-tx")
            .await
            .expect("get")
            .is_none()
    );
    assert_eq!(
        InstancesRepo::desired_state(store.pool(), "i-tx")
            .await
            .expect("desired"),
        Some(InstanceState::Queued)
    );
    let audit = AuditRepo::list(store.pool(), None).await.expect("audit");
    assert!(audit.is_empty());
}

#[tokio::test]
async fn three_table_write_is_atomic() {
    let fixture = Fixture::new().await.expect("fixture");
    let store = &fixture.store;
    seed(store, "m-a", "i-a", InstanceState::Loading).await;
    let instance = InstancesRepo::get(store.pool(), "i-a")
        .await
        .expect("get")
        .expect("row present");

    let mut operation = Operation::new("op-a", OperationKind::Load);
    operation.instance_id = Some("i-a".into());
    operation = operation
        .with_state(OperationState::Running)
        .expect("queued -> running");

    // §9: operation + instance desired state + audit event in ONE
    // transaction.
    three_table_write(store, &instance, &operation, InstanceState::Ready).await;

    assert_eq!(
        InstancesRepo::desired_state(store.pool(), "i-a")
            .await
            .expect("desired"),
        Some(InstanceState::Ready)
    );
    let op = OperationsRepo::get(store.pool(), "op-a")
        .await
        .expect("op")
        .expect("row present");
    assert_eq!(op.state(), OperationState::Running);
    let audit = AuditRepo::list(store.pool(), None).await.expect("audit");
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].kind, AuditKind::InstanceDesiredChanged);
    assert_eq!(audit[0].operation_id.as_deref(), Some("op-a"));
    assert_eq!(audit[0].subject_id.as_deref(), Some("i-a"));
    assert!(audit[0].id > 0, "autoincrement event id");

    // And a rollback of the same shape (fresh operation id — `create` is an
    // INSERT, so the committed `op-a` row cannot be re-inserted) leaves zero
    // rows behind (atomicity).
    let mut retry = Operation::new("op-a2", OperationKind::Load);
    retry.instance_id = Some("i-a".into());
    retry = retry
        .with_state(OperationState::Running)
        .expect("queued -> running");
    let mut tx = store.transaction().await.expect("begin");
    drive_desired_state_in_tx(&mut tx, &instance, &retry, InstanceState::Draining).await;
    tx.rollback().await.expect("rollback");
    assert_eq!(
        InstancesRepo::desired_state(store.pool(), "i-a")
            .await
            .expect("desired"),
        Some(InstanceState::Ready),
        "rolled-back drive must not have landed"
    );
    let audit = AuditRepo::list(store.pool(), None).await.expect("audit");
    assert_eq!(
        audit.len(),
        1,
        "rolled-back audit append must not be visible"
    );
}

#[tokio::test]
async fn restart_recovery_marks_non_terminal_instances() {
    let fixture = Fixture::new().await.expect("fixture");
    let store = &fixture.store;
    let model = model_fixture("m-r");
    ModelsRepo::upsert(store.pool(), &model)
        .await
        .expect("seed model");
    RuntimeRepo::upsert(store.pool(), &runtime_record("rt", RuntimeKind::LlamaCpp))
        .await
        .expect("seed runtime");

    // A mix: non-terminal (queued / loading / ready) plus terminal
    // (unloaded, failed) instances.
    for (id, state) in [
        ("i-q", InstanceState::Queued),
        ("i-l", InstanceState::Loading),
        ("i-rdy", InstanceState::Ready),
        ("i-u", InstanceState::Unloaded),
        ("i-f", InstanceState::Failed),
    ] {
        let instance = if state.is_terminal() {
            terminal_instance(id, &model, "rt", state)
        } else {
            non_terminal_instance(id, &model, "rt", state)
        };
        InstancesRepo::upsert(store.pool(), &instance)
            .await
            .expect("seed instance");
    }

    // §8: the restart query returns exactly the non-terminal ones, and the
    // recovery flow marks them crashed (audit event per mark, one tx).
    let found = store
        .recover_non_terminal_instances()
        .await
        .expect("recovery");
    let found_ids: Vec<&str> = found.iter().map(Instance::instance_id).collect();
    // The §8 query orders by `instance_id` (deterministic restart order).
    assert_eq!(found_ids, vec!["i-l", "i-q", "i-rdy"]);

    let after = store
        .recover_non_terminal_instances()
        .await
        .expect("re-query");
    assert!(after.is_empty(), "post-recovery query must be empty");
    for id in ["i-q", "i-l", "i-rdy"] {
        assert_eq!(
            InstancesRepo::get(store.pool(), id)
                .await
                .expect("get")
                .expect("row present")
                .state(),
            InstanceState::Crashed
        );
    }
    // Terminal instances were untouched.
    assert_eq!(
        InstancesRepo::get(store.pool(), "i-u")
            .await
            .expect("get")
            .expect("row present")
            .state(),
        InstanceState::Unloaded
    );
    assert_eq!(
        InstancesRepo::get(store.pool(), "i-f")
            .await
            .expect("get")
            .expect("row present")
            .state(),
        InstanceState::Failed
    );
    let audit = AuditRepo::list(store.pool(), None).await.expect("audit");
    assert_eq!(audit.len(), 3, "one InstanceRecovered audit event per mark");
    assert!(audit.iter().all(|e| e.kind == AuditKind::InstanceRecovered));
    assert_eq!(
        audit[0].payload["previous_state"], "loading",
        "first mark is i-l (instance_id order)"
    );
}

#[tokio::test]
async fn audit_append_and_replay_ordering() {
    let fixture = Fixture::new().await.expect("fixture");
    let pool = fixture.store.pool();
    let events = [
        AuditEvent::new(
            AuditKind::ModelUpserted,
            Some("model".into()),
            Some("m1".into()),
            None,
            serde_json::json!({ "size_bytes": 1 }),
            ts(T0),
        ),
        AuditEvent::new(
            AuditKind::SettingWritten,
            Some("setting".into()),
            Some("default_runtime_id".into()),
            None,
            serde_json::json!({}),
            ts(T1),
        ),
        AuditEvent::new(
            AuditKind::OperationFinished,
            Some("operation".into()),
            Some("op-9".into()),
            Some("op-9".into()),
            serde_json::json!({ "state": "succeeded" }),
            ts(T2),
        ),
    ];
    for event in &events {
        AuditRepo::append(pool, event).await.expect("append");
    }

    // Event ids are monotonically increasing (SSE `Last-Event-ID` replay).
    let all = AuditRepo::list(pool, None).await.expect("list all");
    assert_eq!(all.len(), 3);
    assert!(all.windows(2).all(|w| w[0].id < w[1].id));
    assert_eq!(all[0].id, 1);
    assert_eq!(all[2].kind, AuditKind::OperationFinished);
    assert_eq!(all[2].operation_id.as_deref(), Some("op-9"));

    // Since-filter replay: only events at/after T1.
    let since = AuditRepo::list(pool, Some(ts(T1)))
        .await
        .expect("list since");
    assert_eq!(since.len(), 2);
    assert_eq!(since[0].created_at, ts(T1));

    // Latest-N, oldest first.
    let latest = AuditRepo::latest(pool, 2).await.expect("latest");
    assert_eq!(latest.len(), 2);
    assert_eq!(latest[0].kind, AuditKind::SettingWritten);
    assert_eq!(latest[1].kind, AuditKind::OperationFinished);
}

#[tokio::test]
async fn settings_round_trip() {
    let fixture = Fixture::new().await.expect("fixture");
    let pool = fixture.store.pool();

    assert!(
        SettingsRepo::get(pool, "default_runtime_id")
            .await
            .expect("get")
            .is_none()
    );
    SettingsRepo::set(pool, "default_runtime_id", "rt-1", false)
        .await
        .expect("set");
    assert_eq!(
        SettingsRepo::get(pool, "default_runtime_id")
            .await
            .expect("get"),
        Some("rt-1".into())
    );

    SettingsRepo::set(pool, "default_runtime_id", "rt-2", true)
        .await
        .expect("update");
    assert_eq!(
        SettingsRepo::get(pool, "default_runtime_id")
            .await
            .expect("get"),
        Some("rt-2".into())
    );
    let all = SettingsRepo::list(pool).await.expect("list");
    let setting = all
        .iter()
        .find(|s| s.key == "default_runtime_id")
        .expect("row present");
    assert_eq!(
        *setting,
        Setting {
            key: "default_runtime_id".into(),
            value: "rt-2".into(),
            restart_required: true,
            updated_at: setting.updated_at,
        }
    );
}

#[tokio::test]
async fn corrupt_json_row_surfaces_internal_error_not_panic() {
    let fixture = Fixture::new().await.expect("fixture");
    let store = &fixture.store;
    seed(store, "m-bad", "i-bad", InstanceState::Queued).await;
    let pool = store.pool();

    // Corrupt the structured load-config column out-of-band.
    sqlx::query("UPDATE instances SET load_config = 'not json' WHERE instance_id = ?1")
        .bind("i-bad")
        .execute(pool)
        .await
        .expect("corrupt");

    let err = InstancesRepo::get(pool, "i-bad")
        .await
        .expect_err("decode failure");
    assert!(
        err.code == ErrorCode::Internal,
        "expected Internal, got {err:?}"
    );
    assert!(
        err.message.contains("load_config"),
        "message: {}",
        err.message
    );
}

#[tokio::test]
async fn non_terminal_operations_query_tracks_the_state_machine() {
    let fixture = Fixture::new().await.expect("fixture");
    let store = &fixture.store;
    seed(store, "m-op2", "i-op2", InstanceState::Queued).await;
    let pool = store.pool();

    let queued = Operation::new("op-q", OperationKind::Load);
    let running = Operation::new("op-r", OperationKind::Unload)
        .with_state(OperationState::Running)
        .expect("queued -> running");
    let done = Operation::new("op-d", OperationKind::Rescan)
        .with_state(OperationState::Running)
        .and_then(|o| o.with_state(OperationState::Succeeded))
        .expect("terminal path legal");
    for operation in [queued, running, done] {
        OperationsRepo::create(pool, &operation)
            .await
            .expect("create");
    }

    let pending = OperationsRepo::non_terminal(pool).await.expect("query");
    let ids: Vec<&str> = pending.iter().map(|o| o.operation_id.as_str()).collect();
    assert_eq!(ids, vec!["op-q", "op-r"], "terminal operations excluded");
}
