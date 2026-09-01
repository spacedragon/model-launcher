use std::error::Error as _;
use std::{
    collections::VecDeque,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};

use model_launcher_core::{
    AppError, CatalogIdentity, EngineCapabilities, EngineFuture, EngineProcess, EngineSpec,
    InferenceEngine, LaunchProfile, LaunchSettings, Lifecycle, LifecycleState, ModelId, ModelKey,
    ModelRecord, ModelState,
};
use tokio::sync::{Notify, oneshot};

#[derive(Clone)]
struct ScriptedEngine {
    inner: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
struct FakeState {
    scripts: VecDeque<SpawnScript>,
    spawned: Vec<ModelId>,
    validation_gate: Option<Arc<Notify>>,
    spawn_gate: Option<Arc<Notify>>,
    graceful_calls: usize,
    force_calls: usize,
    validation_entered: usize,
    spawn_entered: usize,
    ready_entered: usize,
    exits_observed: usize,
    validation_completed: usize,
    spawn_returned: usize,
}

struct SpawnScript {
    ready: Arc<Notify>,
    ready_fails: Arc<AtomicBool>,
    exit_rx: oneshot::Receiver<i32>,
    graceful_gate: Arc<Mutex<Option<Arc<Notify>>>>,
    force_gate: Arc<Mutex<Option<Arc<Notify>>>>,
}

struct SpawnControl {
    ready: Arc<Notify>,
    ready_fails: Arc<AtomicBool>,
    exit_tx: Option<oneshot::Sender<i32>>,
    graceful_gate: Arc<Mutex<Option<Arc<Notify>>>>,
    force_gate: Arc<Mutex<Option<Arc<Notify>>>>,
}

impl SpawnControl {
    fn ready(&self) {
        self.ready.notify_one();
    }

    fn exit(&mut self, code: i32) {
        self.exit_tx.take().expect("exit only once").send(code).ok();
    }

    fn fail_ready(&self) {
        self.ready_fails.store(true, Ordering::Release);
        self.ready.notify_one();
    }

    fn block_graceful(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.graceful_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    fn block_force(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.force_gate.lock().unwrap() = Some(gate.clone());
        gate
    }
}

impl ScriptedEngine {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(FakeState::default())),
        }
    }

    fn script(&self) -> SpawnControl {
        let ready = Arc::new(Notify::new());
        let ready_fails = Arc::new(AtomicBool::new(false));
        let (exit_tx, exit_rx) = oneshot::channel();
        let graceful_gate = Arc::new(Mutex::new(None));
        let force_gate = Arc::new(Mutex::new(None));
        self.inner.lock().unwrap().scripts.push_back(SpawnScript {
            ready: ready.clone(),
            ready_fails: ready_fails.clone(),
            exit_rx,
            graceful_gate: graceful_gate.clone(),
            force_gate: force_gate.clone(),
        });
        SpawnControl {
            ready,
            ready_fails,
            exit_tx: Some(exit_tx),
            graceful_gate,
            force_gate,
        }
    }

    fn spawn_count(&self) -> usize {
        self.inner.lock().unwrap().spawned.len()
    }

    fn block_validation(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.inner.lock().unwrap().validation_gate = Some(gate.clone());
        gate
    }

    fn block_spawn(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.inner.lock().unwrap().spawn_gate = Some(gate.clone());
        gate
    }

    fn shutdown_counts(&self) -> (usize, usize) {
        let state = self.inner.lock().unwrap();
        (state.graceful_calls, state.force_calls)
    }

    fn event_counts(&self) -> (usize, usize, usize, usize, usize, usize) {
        let state = self.inner.lock().unwrap();
        (
            state.validation_entered,
            state.spawn_entered,
            state.ready_entered,
            state.exits_observed,
            state.validation_completed,
            state.spawn_returned,
        )
    }
}

struct FakeProcess {
    ready: Arc<Notify>,
    ready_fails: Arc<AtomicBool>,
    exit_rx: oneshot::Receiver<i32>,
    graceful_gate: Arc<Mutex<Option<Arc<Notify>>>>,
    force_gate: Arc<Mutex<Option<Arc<Notify>>>>,
    engine: ScriptedEngine,
}

