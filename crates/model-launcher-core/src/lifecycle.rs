use std::{collections::HashSet, io, sync::Arc, time::Duration};

use tokio::sync::{mpsc, oneshot, watch};

use crate::{AppError, EngineProcess, InferenceEngine, ModelId, ModelRecord};

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTHY_RESET: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Backoff,
    FailedValidation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessContext {
    pub model_id: ModelId,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleSnapshot {
    pub state: LifecycleState,
    pub desired_model: Option<ModelId>,
    pub generation: u64,
    pub in_flight: usize,
    pub process: Option<ProcessContext>,
    pub diagnostic: Option<String>,
}

impl Default for LifecycleSnapshot {
    fn default() -> Self {
        Self {
            state: LifecycleState::Stopped,
            desired_model: None,
            generation: 0,
            in_flight: 0,
            process: None,
            diagnostic: None,
        }
    }
}

pub struct Lifecycle {
    handle: LifecycleHandle,
}

impl Lifecycle {
    #[must_use]
    pub fn spawn(engine: Arc<dyn InferenceEngine>) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (snapshots, _) = watch::channel(LifecycleSnapshot::default());
        let handle = LifecycleHandle {
            commands,
            snapshots: snapshots.clone(),
        };
        tokio::spawn(run_actor(
            engine,
            handle.commands.clone(),
            receiver,
            snapshots,
        ));
        Self { handle }
    }

    #[must_use]
    pub fn handle(&self) -> LifecycleHandle {
        self.handle.clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<LifecycleSnapshot> {
        self.handle.subscribe()
    }
}

#[derive(Clone)]
pub struct LifecycleHandle {
    commands: mpsc::UnboundedSender<Command>,
    snapshots: watch::Sender<LifecycleSnapshot>,
}

impl LifecycleHandle {
    pub async fn load(&self, model: ModelRecord) -> Result<(), AppError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Load { model, reply })
            .map_err(|_| AppError::EngineUnavailable)?;
        response.await.map_err(|_| AppError::EngineUnavailable)?
    }

    pub async fn acquire(&self, model: ModelRecord) -> Result<InferenceLease, AppError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Acquire { model, reply })
            .map_err(|_| AppError::EngineUnavailable)?;
        response.await.map_err(|_| AppError::EngineUnavailable)?
    }

    pub async fn eject(&self) -> Result<(), AppError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Eject { reply })
            .map_err(|_| AppError::EngineUnavailable)?;
        response.await.map_err(|_| AppError::EngineUnavailable)?
    }

    pub async fn shutdown(&self) -> Result<(), AppError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Shutdown { reply })
            .map_err(|_| AppError::EngineUnavailable)?;
        response.await.map_err(|_| AppError::EngineUnavailable)?
    }

    #[must_use]
    pub fn snapshot(&self) -> LifecycleSnapshot {
        self.snapshots.borrow().clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<LifecycleSnapshot> {
        self.snapshots.subscribe()
    }
}

pub struct InferenceLease {
    commands: mpsc::UnboundedSender<Command>,
    generation: u64,
    id: u64,
    cancelled: watch::Receiver<bool>,
}

impl InferenceLease {
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    pub async fn cancelled(&mut self) {
        if !*self.cancelled.borrow() {
            let _ = self.cancelled.changed().await;
        }
    }
}

impl Drop for InferenceLease {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Release {
            generation: self.generation,
            id: self.id,
        });
    }
}

enum Command {
    Load {
        model: ModelRecord,
        reply: oneshot::Sender<Result<(), AppError>>,
    },
    Acquire {
        model: ModelRecord,
        reply: oneshot::Sender<Result<InferenceLease, AppError>>,
    },
    Release {
        generation: u64,
        id: u64,
    },
    Eject {
        reply: oneshot::Sender<Result<(), AppError>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), AppError>>,
    },
}

