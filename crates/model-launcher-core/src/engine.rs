use std::{future::Future, pin::Pin, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{AppError, EngineCapabilities, LaunchSettings, ModelRecord};

pub type EngineFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AppError>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSpec {
    pub id: String,
    pub display_name: String,
    pub version: String,
}

pub trait InferenceEngine: Send + Sync {
    fn spec(&self) -> EngineFuture<'_, EngineSpec>;

    fn probe_capabilities(&self) -> EngineFuture<'_, EngineCapabilities>;

    fn validate_launch<'a>(
        &'a self,
        model: &'a ModelRecord,
        settings: &'a LaunchSettings,
    ) -> EngineFuture<'a, ()>;

    fn spawn<'a>(
        &'a self,
        model: &'a ModelRecord,
        settings: &'a LaunchSettings,
    ) -> EngineFuture<'a, Box<dyn EngineProcess>>;
}

pub trait EngineProcess: Send {
    fn wait_ready(&mut self, timeout: Duration) -> EngineFuture<'_, ()>;

    fn check_health(&mut self) -> EngineFuture<'_, ()>;

    fn graceful_shutdown(&mut self) -> EngineFuture<'_, ()>;

    fn force_shutdown(&mut self) -> EngineFuture<'_, ()>;

    fn wait_for_exit(&mut self) -> EngineFuture<'_, i32>;
}