impl InferenceEngine for ScriptedEngine {
    fn spec(&self) -> EngineFuture<'_, EngineSpec> {
        Box::pin(async { unreachable!() })
    }

    fn probe_capabilities(&self) -> EngineFuture<'_, EngineCapabilities> {
        Box::pin(async { unreachable!() })
    }

    fn validate_launch<'a>(
        &'a self,
        _model: &'a ModelRecord,
        _settings: &'a LaunchSettings,
    ) -> EngineFuture<'a, ()> {
        self.inner.lock().unwrap().validation_entered += 1;
        let gate = self.inner.lock().unwrap().validation_gate.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            self.inner.lock().unwrap().validation_completed += 1;
            Ok(())
        })
    }

    fn spawn<'a>(
        &'a self,
        model: &'a ModelRecord,
        _settings: &'a LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>> {
        Box::pin(async move {
            self.inner.lock().unwrap().spawn_entered += 1;
            let gate = self.inner.lock().unwrap().spawn_gate.clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            let mut state = self.inner.lock().unwrap();
            state.spawned.push(model.id);
            let script = state.scripts.pop_front().expect("scripted spawn");
            let process = Box::new(FakeProcess {
                ready: script.ready,
                ready_fails: script.ready_fails,
                exit_rx: script.exit_rx,
                graceful_gate: script.graceful_gate,
                force_gate: script.force_gate,
                engine: self.clone(),
            }) as Box<dyn EngineProcess>;
            state.spawn_returned += 1;
            Ok(process)
        })
    }
}

impl EngineProcess for FakeProcess {
    fn endpoint(&self) -> Option<SocketAddr> {
        Some("127.0.0.1:43123".parse().unwrap())
    }

    fn wait_ready(&mut self, _timeout: Duration) -> EngineFuture<'_, ()> {
        Box::pin(async move {
            self.engine.inner.lock().unwrap().ready_entered += 1;
            self.ready.notified().await;
            if self.ready_fails.load(Ordering::Acquire) {
                Err(AppError::EngineProcess(Box::new(io::Error::other(
                    "readiness failed",
                ))))
            } else {
                Ok(())
            }
        })
    }

    fn check_health(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn graceful_shutdown(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async move {
            self.engine.inner.lock().unwrap().graceful_calls += 1;
            let gate = self.graceful_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            Ok(())
        })
    }

    fn force_shutdown(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async move {
            self.engine.inner.lock().unwrap().force_calls += 1;
            let gate = self.force_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            Ok(())
        })
    }

    fn wait_for_exit(&mut self) -> EngineFuture<'_, i32> {
        Box::pin(async move {
            let code = (&mut self.exit_rx).await.unwrap_or_default();
            self.engine.inner.lock().unwrap().exits_observed += 1;
            Ok(code)
        })
    }
}

fn model(name: &str) -> ModelRecord {
    ModelRecord {
        id: ModelId::new(),
        key: ModelKey::parse(name).unwrap(),
        display_name: name.to_owned(),
        path: PathBuf::from(format!("/{name}.gguf")),
        file_identity: CatalogIdentity::Unavailable,
        size_bytes: 1,
        metadata: Default::default(),
        state: ModelState::Available,
        launch_profile: LaunchProfile::default(),
    }
}

async fn wait_for_state(handle: &model_launcher_core::LifecycleHandle, state: LifecycleState) {
    let mut snapshots = handle.subscribe();
    while snapshots.borrow().state != state {
        snapshots.changed().await.unwrap();
    }
}

async fn wait_until(description: &str, mut predicate: impl FnMut() -> bool) {
    for _ in 0..256 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition was not reached: {description}");
}

#[tokio::test(start_paused = true)]
async fn load_transitions_stopped_starting_running_and_waits_for_readiness() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let mut snapshots = lifecycle.subscribe();
    let target = model("alpha");

    let load = tokio::spawn({
        let handle = lifecycle.handle();
        let target = target.clone();
        async move { handle.load(target).await }
    });
    snapshots.changed().await.unwrap();
    assert_eq!(snapshots.borrow().state, LifecycleState::Starting);
    assert!(!load.is_finished(), "load must wait for readiness");

    control.ready();
    assert!(load.await.unwrap().is_ok());
    assert_eq!(snapshots.borrow().state, LifecycleState::Running);
    assert_eq!(snapshots.borrow().desired_model, Some(target.id));
    assert_eq!(
        snapshots.borrow().endpoint,
        Some("127.0.0.1:43123".parse().unwrap())
    );
}