enum ProcessEvent<T> {
    Process(Result<T, AppError>),
    Command(Option<Command>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StopDirective {
    Continue,
    Cancel,
    Shutdown,
}

enum StopEvent {
    Graceful(Result<(), AppError>),
    Timeout,
    Command(Option<Command>),
}

enum StartOutcome {
    Ready(Box<dyn EngineProcess>),
    ValidationFailed(AppError),
    LoadFailed(AppError),
    Cancelled,
    CleanupFailed(String),
}

enum ReadinessOutcome {
    Ready,
    Failed(AppError),
    TimedOut,
    Cancelled,
}

async fn await_readiness(
    process: &mut dyn EngineProcess,
    cancelled: &mut watch::Receiver<bool>,
) -> ReadinessOutcome {
    tokio::select! {
        result = process.wait_ready(READY_TIMEOUT) => match result {
            Ok(()) => ReadinessOutcome::Ready,
            Err(error) => ReadinessOutcome::Failed(error),
        },
        () = tokio::time::sleep(READY_TIMEOUT) => ReadinessOutcome::TimedOut,
        _ = cancelled.changed() => ReadinessOutcome::Cancelled,
    }
}

async fn graceful_result(process: &mut dyn EngineProcess) -> Result<(), AppError> {
    match tokio::time::timeout(STOP_TIMEOUT, process.graceful_shutdown()).await {
        Ok(result) => result,
        Err(_) => Err(load_error("graceful shutdown timed out")),
    }
}

async fn launch_process(
    engine: Arc<dyn InferenceEngine>,
    model: ModelRecord,
    mut cancelled: watch::Receiver<bool>,
) -> StartOutcome {
    let validation = engine.validate_launch(&model, &model.launch_profile.settings);
    tokio::pin!(validation);
    tokio::select! {
        result = &mut validation => if let Err(error) = result {
            return StartOutcome::ValidationFailed(error);
        },
        _ = cancelled.changed() => return StartOutcome::Cancelled,
    }
    if *cancelled.borrow() {
        return StartOutcome::Cancelled;
    }

    // Spawn is deliberately allowed to finish after cancellation. Dropping an arbitrary engine's
    // spawn future is not guaranteed to undo a child it already created; a stale returned process
    // is instead cleaned up below before the task exits.
    let mut process = match engine.spawn(&model, &model.launch_profile.settings).await {
        Ok(process) => process,
        Err(_error) if *cancelled.borrow() => return StartOutcome::Cancelled,
        Err(error) => return StartOutcome::LoadFailed(error),
    };
    if *cancelled.borrow() {
        return match stop_owned_process(&mut *process).await {
            Ok(()) => StartOutcome::Cancelled,
            Err(error) => StartOutcome::CleanupFailed(error.to_string()),
        };
    }

    match await_readiness(&mut *process, &mut cancelled).await {
        ReadinessOutcome::Ready => StartOutcome::Ready(process),
        ReadinessOutcome::Failed(error) => match stop_owned_process(&mut *process).await {
            Ok(()) => StartOutcome::LoadFailed(error),
            Err(stop) => StartOutcome::CleanupFailed(stop.to_string()),
        },
        ReadinessOutcome::TimedOut => match stop_owned_process(&mut *process).await {
            Ok(()) => StartOutcome::LoadFailed(load_error("engine readiness timed out")),
            Err(stop) => StartOutcome::CleanupFailed(stop.to_string()),
        },
        ReadinessOutcome::Cancelled => match stop_owned_process(&mut *process).await {
            Ok(()) => StartOutcome::Cancelled,
            Err(error) => StartOutcome::CleanupFailed(error.to_string()),
        },
    }
}

async fn stop_owned_process(process: &mut dyn EngineProcess) -> Result<(), AppError> {
    if graceful_result(process).await.is_err() {
        match tokio::time::timeout(STOP_TIMEOUT, process.force_shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(AppError::EngineProcess(Box::new(io::Error::other(
                    "force shutdown timed out",
                ))));
            }
        }
    }
    Ok(())
}

async fn select_exit(
    process: &mut dyn EngineProcess,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> ProcessEvent<i32> {
    tokio::select! {
        result = process.wait_for_exit() => ProcessEvent::Process(result),
        command = commands.recv() => ProcessEvent::Command(command),
    }
}

struct Actor {
    engine: Arc<dyn InferenceEngine>,
    commands_tx: mpsc::UnboundedSender<Command>,
    commands: mpsc::UnboundedReceiver<Command>,
    snapshots: watch::Sender<LifecycleSnapshot>,
    snapshot: LifecycleSnapshot,
    desired: Option<ModelRecord>,
    failures: u32,
    lease_cancel: watch::Sender<bool>,
    active_leases: HashSet<u64>,
    next_lease_id: u64,
}

async fn run_actor(
    engine: Arc<dyn InferenceEngine>,
    commands_tx: mpsc::UnboundedSender<Command>,
    commands: mpsc::UnboundedReceiver<Command>,
    snapshots: watch::Sender<LifecycleSnapshot>,
) {
    let (lease_cancel, _) = watch::channel(false);
    let mut actor = Actor {
        engine,
        commands_tx,
        commands,
        snapshots,
        snapshot: LifecycleSnapshot::default(),
        desired: None,
        failures: 0,
        lease_cancel,
        active_leases: HashSet::new(),
        next_lease_id: 1,
    };
    actor.stopped().await;
}

impl Actor {
    fn publish(&mut self) {
        self.snapshot.desired_model = self.desired.as_ref().map(|model| model.id);
        self.snapshots.send_replace(self.snapshot.clone());
    }

    fn set_state(&mut self, state: LifecycleState) {
        self.snapshot.state = state;
        self.publish();
    }

    async fn stopped(&mut self) {
        while let Some(command) = self.commands.recv().await {
            match command {
                Command::Load { model, reply } => {
                    if !self.start(model, vec![reply], Vec::new()).await {
                        return;
                    }
                }
                Command::Acquire { model, reply } => {
                    if !self.start(model, Vec::new(), vec![reply]).await {
                        return;
                    }
                }
                Command::Release { .. } => {}
                Command::Eject { reply } => {
                    self.clear_desired();
                    let _ = reply.send(Ok(()));
                }
                Command::Shutdown { reply } => {
                    self.clear_desired();
                    let _ = reply.send(Ok(()));
                    return;
                }
            }
        }
    }

    async fn start(
        &mut self,
        model: ModelRecord,
        mut loads: Vec<oneshot::Sender<Result<(), AppError>>>,
        mut acquires: Vec<oneshot::Sender<Result<InferenceLease, AppError>>>,
    ) -> bool {
        self.snapshot.generation += 1;
        let generation = self.snapshot.generation;
        self.desired = Some(model.clone());
        self.snapshot.diagnostic = None;
        self.set_state(LifecycleState::Starting);
        let (cancel, cancelled) = watch::channel(false);
        let mut launch = tokio::spawn(launch_process(
            self.engine.clone(),
            model.clone(),
            cancelled,
        ));
        let mut cancelling = false;
        let mut shutdown_after_cancel = false;
        let mut stop_waiters = Vec::new();
        loop {
            tokio::select! {
                outcome = &mut launch => match outcome.unwrap_or_else(|error| StartOutcome::LoadFailed(load_error(&error.to_string()))) {
                    StartOutcome::Ready(mut process) if cancelling => {
                        launch = tokio::spawn(async move {
                            match stop_owned_process(&mut *process).await {
                                Ok(()) => StartOutcome::Cancelled,
                                Err(error) => StartOutcome::CleanupFailed(error.to_string()),
                            }
                        });
                        continue;
                    }
                    StartOutcome::ValidationFailed(_) | StartOutcome::LoadFailed(_) if cancelling => {
                        fail_cancelled(loads, acquires);
                        self.clear_desired();
                        send_stop_waiters(stop_waiters, None);
                        return !shutdown_after_cancel;
                    }
                    StartOutcome::Ready(process) => {
                        self.snapshot.process = Some(ProcessContext { model_id: model.id, generation });
                        self.set_state(LifecycleState::Running);
                        for reply in loads.drain(..) {
                            let _ = reply.send(Ok(()));
                        }
                        self.grant_acquires(&mut acquires, generation);
                        return Box::pin(self.running(process, model, generation)).await;
                    }
                    StartOutcome::ValidationFailed(error) => {
                        self.snapshot.diagnostic = Some(error.to_string());
                        self.desired = None;
                        self.set_state(LifecycleState::FailedValidation);
                        fail_waiters(loads, acquires, error);
                        self.set_state(LifecycleState::Stopped);
                        return true;
                    }
                    StartOutcome::LoadFailed(error) => {
                        self.load_failed(loads, acquires, error);
                        return true;
                    }
                    StartOutcome::Cancelled => {
                        fail_cancelled(loads, acquires);
                        self.clear_desired();
                        send_stop_waiters(stop_waiters, None);
                        return !shutdown_after_cancel;
                    }
                    StartOutcome::CleanupFailed(diagnostic) => {
                        fail_cancelled(loads, acquires);
                        send_stop_waiters(stop_waiters, Some(&diagnostic));
                        self.snapshot.diagnostic = Some(diagnostic);
                        self.set_state(LifecycleState::Stopping);
                        return self.failed_stop().await;
                    }
                },
                command = self.commands.recv() => match command {
                    Some(Command::Load { reply, .. }) if cancelling => {
                        let _ = reply.send(Err(AppError::ModelStarting));
                    }
                    Some(Command::Acquire { reply, .. }) if cancelling => {
                        let _ = reply.send(Err(AppError::ModelStarting));
                    }
                    Some(Command::Load { model: same, reply }) if same.id == model.id => {
                        loads.push(reply)
                    }
                    Some(Command::Acquire { model: same, reply }) if same.id == model.id => {
                        acquires.push(reply)
                    }
                    Some(Command::Eject { reply }) => {
                        cancel.send_replace(true);
                        cancelling = true;
                        self.set_state(LifecycleState::Stopping);
                        stop_waiters.push(reply);
                    }
                    Some(Command::Shutdown { reply }) => {
                        cancel.send_replace(true);
                        cancelling = true;
                        shutdown_after_cancel = true;
                        self.set_state(LifecycleState::Stopping);
                        stop_waiters.push(reply);
                    }
                    Some(Command::Load { reply, .. }) => {
                        let _ = reply.send(Err(AppError::ModelStarting));
                    }
                    Some(Command::Acquire { reply, .. }) => {
                        let _ = reply.send(Err(AppError::ModelStarting));
                    }
                    Some(Command::Release { .. }) => {}
                    None => return false,
                },
            }
        }
    }

    async fn running(
        &mut self,
        mut process: Box<dyn EngineProcess>,
        model: ModelRecord,
        generation: u64,
    ) -> bool {
        let started = tokio::time::Instant::now();
        loop {
            match select_exit(&mut *process, &mut self.commands).await {
                ProcessEvent::Process(result) => {
                    let diagnostic = match result {
                        Ok(code) => format!("engine exited with code {code}"),
                        Err(error) => error.to_string(),
                    };
                    self.snapshot.process = None;
                    self.snapshot.diagnostic = Some(diagnostic);
                    self.cancel_leases();
                    if started.elapsed() >= HEALTHY_RESET {
                        self.failures = 0;
                    }
                    self.failures = self.failures.saturating_add(1);
                    return Box::pin(self.backoff(model, generation)).await;
                }
                ProcessEvent::Command(command) => match command {
                    Some(Command::Acquire { model: same, reply }) if same.id == model.id => {
                        let lease = self.new_lease(generation);
                        let id = lease.id;
                        if reply.send(Ok(lease)).is_ok() {
                            self.active_leases.insert(id);
                            self.sync_in_flight();
                        }
                    }
                    Some(Command::Load { model: same, reply }) if same.id == model.id => {
                        let _ = reply.send(Ok(()));
                    }
                    Some(Command::Load { model: next, reply }) => {
                        if self.snapshot.in_flight > 0 {
                            let _ = reply.send(Err(AppError::ModelBusy));
                        } else {
                            let (directive, failure) = self.stop_process(&mut *process).await;
                            self.failures = 0;
                            if let Some(diagnostic) = failure {
                                let _ = reply.send(Err(stop_error(&diagnostic)));
                                self.snapshot.diagnostic = Some(diagnostic);
                                self.set_state(LifecycleState::Stopping);
                                return self.failed_stop().await;
                            }
                            if directive != StopDirective::Continue {
                                let _ = reply.send(Err(load_error("replacement cancelled")));
                                self.clear_desired();
                                return directive != StopDirective::Shutdown;
                            }
                            return Box::pin(self.start(next, vec![reply], Vec::new())).await;
                        }
                    }
                    Some(Command::Acquire { reply, .. }) => {
                        let _ = reply.send(Err(AppError::ModelBusy));
                    }
                    Some(Command::Release {
                        generation: released,
                        id,
                    }) if released == generation => {
                        if self.active_leases.remove(&id) {
                            self.sync_in_flight();
                        }
                    }
                    Some(Command::Release { .. }) => {}
                    Some(Command::Eject { reply }) => {
                        self.cancel_leases();
                        let (directive, failure) = self.stop_process(&mut *process).await;
                        if let Some(diagnostic) = failure {
                            let _ = reply.send(Err(stop_error(&diagnostic)));
                            self.snapshot.diagnostic = Some(diagnostic);
                            self.set_state(LifecycleState::Stopping);
                            return self.failed_stop().await;
                        }
                        self.clear_desired();
                        let _ = reply.send(Ok(()));
                        return directive != StopDirective::Shutdown;
                    }
                    Some(Command::Shutdown { reply }) => {
                        self.cancel_leases();
                        let (_, failure) = self.stop_process(&mut *process).await;
                        if let Some(diagnostic) = failure {
                            let _ = reply.send(Err(stop_error(&diagnostic)));
                            self.snapshot.diagnostic = Some(diagnostic);
                            self.set_state(LifecycleState::Stopping);
                            return self.failed_stop().await;
                        }
                        self.clear_desired();
                        let _ = reply.send(Ok(()));
                        return false;
                    }
                    None => return false,
                },
            }
        }
    }

    async fn backoff(&mut self, model: ModelRecord, generation: u64) -> bool {
        self.set_state(LifecycleState::Backoff);
        let exponent = self.failures.saturating_sub(1).min(5);
        let delay = Duration::from_secs((1_u64 << exponent).min(30));
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => {
                    if self.snapshot.generation == generation && self.desired.as_ref().map(|m| m.id) == Some(model.id) {
                        return Box::pin(self.start(model, Vec::new(), Vec::new())).await;
                    }
                    return true;
                }
                command = self.commands.recv() => match command {
                    Some(Command::Load { model: same, reply }) if same.id == model.id => {
                        let _ = reply.send(Err(AppError::ModelStarting));
                    }
                    Some(Command::Acquire { model: same, reply }) if same.id == model.id => {
                        let _ = reply.send(Err(AppError::ModelStarting));
                    }
                    Some(Command::Load { model: next, reply }) => {
                        self.failures = 0;
                        return Box::pin(self.start(next, vec![reply], Vec::new())).await;
                    }
                    Some(Command::Acquire { model: next, reply }) => {
                        self.failures = 0;
                        return Box::pin(self.start(next, Vec::new(), vec![reply])).await;
                    }
                    Some(Command::Release { .. }) => {}
                    Some(Command::Eject { reply }) => {
                        self.clear_desired();
                        let _ = reply.send(Ok(()));
                        return true;
                    }
                    Some(Command::Shutdown { reply }) => {
                        self.clear_desired();
                        let _ = reply.send(Ok(()));
                        return false;
                    }
                    None => return false,
                }
            }
        }
    }

