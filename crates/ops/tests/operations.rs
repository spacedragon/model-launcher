use std::sync::Arc;

use chrono::{TimeZone, Utc};
use model_serving_domain::error::ErrorCode;
use model_serving_domain::model::{
    ArtifactKind, Capabilities, FailureClass, Instance, InstanceFailure, InstanceState, LoadConfig,
    Model, OperationError, OperationState, Runtime, RuntimeKind,
};
use model_serving_ops::{OperationManager, ReadyProcess};
use model_serving_persistence::test_support::Fixture;
use model_serving_persistence::{
    InstancesRepo, ModelsRepo, OperationsRepo, RuntimeRepo, RuntimeWithProbe, SqliteStore,
};

fn model() -> Model {
    Model {
        id: "model-1".into(),
        key: "model-one".into(),
        path: "/models/one.gguf".into(),
        artifact_kind: ArtifactKind::Gguf,
        size_bytes: 1,
        mtime: Utc
            .timestamp_opt(1_700_000_000, 0)
            .single()
            .expect("timestamp"),
        display_name: None,
        default_runtime_id: Some("runtime-1".into()),
        default_load_config: None,
        metadata: None,
        deleted: false,
    }
}

fn runtime() -> RuntimeWithProbe {
    RuntimeWithProbe {
        runtime: Runtime {
            id: "runtime-1".into(),
            kind: RuntimeKind::LlamaCpp,
            executable_path: "/bin/true".into(),
            enabled: true,
            version_text: Some("test".into()),
            capabilities: Capabilities::default(),
            fixed_args: Vec::new(),
        },
        last_probe_ok: Some(true),
        last_probed_at: None,
    }
}

fn instance(id: &str) -> Instance {
    Instance::new(
        id,
        "model-1",
        "runtime-1",
        LoadConfig {
            context_length: 4_096,
            max_concurrency: None,
            eval_batch_size: None,
            flash_attention: None,
            offload_kv_cache_to_gpu: None,
            n_gpu_layers: None,
            engine_config: None,
        },
    )
}

async fn manager() -> (Fixture, OperationManager) {
    let fixture = Fixture::new().await.expect("fixture");
    RuntimeRepo::upsert(fixture.store.pool(), &runtime())
        .await
        .expect("runtime");
    ModelsRepo::upsert(fixture.store.pool(), &model())
        .await
        .expect("model");
    let second = SqliteStore::open(&fixture.db_path)
        .await
        .expect("manager store");
    second.migrate().await.expect("migrate manager store");
    (fixture, OperationManager::new(second))
}

async fn ready(manager: &OperationManager, id: &str, op: &str) {
    let queued = manager
        .enqueue_load(op, instance(id))
        .await
        .expect("enqueue load");
    let running = manager.start(&queued).await.expect("start load");
    manager
        .succeed(
            &running,
            Some(ReadyProcess {
                pid: 42,
                port: 12_345,
            }),
            None,
        )
        .await
        .expect("finish load");
}

#[tokio::test]
async fn load_and_unload_each_reach_deterministic_terminal_states() {
    let (fixture, manager) = manager().await;
    ready(&manager, "instance-1", "load-1").await;
    let loaded = InstancesRepo::get(fixture.store.pool(), "instance-1")
        .await
        .expect("read instance")
        .expect("instance");
    assert_eq!(loaded.state(), InstanceState::Ready);
    assert_eq!(loaded.pid, Some(42));
    assert_eq!(loaded.port, Some(12_345));
    assert_eq!(
        OperationsRepo::get(fixture.store.pool(), "load-1")
            .await
            .expect("read operation")
            .expect("operation")
            .state(),
        OperationState::Succeeded
    );

    let draining = manager
        .enqueue_unload("unload-1", "instance-1")
        .await
        .expect("enqueue unload");
    let unloading = manager.start(&draining).await.expect("start unload");
    manager
        .succeed(&unloading, None, None)
        .await
        .expect("finish unload");
    let unloaded = InstancesRepo::get(fixture.store.pool(), "instance-1")
        .await
        .expect("read instance")
        .expect("instance");
    assert_eq!(unloaded.state(), InstanceState::Unloaded);
    assert_eq!(unloaded.pid, None);
    assert_eq!(unloaded.port, None);
    assert_eq!(
        OperationsRepo::get(fixture.store.pool(), "unload-1")
            .await
            .expect("read operation")
            .expect("operation")
            .state(),
        OperationState::Succeeded
    );
}