#[tokio::test(start_paused = true)]
async fn endpoint_is_owned_only_while_process_is_running() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let graceful_gate = control.block_graceful();
    let lifecycle = Lifecycle::spawn(engine);
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("endpoint")).await }
    });
    control.ready();
    load.await.unwrap().unwrap();
    assert_eq!(
        handle.snapshot().endpoint,
        Some("127.0.0.1:43123".parse().unwrap())
    );

    let eject = tokio::spawn({
        let handle = handle.clone();
        async move { handle.eject().await }
    });
    wait_for_state(&handle, LifecycleState::Stopping).await;
    assert_eq!(handle.snapshot().endpoint, None);
    graceful_gate.notify_one();
    eject.await.unwrap().unwrap();
    assert_eq!(handle.snapshot().endpoint, None);
}

#[tokio::test(start_paused = true)]
async fn replacement_stops_a_and_loads_b_without_restoring_a_on_failure() {
    let engine = Arc::new(ScriptedEngine::new());
    let a_control = engine.script();
    let b_control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let a = model("a");
    let b = model("b");

    let load_a = tokio::spawn({
        let handle = handle.clone();
        let a = a.clone();
        async move { handle.load(a).await }
    });
    a_control.ready();
    load_a.await.unwrap().unwrap();

    let load_b = tokio::spawn({
        let handle = handle.clone();
        let b = b.clone();
        async move { handle.load(b).await }
    });
    b_control.fail_ready();
    assert_eq!(
        load_b.await.unwrap().unwrap_err().code(),
        "model_load_failed"
    );
    assert_eq!(handle.snapshot().desired_model, None);
    assert_eq!(engine.spawn_count(), 2, "failed B must not restore A");
}

#[tokio::test(start_paused = true)]
async fn replacement_is_busy_while_inference_lease_is_active() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine);
    let handle = lifecycle.handle();
    let a = model("a");
    let b = model("b");
    let acquire = tokio::spawn({
        let handle = handle.clone();
        let a = a.clone();
        async move { handle.acquire(a).await }
    });
    control.ready();
    let lease = acquire.await.unwrap().unwrap();

    assert_eq!(handle.load(b).await.unwrap_err().code(), "model_busy");
    assert_eq!(handle.snapshot().in_flight, 1);
    drop(lease);
    wait_until("lease release", || handle.snapshot().in_flight == 0).await;
}

#[tokio::test(start_paused = true)]
async fn explicit_eject_cancels_leases_and_clears_desired_model() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine);
    let handle = lifecycle.handle();
    let acquire = tokio::spawn({
        let handle = handle.clone();
        async move { handle.acquire(model("a")).await }
    });
    control.ready();
    let lease = acquire.await.unwrap().unwrap();

    handle.eject().await.unwrap();
    assert!(lease.is_cancelled());
    assert_eq!(handle.snapshot().state, LifecycleState::Stopped);
    assert_eq!(handle.snapshot().desired_model, None);
    assert_eq!(handle.snapshot().in_flight, 0);
}