    async fn failed_stop(&mut self) -> bool {
        let diagnostic = self
            .snapshot
            .diagnostic
            .clone()
            .unwrap_or_else(|| "process stop was not confirmed".to_owned());
        while let Some(command) = self.commands.recv().await {
            match command {
                Command::Load { reply, .. } => {
                    let _ = reply.send(Err(AppError::ModelStarting));
                }
                Command::Acquire { reply, .. } => {
                    let _ = reply.send(Err(AppError::ModelStarting));
                }
                Command::Release { id, .. } => {
                    if self.active_leases.remove(&id) {
                        self.sync_in_flight();
                    }
                }
                Command::Eject { reply } | Command::Shutdown { reply } => {
                    let _ = reply.send(Err(stop_error(&diagnostic)));
                }
            }
        }
        false
    }

    fn grant_acquires(
        &mut self,
        acquires: &mut Vec<oneshot::Sender<Result<InferenceLease, AppError>>>,
        generation: u64,
    ) {
        for reply in acquires.drain(..) {
            let lease = self.new_lease(generation);
            let id = lease.id;
            if reply.send(Ok(lease)).is_ok() {
                self.active_leases.insert(id);
            }
        }
        self.sync_in_flight();
    }

    fn new_lease(&mut self, generation: u64) -> InferenceLease {
        let id = self.next_lease_id;
        self.next_lease_id = self.next_lease_id.wrapping_add(1);
        InferenceLease {
            commands: self.commands_tx.clone(),
            generation,
            id,
            cancelled: self.lease_cancel.subscribe(),
        }
    }

