//! SQLite persistence layer: schema migrations and repositories
//! (`docs/architecture.md` §3, §8, §9).
//!
//! # Responsibility boundary
//!
//! SQLite here serves **crash recovery and audit**, never the inference hot
//! path (`docs/architecture.md` §9): the in-memory instance registry is the
//! routing truth, and every multi-table consistency write (operation +
//! instance desired state + audit event) is committed in one SQLite
//! transaction. Do not add per-inference-request persistence to this crate.
//!
//! # Contents
//!
//! - [`SqliteStore`]: pooled connection with the mandatory PRAGMAs and
//!   `migrate()`; also runs the docs/architecture.md §8 restart-recovery
//!   (`recover_non_terminal_instances`).
//! - [`repos`]: repositories for `models`, `model_roots`, `runtimes`,
//!   `instances`, `operations`, `audit` events and `settings`.
//! - [`mapping`]: the row <-> domain mapping helpers and the error mapping
//!   from `sqlx` failures onto the frozen `model-serving-domain` error
//!   catalog (no new error codes are invented).
//! - [`test_support`] (feature `test-support`, always on for this crate's own
//!   tests): a temp-directory SQLite fixture that is already migrated.
//!
//! # PRAGMA policy
//!
//! Every connection enforces, at connect time (see
//! [`SqliteStore::open`]):
//!
//! - `PRAGMA journal_mode = WAL` (durable across reconnects, once set on the
//!   file);
//! - `PRAGMA foreign_keys = ON` (off by default in SQLite);
//! - `PRAGMA busy_timeout = 10 s` (WAL writers vs. readers).
//!
//! `journal_mode` cannot be switched inside a transaction, so it is applied
//! by the connect options, never in a migration file.
//!
//! # Migration policy
//!
//! Numbered `.sql` files under `migrations/`, applied by sqlx (`migrate()`).
//! **Append-only**: an older database upgrades by running the newer files;
//! shipped files are never edited, and `migrate()` on an already-current
//! database is a no-op. State columns are fenced by named `CHECK`
//! constraints holding the docs/api.md wire tokens, so a state outside the
//! domain state machines can never be stored even by out-of-band writes.

mod mapping;
pub mod repos;
mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use repos::{
    AuditEvent, AuditKind, AuditRepo, InstanceQuery, InstancesRepo, ModelRoot, ModelRootsRepo,
    ModelsRepo, OperationQuery, OperationsRepo, RuntimeRepo, RuntimeWithProbe, Setting,
    SettingsRepo,
};
pub use store::{BUSY_TIMEOUT_SECS, SqliteStore};

#[cfg(test)]
mod tests;
