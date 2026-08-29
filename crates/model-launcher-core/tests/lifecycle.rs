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
}

struct SpawnScript {
    ready: Arc<Notify>,
    ready_fails: Arc<AtomicBool>,
    exit_rx: oneshot::Receiver<i32>,
}

struct SpawnControl {
    ready: Arc<Notify>,
    ready_fails: Arc<AtomicBool>,
    exit_tx: Option<oneshot::Sender<i32>>,
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
        self.inner.lock().unwrap().scripts.push_back(SpawnScript {
            ready: ready.clone(),
            ready_fails: ready_fails.clone(),
            exit_rx,
        });
        SpawnControl {
            ready,
            ready_fails,
            exit_tx: Some(exit_tx),
        }
    }

    fn spawn_count(&self) -> usize {
        self.inner.lock().unwrap().spawned.len()
    }
}

struct FakeProcess {
    ready: Arc<Notify>,
    ready_fails: Arc<AtomicBool>,
    exit_rx: oneshot::Receiver<i32>,
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
        Box::pin(async { Ok(()) })
    }

    fn spawn<'a>(
        &'a self,
        model: &'a ModelRecord,
        _settings: &'a LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>> {
        Box::pin(async move {
            let mut state = self.inner.lock().unwrap();
            state.spawned.push(model.id);
            let script = state.scripts.pop_front().expect("scripted spawn");
            Ok(Box::new(FakeProcess {
                ready: script.ready,
                ready_fails: script.ready_fails,
                exit_rx: script.exit_rx,
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
        Box::pin(async { Ok(()) })
    }

    fn force_shutdown(&mut self) -> EngineFuture<'_, ()> {
        Box::pin(async { Ok(()) })
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

#[test]
fn app_error_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AppError>();
}
