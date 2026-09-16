//! Domain model and shared error catalog of the `model-serving` control plane.
//!
//! Pure domain library: **no I/O, HTTP, storage, or async code**. It fixes the
//! dependency direction (everything -> `domain`) described in
//! `docs/architecture.md` §2 and is the single source of truth for the field
//! names and wire shapes in `docs/api.md`.
//!
//! # Contents
//!
//! - [`model`]: core objects — [`model::Model`], [`model::Runtime`],
//!   [`model::Instance`], [`model::Operation`] and [`model::LoadConfig`] — plus
//!   their enums (`ArtifactKind`, `RuntimeKind`, `InstanceState`,
//!   `OperationState`, `FailureClass`, `KvCapacity`, `EvictionPolicy`,
//!   `EvictionTargets`, engine configs).
//! - [`error`]: the stable [`error::ErrorCode`] catalog and the shared
//!   [`error::DomainError`], with pure mappers to the two wire shapes —
//!   OpenAI-style (`/v1/*`) and Problem Details (`/admin/v1/*`).
//! - [`state_machine`]: the pure instance load-state machine and operation
//!   state machine (`can_transition` / `transition`), per `docs/api.md` §5.
//!
//! # Conventions
//!
//! - IDs are opaque stable strings; timestamps are `chrono::DateTime<Utc>` and
//!   serialize as RFC 3339 UTC strings (`docs/api.md` §1).
//! - Serde wire casing follows `docs/api.md`: `snake_case` enums, camelCase
//!   fields (e.g. `contextLength` in the native API is *not* used — the native
//!   and LM Studio shapes are kept in their own DTOs in `api-types`; here the
//!   domain uses its own consistent `snake_case` field names).

pub mod error;
pub mod model;
pub mod state_machine;

#[cfg(test)]
mod tests {
    #[test]
    fn domain_crate_is_wired_into_workspace() {
        // Smoke test: the crate compiles and is reachable from dependents.
        let _ = module_path!();
    }
}