#[tokio::test]
async fn stale_revision_is_rejected_without_partial_transition() {
    let (fixture, manager) = manager().await;
    let stale = manager
        .enqueue_load("load-stale", instance("instance-stale"))
        .await
        .expect("enqueue");
    let running = manager.start(&stale).await.expect("start");
    let error = manager.start(&stale).await.expect_err("stale start");
    assert_eq!(error.code, ErrorCode::InvalidStateTransition);
    assert_eq!(
        OperationsRepo::get(fixture.store.pool(), "load-stale")
            .await
            .expect("operation")
            .expect("row")
            .state(),
        OperationState::Running
    );
    assert_eq!(
        InstancesRepo::get(fixture.store.pool(), "instance-stale")
            .await
            .expect("instance")
            .expect("row")
            .state(),
        InstanceState::Loading
    );
    manager
        .settle_cancelled(&running)
        .await
        .expect("settle running load");
}

#[tokio::test]
async fn a_failed_queued_load_is_promoted_then_terminalized_atomically() {
    let (fixture, manager) = manager().await;
    let queued = manager
        .enqueue_load("load-fail", instance("instance-fail"))
        .await
        .expect("enqueue");
    manager
        .fail(
            &queued,
            OperationError {
                code: ErrorCode::InvalidModel,
                message: "runtime rejected model".into(),
            },
            InstanceFailure {
                class: FailureClass::InvalidModel,
                exit_code: Some(2),
                stderr_tail: Some("invalid model".into()),
                message: Some("runtime rejected model".into()),
            },
        )
        .await
        .expect("settle failure");
    let operation = OperationsRepo::get(fixture.store.pool(), "load-fail")
        .await
        .expect("operation")
        .expect("row");
    let stored = InstancesRepo::get(fixture.store.pool(), "instance-fail")
        .await
        .expect("instance")
        .expect("row");
    assert_eq!(operation.state(), OperationState::Failed);
    assert!(operation.finished_at.is_some());
    assert_eq!(stored.state(), InstanceState::Failed);
    assert_eq!(
        stored.failure.expect("failure").class,
        FailureClass::InvalidModel
    );
}

#[tokio::test]
async fn enqueue_rolls_back_instance_when_operation_insert_fails() {
    let (fixture, manager) = manager().await;
    manager
        .enqueue_load("duplicate-op", instance("first-instance"))
        .await
        .expect("first enqueue");
    let error = manager
        .enqueue_load("duplicate-op", instance("rolled-back-instance"))
        .await
        .expect_err("duplicate operation must fail");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(
        InstancesRepo::get(fixture.store.pool(), "rolled-back-instance")
            .await
            .expect("lookup")
            .is_none(),
        "the instance insert must roll back with the operation insert"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_unloads_admit_exactly_one_operation() {
    let (fixture, manager) = manager().await;
    ready(&manager, "instance-race", "load-race").await;
    let manager = Arc::new(manager);
    let left = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.enqueue_unload("unload-left", "instance-race").await })
    };
    let right = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            manager
                .enqueue_unload("unload-right", "instance-race")
                .await
        })
    };
    let left = left.await.expect("left task");
    let right = right.await.expect("right task");
    assert_ne!(left.is_ok(), right.is_ok(), "exactly one unload must win");
    let rejected = left
        .as_ref()
        .err()
        .or_else(|| right.as_ref().err())
        .expect("one rejection");
    assert_eq!(rejected.code, ErrorCode::InvalidStateTransition);
    let winner = left.ok().or_else(|| right.ok()).expect("one winner");
    manager
        .settle_cancelled(&winner)
        .await
        .expect("cancel winner");
    assert_eq!(
        InstancesRepo::get(fixture.store.pool(), "instance-race")
            .await
            .expect("instance")
            .expect("row")
            .state(),
        InstanceState::Ready
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_start_and_restart_recovery_preserve_invariants() {
    let (fixture, manager) = manager().await;
    let handle = manager
        .enqueue_load("load-restart-race", instance("instance-restart-race"))
        .await
        .expect("enqueue");
    let manager = Arc::new(manager);
    let start = {
        let manager = Arc::clone(&manager);
        let handle = handle.clone();
        tokio::spawn(async move { manager.start(&handle).await })
    };
    let recover = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.recover_after_restart().await })
    };
    let _ = start.await.expect("start task");
    recover
        .await
        .expect("recovery task")
        .expect("recovery succeeds");

    let operation = OperationsRepo::get(fixture.store.pool(), "load-restart-race")
        .await
        .expect("operation")
        .expect("row");
    let stored = InstancesRepo::get(fixture.store.pool(), "instance-restart-race")
        .await
        .expect("instance")
        .expect("row");
    assert!(operation.state().is_terminal());
    assert!(stored.state().is_terminal());
    assert!(matches!(
        (operation.state(), stored.state()),
        (OperationState::Cancelled, InstanceState::Unloaded)
            | (OperationState::Failed, InstanceState::Crashed)
    ));

    let second = manager
        .recover_after_restart()
        .await
        .expect("idempotent recovery");
    assert_eq!(second.operations, 0);
    assert_eq!(second.instances, 0);
}

