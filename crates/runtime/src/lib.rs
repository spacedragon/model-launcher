//! Runtime abstraction shared by all inference engines.
//!
//! This crate owns the engine-neutral half of a runtime integration
//! (`docs/architecture.md` §4): the [`CommandSpec`] argv/environment contract
//! ("no shell"), the bounded executable [`probe`] used to learn a runtime's
//! version and capabilities, and the initial [`diagnose`] `doctor` verdict.
//! Engine-specific adapters live in the `model-serving-runtime-llamacpp` and
//! `model-serving-runtime-ninfer` crates and consume these types.
//!
//! Process *supervision* (process groups, stdout/stderr ring buffers, health
//! deadlines, exit classification) lands in a later job and will build on
//! [`CommandSpec`].

pub mod adapter;
pub mod args;
pub mod command;
pub mod doctor;
pub mod probe;

pub use args::fixed_arg_collision;
pub use command::CommandSpec;
pub use doctor::{DoctorReport, DoctorStatus, diagnose, diagnose_with};
pub use probe::{OutputStream, ProbeConfig, ProbeError, ProbeSnapshot, ProbeStage, probe};

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_crate_is_wired_into_workspace() {
        let _ = module_path!();
    }
}