#[tokio::test(start_paused = true)]
async fn unexpected_crashes_restart_with_capped_exponential_backoff() {
    let engine = Arc::new(ScriptedEngine::new());
    let mut controls: Vec<_> = (0..7).map(|_| engine.script()).collect();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    controls[0].ready();
    load.await.unwrap().unwrap();

    for (index, delay) in [1, 2, 4, 8, 16, 30].into_iter().enumerate() {
        controls[index].exit(9);
        wait_for_state(&handle, LifecycleState::Backoff).await;
        tokio::time::advance(Duration::from_secs(delay - 1)).await;
        assert_eq!(engine.spawn_count(), index + 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until("backoff restart spawn", || {
            engine.spawn_count() == index + 2
        })
        .await;
        controls[index + 1].ready();
    }
}

#[tokio::test(start_paused = true)]
async fn five_healthy_minutes_reset_crash_backoff() {
    let engine = Arc::new(ScriptedEngine::new());
    let mut first = engine.script();
    let mut second = engine.script();
    let third = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    first.ready();
    load.await.unwrap().unwrap();
    first.exit(9);
    wait_for_state(&handle, LifecycleState::Backoff).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_until("second readiness entered", || engine.event_counts().2 == 2).await;
    second.ready();
    wait_for_state(&handle, LifecycleState::Running).await;
    tokio::time::advance(Duration::from_secs(300)).await;
    second.exit(9);
    wait_for_state(&handle, LifecycleState::Backoff).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_until("healthy window reset restart", || engine.spawn_count() == 3).await;
    third.ready();
}

#[tokio::test(start_paused = true)]
async fn stale_generation_timer_cannot_restart_after_eject() {
    let engine = Arc::new(ScriptedEngine::new());
    let mut control = engine.script();
    let _unused_restart = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    control.ready();
    load.await.unwrap().unwrap();
    control.exit(9);
    handle.eject().await.unwrap();

    tokio::time::advance(Duration::from_secs(60)).await;
    wait_until("shared JIT spawn", || engine.spawn_count() == 1).await;
    assert_eq!(handle.snapshot().state, LifecycleState::Stopped);
}

#[tokio::test(start_paused = true)]
async fn concurrent_same_model_jit_acquires_share_one_load() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let target = model("a");
    let one = tokio::spawn({
        let handle = handle.clone();
        let target = target.clone();
        async move { handle.acquire(target).await }
    });
    let two = tokio::spawn({
        let handle = handle.clone();
        let target = target.clone();
        async move { handle.acquire(target).await }
    });
    wait_until("shared JIT reached readiness", || {
        engine.event_counts().2 == 1
    })
    .await;
    assert_eq!(engine.spawn_count(), 1);
    assert!(!one.is_finished() && !two.is_finished());

    control.ready();
    let lease_one = one.await.unwrap().unwrap();
    let lease_two = two.await.unwrap().unwrap();
    assert_eq!(engine.spawn_count(), 1);
    assert_eq!(handle.snapshot().in_flight, 2);
    drop((lease_one, lease_two));
}

#[tokio::test(start_paused = true)]
async fn crash_cancels_and_clears_active_generation_leases() {
    let engine = Arc::new(ScriptedEngine::new());
    let mut first = engine.script();
    let _restart = engine.script();
    let lifecycle = Lifecycle::spawn(engine);
    let handle = lifecycle.handle();
    let acquire = tokio::spawn({
        let handle = handle.clone();
        async move { handle.acquire(model("a")).await }
    });
    first.ready();
    let lease = acquire.await.unwrap().unwrap();
    first.exit(9);

    wait_until("crash lease cancellation", || lease.is_cancelled()).await;
    wait_until("crash clears in-flight", || {
        handle.snapshot().in_flight == 0
    })
    .await;
    drop(lease);
    wait_until("old lease drop remains clear", || {
        handle.snapshot().in_flight == 0
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn cancelled_acquire_waiter_is_never_counted_as_in_flight() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine);
    let handle = lifecycle.handle();
    let acquire = tokio::spawn({
        let handle = handle.clone();
        async move { handle.acquire(model("a")).await }
    });
    acquire.abort();
    control.ready();
    wait_until("cancelled waiter excluded", || {
        handle.snapshot().in_flight == 0
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn burst_lease_drops_cannot_lose_release_events() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine);
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    control.ready();
    load.await.unwrap().unwrap();
    let target = model("a");
    // Same identity as the running model is required, so retain it from the snapshot through a
    // fresh record with the exact id.
    let running_id = handle.snapshot().desired_model.unwrap();
    let mut target = target;
    target.id = running_id;
    let mut leases = Vec::new();
    for _ in 0..100 {
        leases.push(handle.acquire(target.clone()).await.unwrap());
    }
    assert_eq!(handle.snapshot().in_flight, 100);
    drop(leases);
    wait_until("burst releases drained", || {
        handle.snapshot().in_flight == 0
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn same_model_load_waiters_share_stable_failure() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine);
    let handle = lifecycle.handle();
    let target = model("a");
    let one = tokio::spawn({
        let handle = handle.clone();
        let target = target.clone();
        async move { handle.load(target).await.unwrap_err() }
    });
    let two = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(target).await.unwrap_err() }
    });
    control.fail_ready();
    let one = one.await.unwrap();
    let two = two.await.unwrap();
    assert_eq!(one.code(), "model_load_failed");
    assert_eq!(two.code(), one.code());
    assert_eq!(two.to_string(), one.to_string());
}

