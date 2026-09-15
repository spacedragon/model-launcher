//! Runtime abstraction shared by all inference engines.
//!
//! Planned content (see `docs/architecture.md` §4): the `RuntimeAdapter`
//! trait (`probe` / `validate` / `command` / `health` / `classify_exit`),
//! `CommandSpec` (argv + controlled environment, no shell) and the process
//! supervisor. Engine-specific adapters live in the
//! `model-serving-runtime-llamacpp` and `model-serving-runtime-ninfer`
//! crates.

pub mod adapter;

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_crate_is_wired_into_workspace() {
        let _ = module_path!();
    }
}