#[tokio::test]
async fn crashed_instance_can_be_explicitly_reloaded() {
    let (fixture, manager) = manager().await;
    let queued = manager
        .enqueue_load("load-before-crash", instance("instance-reload"))
        .await
        .expect("enqueue");
    manager.start(&queued).await.expect("start");
    manager
        .recover_after_restart()
        .await
        .expect("recover running load");
    let crashed = InstancesRepo::get(fixture.store.pool(), "instance-reload")
        .await
        .expect("instance")
        .expect("row");
    assert_eq!(crashed.state(), InstanceState::Crashed);

    let requeued = manager
        .enqueue_load("load-after-crash", crashed)
        .await
        .expect("explicit reload");
    assert!(requeued.instance_revision > 0);
    assert_eq!(
        InstancesRepo::get(fixture.store.pool(), "instance-reload")
            .await
            .expect("instance")
            .expect("row")
            .state(),
        InstanceState::Queued
    );
    let running = manager.start(&requeued).await.expect("start reload");
    manager
        .succeed(
            &running,
            Some(ReadyProcess {
                pid: 84,
                port: 12_346,
            }),
            None,
        )
        .await
        .expect("finish reload");
    let ready = InstancesRepo::get(fixture.store.pool(), "instance-reload")
        .await
        .expect("instance")
        .expect("row");
    assert_eq!(ready.state(), InstanceState::Ready);
    assert_eq!(ready.pid, Some(84));
}

#[tokio::test]
async fn load_success_requires_ready_process_and_rolls_back_on_rejection() {
    let (fixture, manager) = manager().await;
    let queued = manager
        .enqueue_load("load-no-process", instance("instance-no-process"))
        .await
        .expect("enqueue");
    let running = manager.start(&queued).await.expect("start");
    let error = manager
        .succeed(&running, None, None)
        .await
        .expect_err("missing process must be rejected");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(
        OperationsRepo::get(fixture.store.pool(), "load-no-process")
            .await
            .expect("operation")
            .expect("row")
            .state(),
        OperationState::Running
    );
    assert_eq!(
        InstancesRepo::get(fixture.store.pool(), "instance-no-process")
            .await
            .expect("instance")
            .expect("row")
            .state(),
        InstanceState::Loading
    );
    manager
        .settle_cancelled(&running)
        .await
        .expect("settle after rejection");
}

#[tokio::test]
async fn running_unload_failure_crashes_the_instance() {
    let (fixture, manager) = manager().await;
    ready(&manager, "instance-unload-fail", "load-unload-fail").await;
    let queued = manager
        .enqueue_unload("unload-fail", "instance-unload-fail")
        .await
        .expect("enqueue unload");
    let running = manager.start(&queued).await.expect("start unload");
    manager
        .fail(
            &running,
            OperationError {
                code: ErrorCode::Internal,
                message: "process could not be stopped".into(),
            },
            InstanceFailure {
                class: FailureClass::ProcessCrash,
                exit_code: None,
                stderr_tail: None,
                message: Some("process could not be stopped".into()),
            },
        )
        .await
        .expect("settle unload failure");
    assert_eq!(
        InstancesRepo::get(fixture.store.pool(), "instance-unload-fail")
            .await
            .expect("instance")
            .expect("row")
            .state(),
        InstanceState::Crashed
    );
}

#[tokio::test]
async fn restart_during_queued_unload_clears_stale_runtime_facts() {
    let (fixture, manager) = manager().await;
    ready(&manager, "instance-unload-restart", "load-unload-restart").await;
    manager
        .enqueue_unload("unload-before-restart", "instance-unload-restart")
        .await
        .expect("enqueue unload");
    manager
        .recover_after_restart()
        .await
        .expect("recover queued unload");
    let recovered = InstancesRepo::get(fixture.store.pool(), "instance-unload-restart")
        .await
        .expect("instance")
        .expect("row");
    assert_eq!(recovered.state(), InstanceState::Crashed);
    assert_eq!(recovered.pid, None);
    assert_eq!(recovered.port, None);
    assert_eq!(
        OperationsRepo::get(fixture.store.pool(), "unload-before-restart")
            .await
            .expect("operation")
            .expect("row")
            .state(),
        OperationState::Cancelled
    );
}