#[tokio::test(start_paused = true)]
async fn acquire_different_model_during_backoff_switches_immediately() {
    let engine = Arc::new(ScriptedEngine::new());
    let mut a_control = engine.script();
    let b_control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let a = model("a");
    let b = model("b");
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(a).await }
    });
    a_control.ready();
    load.await.unwrap().unwrap();
    a_control.exit(9);
    let acquire = tokio::spawn({
        let handle = handle.clone();
        let b = b.clone();
        async move { handle.acquire(b).await }
    });
    wait_until("backoff model switch spawn", || engine.spawn_count() == 2).await;
    b_control.ready();
    assert!(acquire.await.unwrap().is_ok());
    assert_eq!(handle.snapshot().desired_model, Some(b.id));
}

#[tokio::test(start_paused = true)]
async fn startup_timeout_stops_owned_process_and_fails_load() {
    let engine = Arc::new(ScriptedEngine::new());
    let _control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load = tokio::spawn(async move { handle.load(model("a")).await });
    wait_until("startup readiness entered", || engine.event_counts().2 == 1).await;
    tokio::time::advance(Duration::from_secs(30)).await;

    wait_until("startup timeout reply", || load.is_finished()).await;
    assert_eq!(load.await.unwrap().unwrap_err().code(), "model_load_failed");
    assert_eq!(engine.shutdown_counts().0, 1);
}

#[tokio::test(start_paused = true)]
async fn readiness_failure_stops_process_before_reporting_failure() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load = tokio::spawn(async move { handle.load(model("a")).await });
    control.fail_ready();
    let error = load.await.unwrap().unwrap_err();
    assert_eq!(error.code(), "model_load_failed");
    let mut sources = Vec::new();
    let mut source = error.source();
    while let Some(error) = source {
        sources.push(error.to_string());
        source = error.source();
    }
    assert!(sources.iter().any(|source| source == "readiness failed"));
    assert_eq!(engine.shutdown_counts().0, 1);
}

#[tokio::test(start_paused = true)]
async fn eject_responds_while_validation_is_pending() {
    let engine = Arc::new(ScriptedEngine::new());
    let _gate = engine.block_validation();
    let _control = engine.script();
    let lifecycle = Lifecycle::spawn(engine);
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    let eject = tokio::spawn(async move { handle.eject().await });
    wait_until("validation cancellation reply", || eject.is_finished()).await;
    eject.await.unwrap().unwrap();
    assert_eq!(lifecycle.handle().snapshot().state, LifecycleState::Stopped);
    load.abort();
}

#[tokio::test(start_paused = true)]
async fn eject_responds_while_spawn_is_pending() {
    let engine = Arc::new(ScriptedEngine::new());
    let gate = engine.block_spawn();
    let _control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    let eject = tokio::spawn(async move { handle.eject().await });
    wait_until("spawn cancellation reply", || eject.is_finished()).await;
    eject.await.unwrap().unwrap();
    gate.notify_one();
    assert_eq!(engine.spawn_count(), 0);
    load.abort();
}

#[tokio::test(start_paused = true)]
async fn eject_responds_while_readiness_is_pending_and_cleans_process() {
    let engine = Arc::new(ScriptedEngine::new());
    let _control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    wait_until("readiness wait entered", || engine.event_counts().2 == 1).await;
    handle.eject().await.unwrap();
    wait_until("readiness cancellation cleanup", || {
        engine.shutdown_counts().0 == 1
    })
    .await;
    assert_eq!(load.await.unwrap().unwrap_err().code(), "model_load_failed");
}

#[tokio::test(start_paused = true)]
async fn graceful_stop_timeout_forces_process_and_allows_replacement() {
    let engine = Arc::new(ScriptedEngine::new());
    let a_control = engine.script();
    let _gate = a_control.block_graceful();
    let b_control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load_a = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    a_control.ready();
    load_a.await.unwrap().unwrap();
    let load_b = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("b")).await }
    });
    wait_for_state(&handle, LifecycleState::Stopping).await;
    wait_until("graceful replacement stop entered", || {
        engine.shutdown_counts().0 == 1
    })
    .await;
    tokio::time::advance(Duration::from_secs(5)).await;
    wait_until("forced replacement stop", || {
        engine.shutdown_counts() == (1, 1)
    })
    .await;
    assert_eq!(engine.spawn_count(), 2);
    b_control.ready();
    load_b.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_is_accepted_while_graceful_stop_is_pending() {
    let engine = Arc::new(ScriptedEngine::new());
    let a_control = engine.script();
    let _gate = a_control.block_graceful();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load_a = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    a_control.ready();
    load_a.await.unwrap().unwrap();
    let replacement = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("b")).await }
    });
    wait_for_state(&handle, LifecycleState::Stopping).await;
    wait_until("graceful shutdown entered", || {
        engine.shutdown_counts().0 == 1
    })
    .await;
    let shutdown = tokio::spawn(async move { handle.shutdown().await });
    assert!(
        !shutdown.is_finished(),
        "shutdown waits for actual stop completion"
    );
    tokio::time::advance(Duration::from_secs(5)).await;
    wait_until("shutdown after stop completion", || shutdown.is_finished()).await;
    shutdown.await.unwrap().unwrap();
    replacement.abort();
    assert_eq!(
        engine.spawn_count(),
        1,
        "shutdown cancels pending replacement"
    );
}

