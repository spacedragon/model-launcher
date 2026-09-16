//! The pooled SQLite endpoint, PRAGMA enforcement, migration runner,
//! explicit transaction API and the §8 restart-recovery entry point.

use std::path::Path;
use std::time::Duration;

use model_serving_domain::error::{DomainError, ErrorCode, Result};
use model_serving_domain::model::{Instance, InstanceState};
use model_serving_domain::state_machine::transition_instance;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteTransaction};

use crate::mapping::{now, parse_wire, storage_error, ts_string, wire_token};
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
            // Atomic create (`O_EXCL`): never truncate, so a file that
            // appears concurrently (another process creating or restoring
            // the database between the stat above and this open) can never
            // be destroyed. Losing the create race just means the file now
            // exists and falls through to the re-stat below.
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
            {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(DomainError::with_message(
                        ErrorCode::Internal,
                        format!("create database file {}: {e}", path.display()),
                    ));
                }
            }
            std::fs::metadata(path).map_err(|e| {
                DomainError::with_message(
                    ErrorCode::Internal,
                    format!("stat database file {}: {e}", path.display()),
                )
            })?
        }
        Err(e) => {
            return Err(DomainError::with_message(
                ErrorCode::Internal,
                format!("stat database file {}: {e}", path.display()),
            ));
        }
    };
    // Deterministically force owner-only (0600) regardless of umask.
    if meta.permissions().mode() & 0o777 != 0o600 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            DomainError::with_message(
                ErrorCode::Internal,
                format!(
                    "set database file {} permissions to 0600: {e}",
                    path.display()
                ),
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
            // Re-read the row inside the transaction: the pool-based scan
            // above predates it, and a legal `ready -> draining -> ready`
            // cycle could have moved the row and back. Validate the move
            // through the frozen domain state machine (never a raw write)
            // against this fresh state, then guard the update on BOTH the
            // state and the monotonic `revision` counter so a same-state ABA
            // cycle is rejected too (`&mut SqliteConnection` is not `Copy`,
            // so the two-query repository guard cannot run in a transaction;
            // sequential re-borrows of the transaction do the same job).
            let current =
                sqlx::query("SELECT state, revision FROM instances WHERE instance_id = ?1")
                    .bind(found.instance_id())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| storage_error(&e, "read instance state"))?;
            let Some(current) = current else {
                return Err(DomainError::with_message(
                    ErrorCode::Internal,
                    format!(
                        "instance {} vanished before recovery applied",
                        found.instance_id()
                    ),
                ));
            };
            let raw_state = current.try_get::<String, _>(0).map_err(|e| {
                DomainError::with_message(
                    ErrorCode::Internal,
                    format!("read instance {} state: {e}", found.instance_id()),
                )
            })?;
            let revised = current.try_get::<i64, _>(1).map_err(|e| {
                DomainError::with_message(
                    ErrorCode::Internal,
                    format!("read instance {} revision: {e}", found.instance_id()),
                )
            })?;
            let current_state = parse_wire::<InstanceState>(&raw_state, "instances.state")?;
            transition_instance(current_state, InstanceState::Crashed).map_err(|_| {
                DomainError::with_message(
                    ErrorCode::InvalidStateTransition,
                    format!(
                        "instance {} in {current_state:?} cannot legally reach crashed",
                        found.instance_id()
                    ),
                )
            })?;
            let result = sqlx::query(
                "UPDATE instances SET state = ?2, failure = NULL, updated_at = ?3, revision = revision + 1 \
                 WHERE instance_id = ?1 AND state = ?4 AND revision = ?5",
            )
            .bind(found.instance_id())
            .bind(wire_token(&InstanceState::Crashed))
            .bind(ts_string(at))
            .bind(wire_token(&current_state))
            .bind(revised)
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
                    // The fresh in-transaction state, not the older pool-scan
                    // value: if the row moved between scan and transaction,
                    // this is the state the recovery actually transitioned
                    // from.
                    serde_json::json!({ "previous_state": current_state }),
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