    fn sync_in_flight(&mut self) {
        self.snapshot.in_flight = self.active_leases.len();
        self.publish();
    }

    fn cancel_leases(&mut self) {
        self.lease_cancel.send_replace(true);
        let (lease_cancel, _) = watch::channel(false);
        self.lease_cancel = lease_cancel;
        self.active_leases.clear();
        self.sync_in_flight();
    }

    async fn stop_process(
        &mut self,
        process: &mut dyn EngineProcess,
    ) -> (StopDirective, Option<String>) {
        self.set_state(LifecycleState::Stopping);
        let mut directive = StopDirective::Continue;
        let mut waiters = Vec::new();
        let force = {
            let graceful = process.graceful_shutdown();
            tokio::pin!(graceful);
            let timeout = tokio::time::sleep(STOP_TIMEOUT);
            tokio::pin!(timeout);
            loop {
                let event = tokio::select! {
                    result = &mut graceful => StopEvent::Graceful(result),
                    () = &mut timeout => StopEvent::Timeout,
                    command = self.commands.recv() => StopEvent::Command(command),
                };
                match event {
                    StopEvent::Graceful(result) => break result.is_err(),
                    StopEvent::Timeout => break true,
                    StopEvent::Command(command) => {
                        self.handle_stop_command(command, &mut directive, &mut waiters)
                    }
                }
            }
        };
        let mut failure = None;
        if force {
            let forced = {
                let force = process.force_shutdown();
                tokio::pin!(force);
                let timeout = tokio::time::sleep(STOP_TIMEOUT);
                tokio::pin!(timeout);
                loop {
                    let event = tokio::select! {
                        result = &mut force => StopEvent::Graceful(result),
                        () = &mut timeout => StopEvent::Timeout,
                        command = self.commands.recv() => StopEvent::Command(command),
                    };
                    match event {
                        StopEvent::Graceful(result) => {
                            break result.map_err(|error| error.to_string());
                        }
                        StopEvent::Timeout => break Err("force shutdown timed out".to_owned()),
                        StopEvent::Command(command) => {
                            self.handle_stop_command(command, &mut directive, &mut waiters)
                        }
                    }
                }
            };
            failure = forced.err();
        }
        if failure.is_none() {
            self.snapshot.process = None;
        }
        send_stop_waiters(waiters, failure.as_deref());
        (directive, failure)
    }

