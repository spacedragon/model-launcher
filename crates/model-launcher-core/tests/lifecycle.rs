use std::error::Error as _;
use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};

use model_launcher_core::{
    AppError, EngineCapabilities, EngineFuture, EngineProcess, EngineSpec, InferenceEngine,
    LaunchProfile, LaunchSettings, Lifecycle, LifecycleState, ModelId, ModelKey, ModelRecord,
    ModelState,
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
        self.ready.notify_waiters();
    }

    fn exit(&mut self, code: i32) {
        self.exit_tx.take().expect("exit only once").send(code).ok();
    }

    fn fail_ready(&self) {
        self.ready_fails.store(true, Ordering::Release);
        self.ready.notify_waiters();
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
        let gate = self.inner.lock().unwrap().validation_gate.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            Ok(())
        })
    }

    fn spawn<'a>(
        &'a self,
        model: &'a ModelRecord,
        _settings: &'a LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>> {
        Box::pin(async move {
            let gate = self.inner.lock().unwrap().spawn_gate.clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            let mut state = self.inner.lock().unwrap();
            state.spawned.push(model.id);
            let script = state.scripts.pop_front().expect("scripted spawn");
            Ok(Box::new(FakeProcess {
                ready: script.ready,
                ready_fails: script.ready_fails,
                exit_rx: script.exit_rx,
                graceful_gate: script.graceful_gate,
                force_gate: script.force_gate,
                engine: self.clone(),
            }) as Box<dyn EngineProcess>)
        })
    }
}

impl EngineProcess for FakeProcess {
    fn wait_ready(&mut self, _timeout: Duration) -> EngineFuture<'_, ()> {
        Box::pin(async move {
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
        Box::pin(async move { Ok((&mut self.exit_rx).await.unwrap_or_default()) })
    }
}

fn model(name: &str) -> ModelRecord {
    ModelRecord {
        id: ModelId::new(),
        key: ModelKey::parse(name).unwrap(),
        display_name: name.to_owned(),
        path: PathBuf::from(format!("/{name}.gguf")),
        size_bytes: 1,
        state: ModelState::Available,
        launch_profile: LaunchProfile::default(),
    }
}

async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn load_transitions_stopped_starting_running_and_waits_for_readiness() {
    let engine = Arc::new(ScriptedEngine::new());
    let control = engine.script();
    let lifecycle = Lifecycle::spawn(engine);
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
    settle().await;
    a_control.ready();
    load_a.await.unwrap().unwrap();

    let load_b = tokio::spawn({
        let handle = handle.clone();
        let b = b.clone();
        async move { handle.load(b).await }
    });
    settle().await;
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
    settle().await;
    control.ready();
    let lease = acquire.await.unwrap().unwrap();

    assert_eq!(handle.load(b).await.unwrap_err().code(), "model_busy");
    assert_eq!(handle.snapshot().in_flight, 1);
    drop(lease);
    settle().await;
    assert_eq!(handle.snapshot().in_flight, 0);
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
    settle().await;
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
    settle().await;
    controls[0].ready();
    load.await.unwrap().unwrap();

    for (index, delay) in [1, 2, 4, 8, 16, 30].into_iter().enumerate() {
        controls[index].exit(9);
        settle().await;
        assert_eq!(handle.snapshot().state, LifecycleState::Backoff);
        tokio::time::advance(Duration::from_secs(delay - 1)).await;
        settle().await;
        assert_eq!(engine.spawn_count(), index + 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(engine.spawn_count(), index + 2);
        controls[index + 1].ready();
        settle().await;
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
    settle().await;
    first.ready();
    load.await.unwrap().unwrap();
    first.exit(9);
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    second.ready();
    settle().await;
    tokio::time::advance(Duration::from_secs(300)).await;
    second.exit(9);
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(engine.spawn_count(), 3, "healthy window resets delay to 1s");
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
    settle().await;
    control.ready();
    load.await.unwrap().unwrap();
    control.exit(9);
    settle().await;
    handle.eject().await.unwrap();

    tokio::time::advance(Duration::from_secs(60)).await;
    settle().await;
    assert_eq!(engine.spawn_count(), 1);
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
    settle().await;
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
    settle().await;
    first.ready();
    let lease = acquire.await.unwrap().unwrap();
    first.exit(9);
    settle().await;

    assert!(lease.is_cancelled());
    assert_eq!(handle.snapshot().in_flight, 0);
    drop(lease);
    settle().await;
    assert_eq!(handle.snapshot().in_flight, 0);
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
    settle().await;
    acquire.abort();
    control.ready();
    settle().await;
    assert_eq!(handle.snapshot().in_flight, 0);
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
    settle().await;
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
    settle().await;
    assert_eq!(handle.snapshot().in_flight, 0);
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
    settle().await;
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
    settle().await;
    a_control.ready();
    load.await.unwrap().unwrap();
    a_control.exit(9);
    settle().await;
    let acquire = tokio::spawn({
        let handle = handle.clone();
        let b = b.clone();
        async move { handle.acquire(b).await }
    });
    settle().await;
    assert_eq!(engine.spawn_count(), 2);
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
    settle().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    settle().await;

    assert!(load.is_finished());
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
    settle().await;
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
    settle().await;
    let eject = tokio::spawn(async move { handle.eject().await });
    settle().await;
    assert!(eject.is_finished());
    eject.await.unwrap().unwrap();
    assert_eq!(lifecycle.handle().snapshot().state, LifecycleState::Stopped);
    load.abort();
}

#[tokio::test(start_paused = true)]
async fn eject_responds_while_spawn_is_pending() {
    let engine = Arc::new(ScriptedEngine::new());
    let gate = engine.block_spawn();
    let _control = engine.script();
    let lifecycle = Lifecycle::spawn(engine);
    let handle = lifecycle.handle();
    let load = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    settle().await;
    let eject = tokio::spawn(async move { handle.eject().await });
    settle().await;
    assert!(!eject.is_finished());
    gate.notify_waiters();
    settle().await;
    assert!(eject.is_finished());
    eject.await.unwrap().unwrap();
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
    settle().await;
    handle.eject().await.unwrap();
    settle().await;
    assert_eq!(engine.shutdown_counts().0, 1);
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
    settle().await;
    a_control.ready();
    load_a.await.unwrap().unwrap();
    let load_b = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("b")).await }
    });
    settle().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert_eq!(engine.shutdown_counts(), (1, 1));
    assert_eq!(engine.spawn_count(), 2);
    b_control.ready();
    load_b.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_is_accepted_while_graceful_stop_is_pending() {
    let engine = Arc::new(ScriptedEngine::new());
    let a_control = engine.script();
    let _gate = a_control.block_graceful();
    let _b_control = engine.script();
    let lifecycle = Lifecycle::spawn(engine.clone());
    let handle = lifecycle.handle();
    let load_a = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("a")).await }
    });
    settle().await;
    a_control.ready();
    load_a.await.unwrap().unwrap();
    let replacement = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("b")).await }
    });
    settle().await;
    let shutdown = tokio::spawn(async move { handle.shutdown().await });
    settle().await;
    assert!(
        !shutdown.is_finished(),
        "shutdown waits for actual stop completion"
    );
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert!(shutdown.is_finished());
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
    settle().await;
    let eject = tokio::spawn({
        let handle = handle.clone();
        async move { handle.eject().await }
    });
    settle().await;
    let load_b = tokio::spawn(async move { handle.load(model("b")).await });
    settle().await;
    assert!(!eject.is_finished());
    assert_eq!(engine.spawn_count(), 1);
    assert_eq!(load_b.await.unwrap().unwrap_err().code(), "model_starting");

    graceful.notify_waiters();
    settle().await;
    eject.await.unwrap().unwrap();
    assert_eq!(engine.spawn_count(), 1);
    assert_eq!(
        load_a.await.unwrap().unwrap_err().code(),
        "model_load_failed"
    );
}