#[tokio::test(start_paused = true)]
async fn readiness_cancellation_blocks_new_launch_until_cleanup_finishes() {
    let engine = Arc::new(ScriptedEngine::new());
    let a_control = engine.script();
    let graceful = a_control.block_graceful();
    let _b_control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load_a = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    wait_until("readiness wait entered", || engine.event_counts().2 == 1).await;
    let eject = tokio::spawn({
        let handle = handle.clone();
        async move { handle.eject().await }
    });
    let load_b = tokio::spawn(async move { handle.load(model("b")).await });
    assert!(!eject.is_finished());
    wait_until("readiness entered spawn", || engine.spawn_count() == 1).await;
    assert_eq!(load_b.await.unwrap().unwrap_err().code(), "model_starting");

    graceful.notify_one();
    eject.await.unwrap().unwrap();
    assert_eq!(engine.spawn_count(), 1);
    assert_eq!(
        load_a.await.unwrap().unwrap_err().code(),
        "model_load_failed"
    );
}

#[tokio::test(start_paused = true)]
async fn cancelled_spawn_future_cannot_create_a_late_process() {
    let engine = Arc::new(ScriptedEngine::new());
    let spawn_gate = engine.block_spawn();
    let _a_control = engine.script();
    let _b_control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let _load_a = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    let eject = tokio::spawn({
        let handle = handle.clone();
        async move { handle.eject().await }
    });
    assert_eq!(engine.spawn_count(), 0);
    wait_until("cancelled spawn reply", || eject.is_finished()).await;

    eject.await.unwrap().unwrap();
    spawn_gate.notify_one();
    assert_eq!(engine.spawn_count(), 0);
    assert_eq!(engine.shutdown_counts().0, 0);
}

#[tokio::test(start_paused = true)]
async fn force_timeout_aggregates_stop_requests_and_blocks_replacement() {
    let engine = Arc::new(ScriptedEngine::new());
    let a_control = engine.script();
    let _graceful = a_control.block_graceful();
    let _force = a_control.block_force();
    let _b_control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load_a = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    a_control.ready();
    load_a.await.unwrap().unwrap();
    let replacement = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("b")).await }
    });
    wait_for_state(&handle, LifecycleState::Stopping).await;
    wait_until("graceful force-timeout stop entered", || {
        engine.shutdown_counts().0 == 1
    })
    .await;
    tokio::time::advance(Duration::from_secs(5)).await;
    wait_until("force-timeout phase entered", || {
        engine.shutdown_counts().1 == 1
    })
    .await;
    let shutdown = tokio::spawn({
        let handle = lifecycle.handle();
        async move { handle.shutdown().await }
    });
    let eject = tokio::spawn({
        let handle = lifecycle.handle();
        async move { handle.eject().await }
    });
    assert!(!shutdown.is_finished() && !eject.is_finished());
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(shutdown.await.unwrap().is_err());
    assert!(eject.await.unwrap().is_err());
    assert!(replacement.await.unwrap().is_err());
    assert_eq!(engine.spawn_count(), 1);
    assert_eq!(
        lifecycle.handle().snapshot().state,
        LifecycleState::Stopping
    );
}

