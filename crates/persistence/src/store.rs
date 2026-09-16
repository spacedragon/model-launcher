//! The pooled SQLite endpoint, PRAGMA enforcement, migration runner,
//! explicit transaction API and the §8 restart-recovery entry point.

use std::path::Path;
use std::time::Duration;

use model_serving_domain::error::{DomainError, ErrorCode, Result};
use model_serving_domain::model::{Instance, InstanceState};
use model_serving_domain::state_machine::transition_instance;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteTransaction};

use crate::mapping::{now, storage_error, ts_string, wire_token};
use crate::repos::{AuditEvent, AuditKind, AuditRepo, InstancesRepo};

/// The `busy_timeout` enforced on every connection, in seconds.
pub const BUSY_TIMEOUT_SECS: u64 = 10;

/// Ensure the on-disk database file is owner-only (`0600`), per
/// `docs/security-and-deployment.md` (the file holds credentials-adjacent
/// state and must not be group/world readable).
///
/// If the file does not yet exist it is created owner-only up front (so the
/// first byte SQLite writes already lands in a `0600` file — no permissive
/// window); if it exists and is more permissive than `0600` (any group/other
/// bit set) it is tightened to `0600`. The `set_permissions` call is what makes
/// the result deterministic regardless of the process `umask`.
///
/// Unix-only: on Windows the file-attribute model differs and this is a no-op.
///
/// # Errors
///
/// [`ErrorCode::Internal`] if the file cannot be stat-ed, created, or `chmod`-ed.
#[cfg(unix)]
fn secure_db_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| {
                    DomainError::with_message(
                        ErrorCode::Internal,
                        format!("create database file {path:?}: {e}"),
                    )
                })?;
            std::fs::metadata(path).map_err(|e| {
                DomainError::with_message(
                    ErrorCode::Internal,
                    format!("stat database file {path:?}: {e}"),
                )
            })?
        }
        Err(e) => {
            return Err(DomainError::with_message(
                ErrorCode::Internal,
                format!("stat database file {path:?}: {e}"),
            ));
        }
    };
    // Deterministically force owner-only (0600) regardless of umask.
    if meta.permissions().mode() & 0o777 != 0o600 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            DomainError::with_message(
                ErrorCode::Internal,
                format!("set database file {path:?} permissions to 0600: {e}"),
            )
        })?;
    }
    Ok(())
}

/// A pooled SQLite connection with the mandatory PRAGMAs applied at connect
/// time: `journal_mode = WAL`, `foreign_keys = ON`,
/// `busy_timeout = 10 s` (policy in the crate docs).
///
/// `SqliteStore` is the shared endpoint for a daemon process. Repositories
/// take an explicit `sqlx::Executor` (either `store.pool()` for
/// single-statement work or a handle from [`SqliteStore::transaction`] for
/// multi-table writes) so that `docs/architecture.md` §9 consistency writes
/// — operation + instance desired state + audit event — commit atomically.
///
/// `SqliteStore` is `Sync`; clone the cheap pool, do not clone the store
/// between unrelated async tasks unless you intend to share the pool.
#[derive(Debug)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating the file if missing) the SQLite database at `path` and
    /// connect with `journal_mode = WAL`, `foreign_keys = ON` and a
    /// 10-second `busy_timeout` on every pool connection.
    ///
    /// # Errors
    ///
    /// The domain `Internal` error code if the database cannot be opened.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        #[cfg(unix)]
        secure_db_file(path)?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECS));
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .map_err(|e| storage_error(&e, "open sqlite database"))?;
        Ok(Self { pool })
    }

    /// Run the embedded `migrations/` against the database.
    ///
    /// Idempotent: already-applied migrations are a no-op, so `migrate()` is
    /// safe to call on every daemon start (append-only upgrade path).
    ///
    /// # Errors
    ///
    /// The domain `Internal` error code if a migration fails to apply.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!().run(&self.pool).await.map_err(|e| {
            DomainError::with_message(ErrorCode::Internal, format!("run migrations: {e}"))
        })
    }

    /// The underlying pool (use as the repository executor for
    /// single-statement work).
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Begin an explicit transaction for a multi-table consistency write
    /// (`docs/architecture.md` §9). Run repository calls with `&mut *tx` as
    /// the executor; the caller commits or rolls back.
    ///
    /// # Errors
    ///
    /// The domain `Internal` error code if the transaction cannot be
    /// started.
    pub async fn transaction(&self) -> Result<SqliteTransaction<'static>> {
        self.pool
            .begin()
            .await
            .map_err(|e| storage_error(&e, "begin transaction"))
    }

    /// Docs/architecture.md §8: on daemon start, mark every **non-terminal**
    /// instance (`queued` / `loading` / `ready` / `draining` / `unloading`)
    /// as `crashed` — a process was expected to be running but the daemon
    /// restarted, so those records are orphaned. Each move is validated
    /// through the domain state machine (every non-terminal state legally
    /// reaches `crashed`) and audit-logged, and the whole recovery commits
    /// in one transaction. Settled records (`unloaded` / `failed` /
    /// `crashed`) are left untouched, and a second call finds nothing to do.
    ///
    /// Returns the instances as they were found (with their pre-recovery
    /// state), in recovery order. Non-terminal **operations** are exposed
    /// via `crate::repos::OperationsRepo::non_terminal` for the supervisor
    /// (job 6) rather than settled here.
    ///
    /// # Errors
    ///
    /// `InvalidStateTransition` if a stored state cannot legally reach
    /// `crashed` (defensive: schema fence and state machine agree on the
    /// vocabulary), or `Internal` on storage failure.
    pub async fn recover_non_terminal_instances(&self) -> Result<Vec<Instance>> {
        let stale = InstancesRepo::non_terminal(self.pool()).await?;
        if stale.is_empty() {
            return Ok(Vec::new());
        }
        let at = now();
        let mut tx = self.transaction().await?;
        for found in &stale {
            // Validate the move through the frozen domain state machine (never
            // a raw write). The expected `from` is the state just read, so the
            // guarded update below needs no re-SELECT on the transaction
            // connection (`&mut SqliteConnection` is not `Copy`, so the
            // two-query repository guard cannot run in a transaction).
            transition_instance(found.state(), InstanceState::Crashed).map_err(|_| {
                DomainError::with_message(
                    ErrorCode::InvalidStateTransition,
                    format!(
                        "instance {} in {:?} cannot legally reach crashed",
                        found.instance_id(),
                        found.state()
                    ),
                )
            })?;
            let result = sqlx::query(
                "UPDATE instances SET state = ?2, failure = NULL, updated_at = ?3 \
                 WHERE instance_id = ?1 AND state = ?4",
            )
            .bind(found.instance_id())
            .bind(wire_token(&InstanceState::Crashed))
            .bind(ts_string(at))
            .bind(wire_token(&found.state()))
            .execute(&mut *tx)
            .await
            .map_err(|e| storage_error(&e, "write instance state"))?;
            if result.rows_affected() == 0 {
                return Err(DomainError::with_message(
                    ErrorCode::InvalidStateTransition,
                    format!(
                        "instance {} changed state before recovery applied",
                        found.instance_id()
                    ),
                ));
            }
            AuditRepo::append(
                &mut *tx,
                &AuditEvent::new(
                    AuditKind::InstanceRecovered,
                    Some("instance".to_owned()),
                    Some(found.instance_id().to_string()),
                    None,
                    serde_json::json!({ "previous_state": found.state() }),
                    at,
                ),
            )
            .await?;
        }
        tx.commit()
            .await
            .map_err(|e| storage_error(&e, "commit recovery transaction"))?;
        Ok(stale)
    }
}
