//! Runtime abstraction shared by all inference engines.
//!
//! This crate owns the engine-neutral half of a runtime integration
//! (`docs/architecture.md` §4): the [`CommandSpec`] argv/environment contract
//! ("no shell"), the bounded executable [`probe`] used to learn a runtime's
//! version and capabilities, and the initial [`diagnose`] `doctor` verdict.
//! Engine-specific adapters live in the `model-serving-runtime-llamacpp` and
//! `model-serving-runtime-ninfer` crates and consume these types.
//!
//! Process supervision builds on [`CommandSpec`] with isolated Unix process
//! groups, bounded stdout/stderr rings, readiness and health deadlines,
//! TERM-to-KILL shutdown, and structured exit classification.

pub mod adapter;
pub mod args;
pub mod command;
pub mod doctor;
pub mod managed_process;
pub mod probe;
pub mod supervisor;

pub use args::fixed_arg_collision;
pub use command::CommandSpec;
pub use doctor::{DoctorReport, DoctorStatus, diagnose, diagnose_with};
pub use managed_process::{
    ExitClassification, ExitReport, ManagedProcess, ProbeStatus, ProcessLogs, ShutdownOutcome,
    SupervisorConfig, SupervisorError,
};
pub use probe::{OutputStream, ProbeConfig, ProbeError, ProbeSnapshot, ProbeStage, probe};
pub use supervisor::{ByteTailRing, ByteTailRingError};

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_crate_is_wired_into_workspace() {
        let _ = module_path!();
    }
}