#[tokio::test(start_paused = true)]
async fn late_spawn_after_cancellation_is_cleaned_before_new_launch() {
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
    settle().await;
    let eject = tokio::spawn({
        let handle = handle.clone();
        async move { handle.eject().await }
    });
    settle().await;
    let load_b = tokio::spawn(async move { handle.load(model("b")).await });
    settle().await;
    assert_eq!(engine.spawn_count(), 0);
    assert!(!eject.is_finished());
    assert_eq!(load_b.await.unwrap().unwrap_err().code(), "model_starting");

    spawn_gate.notify_waiters();
    settle().await;
    eject.await.unwrap().unwrap();
    assert_eq!(engine.spawn_count(), 1);
    assert_eq!(engine.shutdown_counts().0, 1);
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
    settle().await;
    a_control.ready();
    load_a.await.unwrap().unwrap();
    let replacement = tokio::spawn({
        let handle = handle.clone();
        async move { handle.load(model("b")).await }
    });
    settle().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    let shutdown = tokio::spawn({
        let handle = lifecycle.handle();
        async move { handle.shutdown().await }
    });
    let eject = tokio::spawn({
        let handle = lifecycle.handle();
        async move { handle.eject().await }
    });
    settle().await;
    assert!(!shutdown.is_finished() && !eject.is_finished());
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    assert!(shutdown.await.unwrap().is_err());
    assert!(eject.await.unwrap().is_err());
    assert!(replacement.await.unwrap().is_err());
    assert_eq!(engine.spawn_count(), 1);
    assert_eq!(
        lifecycle.handle().snapshot().state,
        LifecycleState::Stopping
    );
}

#[test]
fn app_error_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AppError>();
}
