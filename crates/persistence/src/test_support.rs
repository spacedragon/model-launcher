//! Test fixture: a temp directory + a temp SQLite file + an already
//! migrated [`crate::SqliteStore`].
//!
//! Deterministic and offline: the database lives in a per-fixture
//! `tempfile::TempDir` (cleaned up on drop), so tests never touch a real
//! data directory and never need the network. Paths use `std::path`, so the
//! fixture works unchanged on Windows and Linux/WSL2.
//!
//! Enabled for this crate's own `#[cfg(test)]` builds automatically, and for
//! dependent crates via the `test-support` feature:
//!
//! ```toml
//! model-serving-persistence = { path = "crates/persistence", features = ["test-support"] }
//! ```

use std::path::PathBuf;

use model_serving_domain::error::Result;

use crate::store::SqliteStore;

/// A migrated temporary SQLite database.
///
/// `dir` is the temp directory (dropped with the fixture); `db_path` is the
/// SQLite file inside it (`test.sqlite` — WAL mode also creates
/// `test.sqlite-wal` / `test.sqlite-shm` siblings).
#[derive(Debug)]
pub struct Fixture {
    /// The temp directory backing this fixture (kept alive; dropped on drop).
    pub dir: tempfile::TempDir,
    /// Path of the SQLite database file.
    pub db_path: PathBuf,
    /// The migrated store connected to `db_path`.
    pub store: SqliteStore,
}

impl Fixture {
    /// Create a fresh temp directory, open (creating) a SQLite database in
    /// it with the mandatory PRAGMAs, and run all migrations.
    ///
    /// # Errors
    ///
    /// The domain `Internal` error code if the database cannot be opened or
    /// migrated.
    pub async fn new() -> Result<Self> {
        let dir = tempfile::tempdir().map_err(|e| {
            model_serving_domain::error::DomainError::with_message(
                model_serving_domain::error::ErrorCode::Internal,
                format!("create temp directory failed: {e}"),
            )
        })?;
        let db_path = dir.path().join("test.sqlite");
        let store = SqliteStore::open(&db_path).await?;
        store.migrate().await?;
        Ok(Self {
            dir,
            db_path,
            store,
        })
    }
}
