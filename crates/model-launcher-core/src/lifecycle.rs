use std::{io, sync::Arc, time::Duration};

use tokio::sync::{mpsc, oneshot, watch};

use crate::{AppError, EngineProcess, InferenceEngine, ModelId, ModelRecord};

const READY_TIMEOUT: Duration = Duration::from_secs(30);
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
        let (commands, receiver) = mpsc::channel(32);
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
    commands: mpsc::Sender<Command>,
    snapshots: watch::Sender<LifecycleSnapshot>,
}

impl LifecycleHandle {
    pub async fn load(&self, model: ModelRecord) -> Result<(), AppError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Load { model, reply })
            .await
            .map_err(|_| AppError::EngineUnavailable)?;
        response.await.map_err(|_| AppError::EngineUnavailable)?
    }

    pub async fn acquire(&self, model: ModelRecord) -> Result<InferenceLease, AppError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Acquire { model, reply })
            .await
            .map_err(|_| AppError::EngineUnavailable)?;
        response.await.map_err(|_| AppError::EngineUnavailable)?
    }

    pub async fn eject(&self) -> Result<(), AppError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Eject { reply })
            .await
            .map_err(|_| AppError::EngineUnavailable)?;
        response.await.map_err(|_| AppError::EngineUnavailable)?
    }

    pub async fn shutdown(&self) -> Result<(), AppError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Shutdown { reply })
            .await
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
    commands: mpsc::Sender<Command>,
    generation: u64,
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
        let _ = self.commands.try_send(Command::Release {
            generation: self.generation,
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

async fn select_ready(
    process: &mut dyn EngineProcess,
    commands: &mut mpsc::Receiver<Command>,
) -> ProcessEvent<()> {
    tokio::select! {
        result = process.wait_ready(READY_TIMEOUT) => ProcessEvent::Process(result),
        command = commands.recv() => ProcessEvent::Command(command),
    }
}

async fn select_exit(
    process: &mut dyn EngineProcess,
    commands: &mut mpsc::Receiver<Command>,
) -> ProcessEvent<i32> {
    tokio::select! {
        result = process.wait_for_exit() => ProcessEvent::Process(result),
        command = commands.recv() => ProcessEvent::Command(command),
    }
}

struct Actor {
    engine: Arc<dyn InferenceEngine>,
    commands_tx: mpsc::Sender<Command>,
    commands: mpsc::Receiver<Command>,
    snapshots: watch::Sender<LifecycleSnapshot>,
    snapshot: LifecycleSnapshot,
    desired: Option<ModelRecord>,
    failures: u32,
    lease_cancel: watch::Sender<bool>,
}

async fn run_actor(
    engine: Arc<dyn InferenceEngine>,
    commands_tx: mpsc::Sender<Command>,
    commands: mpsc::Receiver<Command>,
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

        if let Err(error) = self
            .engine
            .validate_launch(&model, &model.launch_profile.settings)
            .await
        {
            self.snapshot.diagnostic = Some(error.to_string());
            self.desired = None;
            self.set_state(LifecycleState::FailedValidation);
            fail_waiters(loads, acquires, error);
            self.set_state(LifecycleState::Stopped);
            return true;
        }

        let process = self
            .engine
            .spawn(&model, &model.launch_profile.settings)
            .await;
        let mut process = match process {
            Ok(process) => process,
            Err(error) => {
                self.load_failed(loads, acquires, error);
                return true;
            }
        };
        self.snapshot.process = Some(ProcessContext {
            model_id: model.id,
            generation,
        });
        self.publish();

        loop {
            match select_ready(&mut *process, &mut self.commands).await {
                ProcessEvent::Process(result) => match result {
                    Ok(()) => {
                        self.set_state(LifecycleState::Running);
                        for reply in loads.drain(..) {
                            let _ = reply.send(Ok(()));
                        }
                        self.grant_acquires(&mut acquires, generation);
                        return Box::pin(self.running(process, model, generation)).await;
                    }
                    Err(error) => {
                        self.load_failed(loads, acquires, error);
                        return true;
                    }
                },
                ProcessEvent::Command(command) => match command {
                    Some(Command::Load { model: same, reply }) if same.id == model.id => {
                        loads.push(reply)
                    }
                    Some(Command::Acquire { model: same, reply }) if same.id == model.id => {
                        acquires.push(reply)
                    }
                    Some(Command::Eject { reply }) => {
                        self.stop_process(&mut *process).await;
                        self.clear_desired();
                        fail_cancelled(loads, acquires);
                        let _ = reply.send(Ok(()));
                        return true;
                    }
                    Some(Command::Shutdown { reply }) => {
                        self.stop_process(&mut *process).await;
                        self.clear_desired();
                        fail_cancelled(loads, acquires);
                        let _ = reply.send(Ok(()));
                        return false;
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
                    if started.elapsed() >= HEALTHY_RESET {
                        self.failures = 0;
                    }
                    self.failures = self.failures.saturating_add(1);
                    return Box::pin(self.backoff(model, generation)).await;
                }
                ProcessEvent::Command(command) => match command {
                    Some(Command::Acquire { model: same, reply }) if same.id == model.id => {
                        self.snapshot.in_flight += 1;
                        self.publish();
                        let _ = reply.send(Ok(self.lease(generation)));
                    }
                    Some(Command::Load { model: same, reply }) if same.id == model.id => {
                        let _ = reply.send(Ok(()));
                    }
                    Some(Command::Load { model: next, reply }) => {
                        if self.snapshot.in_flight > 0 {
                            let _ = reply.send(Err(AppError::ModelBusy));
                        } else {
                            self.stop_process(&mut *process).await;
                            self.failures = 0;
                            return Box::pin(self.start(next, vec![reply], Vec::new())).await;
                        }
                    }
                    Some(Command::Acquire { reply, .. }) => {
                        let _ = reply.send(Err(AppError::ModelBusy));
                    }
                    Some(Command::Release {
                        generation: released,
                    }) if released == generation => {
                        self.snapshot.in_flight = self.snapshot.in_flight.saturating_sub(1);
                        self.publish();
                    }
                    Some(Command::Release { .. }) => {}
                    Some(Command::Eject { reply }) => {
                        self.cancel_leases();
                        self.stop_process(&mut *process).await;
                        self.clear_desired();
                        let _ = reply.send(Ok(()));
                        return true;
                    }
                    Some(Command::Shutdown { reply }) => {
                        self.cancel_leases();
                        self.stop_process(&mut *process).await;
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
                    Some(Command::Acquire { reply, .. }) => { let _ = reply.send(Err(AppError::ModelBusy)); }
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

    fn grant_acquires(
        &mut self,
        acquires: &mut Vec<oneshot::Sender<Result<InferenceLease, AppError>>>,
        generation: u64,
    ) {
        for reply in acquires.drain(..) {
            self.snapshot.in_flight += 1;
            let _ = reply.send(Ok(self.lease(generation)));
        }
        self.publish();
    }

    fn lease(&self, generation: u64) -> InferenceLease {
        InferenceLease {
            commands: self.commands_tx.clone(),
            generation,
            cancelled: self.lease_cancel.subscribe(),
        }
    }

    fn cancel_leases(&mut self) {
        self.lease_cancel.send_replace(true);
        let (lease_cancel, _) = watch::channel(false);
        self.lease_cancel = lease_cancel;
        self.snapshot.in_flight = 0;
        self.publish();
    }

    async fn stop_process(&mut self, process: &mut dyn EngineProcess) {
        self.set_state(LifecycleState::Stopping);
        if process.graceful_shutdown().await.is_err() {
            let _ = process.force_shutdown().await;
        }
        self.snapshot.process = None;
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
        fail_waiters(loads, acquires, load_error(&diagnostic));
        self.set_state(LifecycleState::Stopped);
    }
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