#[tokio::test(start_paused = true)]
async fn dropping_all_command_owners_terminates_stopped_actor() {
    let engine = Arc::new(ScriptedEngine::new());
    let weak = Arc::downgrade(&engine);
    let lifecycle = Lifecycle::spawn(engine.clone());
    drop(engine);

    lifecycle.wait_for_termination().await;
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn bounded_termination_aborts_and_joins_actor_without_detaching() {
    let engine = Arc::new(ScriptedEngine::new());
    let _gate = engine.block_validation();
    let weak = Arc::downgrade(&engine);
    let lifecycle = Lifecycle::spawn(engine.clone());
    let load = tokio::spawn({
        let handle = lifecycle.handle();
        async move { handle.load(model("a")).await }
    });
    drop(engine);

    assert!(
        !lifecycle
            .wait_for_termination_bounded(Duration::from_millis(10))
            .await
    );
    assert!(load.await.unwrap().is_err());
    assert!(
        weak.upgrade().is_none(),
        "joined actor must release its engine owner"
    );
}

#[tokio::test(start_paused = true)]
async fn dropping_all_command_owners_stops_running_process_before_actor_exit() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    wait_for_state(&handle, LifecycleState::Starting).await;
    control.ready();
    load.await.unwrap().unwrap();
    drop(handle);

    lifecycle.wait_for_termination().await;
    assert_eq!(engine.shutdown_counts(), (1, 0));
}

#[tokio::test(start_paused = true)]
async fn owner_drop_during_pending_stop_exits_after_bounded_force_timeout() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let _graceful = control.block_graceful();
    let _force = control.block_force();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    wait_for_state(&handle, LifecycleState::Starting).await;
    control.ready();
    load.await.unwrap().unwrap();
    let mut snapshots = handle.subscribe();
    drop(handle);
    let termination = tokio::spawn(lifecycle.wait_for_termination());
    while snapshots.borrow().state != LifecycleState::Stopping {
        snapshots.changed().await.unwrap();
    }
    wait_until("owner-drop graceful stop entered", || {
        engine.shutdown_counts().0 == 1
    })
    .await;

    tokio::time::advance(Duration::from_secs(5)).await;
    wait_until("force shutdown start", || engine.shutdown_counts().1 == 1).await;
    assert_eq!(engine.shutdown_counts(), (1, 1));
    assert!(!termination.is_finished());
    tokio::time::advance(Duration::from_secs(5)).await;
    termination.await.unwrap();
    assert_eq!(engine.shutdown_counts(), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn validation_and_spawn_have_actor_enforced_timeouts() {
    let validating = Arc::new(ScriptedEngine::new());
    let _validation_gate = validating.block_validation();
    let _validation_process = validating.script();
    let validation_lifecycle = Lifecycle::spawn(validating.clone());
    let validation_handle = validation_lifecycle.handle();
    let validation = tokio::spawn(async move { validation_handle.load(model("a")).await });
    wait_until("validation future entered", || {
        validating.event_counts().0 == 1
    })
    .await;
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(
        validation.await.unwrap().unwrap_err().code(),
        "model_load_failed"
    );

    let spawning = Arc::new(ScriptedEngine::new());
    let _spawn_gate = spawning.block_spawn();
    let _spawn_process = spawning.script();
    let spawn_lifecycle = Lifecycle::spawn(spawning.clone());
    let spawn_handle = spawn_lifecycle.handle();
    let spawn = tokio::spawn(async move { spawn_handle.load(model("a")).await });
    wait_until("spawn future entered", || spawning.event_counts().1 == 1).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(
        spawn.await.unwrap().unwrap_err().code(),
        "model_load_failed"
    );
}

#[tokio::test(start_paused = true)]
async fn same_model_start_waiters_are_capped_without_duplicate_spawn() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let target = model("a");
    let mut loads = Vec::new();
    for _ in 0..160 {
        loads.push(tokio::spawn({
            let handle = handle.clone();
            let target = target.clone();
            async move { handle.load(target).await }
        }));
    }
    wait_for_state(&handle, LifecycleState::Starting).await;
    wait_until("waiter cap overload replies", || {
        loads.iter().filter(|load| load.is_finished()).count() >= 32
    })
    .await;
    assert_eq!(engine.spawn_count(), 1);
    control.ready();

    let mut busy = 0;
    let mut success = 0;
    for load in loads {
        match load.await.unwrap() {
            Ok(()) => success += 1,
            Err(error) if error.code() == "model_busy" => busy += 1,
            Err(error) => panic!("unexpected load error: {}", error.code()),
        }
    }
    assert_eq!(success, 128);
    assert_eq!(busy, 32);
}

#[test]
fn app_error_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AppError>();
}