    fn handle_stop_command(
        &mut self,
        command: Option<Command>,
        directive: &mut StopDirective,
        waiters: &mut Vec<oneshot::Sender<Result<(), AppError>>>,
    ) {
        match command {
            Some(Command::Release { id, .. }) => {
                if self.active_leases.remove(&id) {
                    self.sync_in_flight();
                }
            }
            Some(Command::Eject { reply }) => {
                if *directive != StopDirective::Shutdown {
                    *directive = StopDirective::Cancel;
                }
                self.desired = None;
                self.publish();
                waiters.push(reply);
            }
            Some(Command::Shutdown { reply }) => {
                *directive = StopDirective::Shutdown;
                self.desired = None;
                self.publish();
                waiters.push(reply);
            }
            Some(Command::Load { reply, .. }) => {
                let _ = reply.send(Err(AppError::ModelStarting));
            }
            Some(Command::Acquire { reply, .. }) => {
                let _ = reply.send(Err(AppError::ModelStarting));
            }
            None => *directive = StopDirective::Shutdown,
        }
    }

    fn clear_desired(&mut self) {
        self.desired = None;
        self.snapshot.generation += 1;
        self.snapshot.process = None;
        self.snapshot.diagnostic = None;
        self.set_state(LifecycleState::Stopped);
    }

