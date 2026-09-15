//! Domain model of the model-serving control plane.
//!
//! Planned content (see `docs/architecture.md` §3, implemented in a later
//! job): `Model`, `Runtime`, `Instance`, `Operation`, `LoadConfig` and the
//! shared error catalog. Until then this crate only carries the module
//! skeleton so the workspace compiles and the dependency direction
//! (everything -> domain) is fixed from day one.

pub mod error;
pub mod model;

#[cfg(test)]
mod tests {
    #[test]
    fn domain_crate_is_wired_into_workspace() {
        // Smoke test: the crate compiles and is reachable from dependents.
        let _ = module_path!();
    }
}