    fn load_failed(
        &mut self,
        loads: Vec<oneshot::Sender<Result<(), AppError>>>,
        acquires: Vec<oneshot::Sender<Result<InferenceLease, AppError>>>,
        error: AppError,
    ) {
        let diagnostic = error.to_string();
        self.snapshot.process = None;
        self.snapshot.diagnostic = Some(diagnostic.clone());
        self.desired = None;
        fail_waiters_shared(loads, acquires, Arc::new(error));
        self.set_state(LifecycleState::Stopped);
    }
}

fn fail_waiters_shared(
    loads: Vec<oneshot::Sender<Result<(), AppError>>>,
    acquires: Vec<oneshot::Sender<Result<InferenceLease, AppError>>>,
    source: Arc<AppError>,
) {
    for reply in loads {
        let _ = reply.send(Err(AppError::ModelLoadFailed(Box::new(source.clone()))));
    }
    for reply in acquires {
        let _ = reply.send(Err(AppError::ModelLoadFailed(Box::new(source.clone()))));
    }
}

fn send_stop_waiters(
    waiters: Vec<oneshot::Sender<Result<(), AppError>>>,
    diagnostic: Option<&str>,
) {
    for reply in waiters {
        let result = diagnostic.map_or_else(|| Ok(()), |message| Err(stop_error(message)));
        let _ = reply.send(result);
    }
}

fn stop_error(diagnostic: &str) -> AppError {
    AppError::EngineProcess(Box::new(io::Error::other(diagnostic.to_owned())))
}

fn fail_waiters(
    loads: Vec<oneshot::Sender<Result<(), AppError>>>,
    acquires: Vec<oneshot::Sender<Result<InferenceLease, AppError>>>,
    error: AppError,
) {
    let diagnostic = error.to_string();
    let mut first = Some(error);
    for reply in loads {
        let error = first.take().unwrap_or_else(|| load_error(&diagnostic));
        let _ = reply.send(Err(error));
    }
    for reply in acquires {
        let error = first.take().unwrap_or_else(|| load_error(&diagnostic));
        let _ = reply.send(Err(error));
    }
}

fn fail_cancelled(
    loads: Vec<oneshot::Sender<Result<(), AppError>>>,
    acquires: Vec<oneshot::Sender<Result<InferenceLease, AppError>>>,
) {
    fail_waiters(loads, acquires, load_error("load cancelled"));
}

fn load_error(diagnostic: &str) -> AppError {
    AppError::ModelLoadFailed(Box::new(io::Error::other(diagnostic.to_owned())))
}
