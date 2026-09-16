//! The scanner's SQLite statements and the single-transaction reconciliation.
//!
//! [`SqliteStore::record_scan`](crate::persistence::ScanWrite::record_scan)
//! is the *only* place the scanner writes to the database, and it does so in
//! one transaction: model upserts, soft-deletes and the root's scan
//! bookkeeping either all commit or all roll back (`docs/architecture.md` §9).
//!
//! # Why the scanner does not use `ModelsRepo::upsert`
//!
//! `ModelsRepo::upsert` (M1 job 3) is the *CRUD* write path: it is driven by a
//! fully-populated domain `Model` and refreshes every stored column, including
//! the admin-owned `display_name` / `default_runtime_id` /
//! `default_load_config` / `metadata`. That is correct for
//! `PATCH /admin/v1/models/{id}` and wrong for a scan: a scan that re-derived
//! those fields would silently discard every admin edit
//! (`docs/product-requirements.md` §3.1: the daemon only records path, size,
//! mtime, format; display name and load config are user input).
//!
//! The scanner therefore issues its own statements:
//!
//! - [`INSERT_MODEL`] for a **first sighting** (nothing stored yet for that
//!   `key`), which seeds the admin-owned columns from the scan (a display name
//!   read from a GGUF header, or the file stem) and sets the bookkeeping;
//! - [`UPDATE_MODEL`] for an **existing row**, which writes only the
//!   scan-owned columns (`path`, `artifact_kind`, `size_bytes`, `mtime`,
//!   `deleted`, `last_seen_at`) plus the bookkeeping columns that must never be
//!   lost (`root_id` — the newest root that saw it — and `first_seen_at`, only
//!   when it is still `NULL`).
//!
//! Identity is stable by construction: the row's `id` and `key` are never
//! rewritten, and both statements are keyed on `key`, so an unchanged file,
//! an updated file, a delete/re-appear cycle and a daemon restart all converge
//! on the same row.

use std::path::Path;

use chrono::{DateTime, Utc};
use model_serving_domain::error::Result;
use model_serving_domain::model::ArtifactKind;
use model_serving_persistence::repos::{AuditEvent, AuditKind, AuditRepo, ModelRoot};
use model_serving_persistence::{SqliteStore, storage_error, ts_string, wire_token};
use sqlx::Row;

use crate::key::Reserved;
use crate::{Existing, ScanSummary};
use std::collections::BTreeSet;

/// The one writer of scan state: reconcile the index and record the root
/// bookkeeping in a single transaction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScanWrite<'a> {
    pub(crate) store: &'a SqliteStore,
}

/// Timestamps are written with the persistence crate's canonical RFC 3339
/// rendering, so a stored value always round-trips through the same parser.
fn now() -> DateTime<Utc> {
    Utc::now()
}

/// The persisted state of a row the scanner may already track: loaded once per
/// scan into a `key -> state` map so reconciliation needs one read of the
/// `models` table (the incremental-scan input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistenceState {
    /// Stable row identity (`models.id`).
    pub id: String,
    /// Path last recorded for this row.
    pub path: String,
    /// Size last recorded.
    pub size_bytes: u64,
    /// Modification time last recorded.
    pub mtime: DateTime<Utc>,
    /// Whether the row is currently soft-deleted.
    pub deleted: bool,
}

/// One artifact to reconcile, with the identity decision already made.
#[derive(Debug, Clone)]
pub struct ScanEntry {
    /// Stable row identity (from the index, or derived from the path).
    pub id: String,
    /// The key this row must use.
    pub key: String,
    /// Canonical absolute path.
    pub path: String,
    /// Format implied by the extension.
    pub artifact_kind: ArtifactKind,
    /// Size in bytes at scan time.
    pub size_bytes: u64,
    /// Modification time at scan time.
    pub mtime: DateTime<Utc>,
    /// Display name read from the artifact, when one was read.
    pub display_name: Option<String>,
    /// The matching persisted row, when the index already had one for this
    /// path (drives the identity-preserving update).
    pub existing: Option<Existing>,
}

/// Everything a single scan needs to write, computed before the transaction
/// opens.
#[derive(Debug, Clone)]
pub struct Bookkeeping {
    /// One entry per artifact on disk.
    pub entries: Vec<ScanEntry>,
    /// Every key that must remain occupied after this scan: keys of rows the
    /// scan did not revisit (so their keys are never recycled) plus the keys
    /// claimed by this scan's artifacts. It is the collision-avoidance input,
    /// **not** the sweep input.
    pub reserved: Reserved,
    /// The keys of the rows this scan actually re-indexed. The sweep marks a
    /// model deleted when its key is absent from this set — using `reserved`
    /// instead would keep every missing row alive, because `reserved` also
    /// holds the keys of the rows that are no longer on disk.
    pub present_keys: BTreeSet<String>,
    /// Paths the walk refused (outside the root, symlinks, non-regular files):
    /// surfaced in the summary, never indexed.
    pub rejected: u64,
}

/// Incremental-scan classification of one artifact against the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Added,
    Updated,
    Unchanged,
    Restored,
}

/// Statements the scanner owns (see the module docs for why these are not
/// `ModelsRepo::upsert`).
const INSERT_MODEL: &str = "
    INSERT INTO models (
        id, key, path, display_name, artifact_kind, size_bytes, mtime,
        default_runtime_id, default_load_config, metadata, deleted,
        root_id, first_seen_at, last_seen_at
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL, 0, ?8, ?9, ?9)
    ON CONFLICT (key) DO NOTHING
";

const UPDATE_MODEL: &str = "
    UPDATE models SET
        path        = ?2,
        artifact_kind = ?3,
        size_bytes  = ?4,
        mtime       = ?5,
        deleted     = 0,
        root_id     = ?6,
        first_seen_at = COALESCE(first_seen_at, ?7),
        last_seen_at  = ?7
    WHERE key = ?1
      AND (path IS NOT ?2 OR size_bytes IS NOT ?4 OR mtime IS NOT ?5 OR deleted IS NOT 0)
";

/// A row whose content already matches the scan still needs its bookkeeping
/// refreshed: a scan is the only writer of `root_id` / `last_seen_at`, and a
/// file that was soft-deleted must not stay deleted after it re-appeared.
const UPDATE_BOOKKEEPING: &str = "
    UPDATE models SET
        deleted     = 0,
        root_id     = ?6,
        first_seen_at = COALESCE(first_seen_at, ?7),
        last_seen_at  = ?7
    WHERE key = ?1
      AND (deleted IS NOT 0 OR root_id IS NOT ?6)
";

/// Delete every non-deleted row that belongs to `root_id` and whose key is not
/// in this scan's [`Bookkeeping::present_keys`] set.
///
/// A row that moved to a different root is re-homed by [`UPDATE_MODEL`], so it
/// is `root_id = <other root>` by the time the sweeping statement runs and is
/// not deleted by the root it left.
const DELETE_MISSING: &str = "
    UPDATE models SET deleted = 1, last_seen_at = ?3
    WHERE root_id = ?1 AND deleted = 0 AND key NOT IN (SELECT value FROM json_each(?2))
";

/// How many rows changed in each class, for the audit payload and the summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    added: u64,
    updated: u64,
    unchanged: u64,
    restored: u64,
    removed: u64,
}

/// Load the persisted identities the scanner needs for an incremental scan.
///
/// Returns `(states, occupied)`:
///
/// - `states` maps `key -> state` for every row this scan may legitimately
///   track: the live rows (whether or not they have an owner, so an artifact
///   inserted by an administrator is adopted rather than duplicated) plus the
///   soft-deleted rows **owned by `root_id`**. A deleted row owned by another
///   root — or by nobody (`root_id IS NULL`) — is excluded: a scan of this root
///   must not resurrect it;
/// - `occupied` is the key set of **every** `models` row, including the
///   soft-deleted and unowned rows. Their `models.key` is still `UNIQUE`, so a
///   new artifact of this root must never select one: otherwise the
///   `INSERT … ON CONFLICT DO NOTHING` would silently lose and the update would
///   re-home the unrelated row (adopting its `id` and admin fields).
///
/// # Errors
///
/// [`ErrorCode::Internal`] on a storage or decode failure.
pub async fn load_states(
    store: &SqliteStore,
    root_id: &str,
) -> Result<(
    std::collections::HashMap<String, PersistenceState>,
    BTreeSet<String>,
)> {
    let rows = sqlx::query(
        "SELECT key, id, path, size_bytes, mtime, deleted, root_id FROM models ORDER BY key",
    )
    .fetch_all(store.pool())
    .await
    .map_err(|e| storage_error(&e, "load model index identities"))?;

    let mut states = std::collections::HashMap::with_capacity(rows.len());
    let mut occupied = BTreeSet::new();
    for row in rows {
        let key: String = row
            .try_get("key")
            .map_err(|e| decode_error("models.key", &e))?;
        // `models.root_id` is nullable by schema (`REFERENCES model_roots (id)
        // ON DELETE SET NULL`), and `ModelsRepo::upsert` never writes it, so a
        // row created outside a scan — an administrator-added artifact, or one
        // whose root was deleted — legitimately carries `NULL`. Decoding that
        // as `String` would fail the whole scan with `Internal`.
        let row_root: Option<String> = row
            .try_get("root_id")
            .map_err(|e| decode_error("models.root_id", &e))?;
        let deleted: bool = row
            .try_get("deleted")
            .map_err(|e| decode_error("models.deleted", &e))?;
        // Every row's key blocks reuse, whatever its state or owner.
        occupied.insert(key.clone());
        // A row is eligible for incremental tracking only when this scan may
        // legitimately write to it. An *unowned* (`root_id IS NULL`) row is
        // eligible while it is live, because a scan that finds a file at its
        // recorded path should adopt that row rather than insert a duplicate —
        // but a *soft-deleted* unowned row is not: nothing recorded which root
        // deleted it, so no scan may resurrect it. A row owned by another root
        // is likewise ineligible once deleted.
        let owned_by_this_root = row_root.as_deref() == Some(root_id);
        if deleted && !owned_by_this_root {
            continue;
        }
        let id: String = row
            .try_get("id")
            .map_err(|e| decode_error("models.id", &e))?;
        let path: String = row
            .try_get("path")
            .map_err(|e| decode_error("models.path", &e))?;
        let size_bytes: i64 = row
            .try_get("size_bytes")
            .map_err(|e| decode_error("models.size_bytes", &e))?;
        let mtime: String = row
            .try_get("mtime")
            .map_err(|e| decode_error("models.mtime", &e))?;
        states.insert(
            key,
            PersistenceState {
                id,
                path,
                size_bytes: u64::try_from(size_bytes).map_err(|_| {
                    crate::error::reconcile("models.size_bytes", "negative size stored")
                })?,
                // Normalise as well as the walk does: a row written before
                // this invariant existed (or by a future writer) could still
                // hold sub-millisecond precision, and comparing it against a
                // normalised scan value would churn the artifact as
                // `Updated` forever.
                mtime: crate::walk::normalize_mtime(parse_mtime(&mtime)?),
                deleted,
            },
        );
    }
    Ok((states, occupied))
}

fn decode_error(column: &str, err: &sqlx::Error) -> model_serving_domain::error::DomainError {
    crate::error::reconcile(column, &err.to_string())
}
fn parse_mtime(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|e| {
            crate::error::reconcile(
                "models.mtime",
                &format!("stored {raw:?} is not RFC 3339: {e}"),
            )
        })
}

/// The scan transaction: reconcile the index and record the root bookkeeping.
impl ScanWrite<'_> {
    /// Reconcile `bookkeeping` for `root` and write the root's scan result.
    pub(crate) async fn record_scan(
        &self,
        root: &ModelRoot,
        canonical: &Path,
        bookkeeping: &Bookkeeping,
        skipped: u64,
    ) -> Result<ScanSummary> {
        let started = std::time::Instant::now();
        let at = now();
        let mut tx = self.store.transaction().await?;

        // Re-verify every row this scan matched by path *before* mutating
        // anything. `bookkeeping` was built from a snapshot taken outside this
        // transaction, so a concurrent administrator edit (re-key, re-path,
        // re-own, re-delete) can invalidate it. Without this check an UPDATE
        // that matches no row returns 0 and would be reported as `unchanged`,
        // after which the sweep would delete the very row the administrator
        // just renamed. Any mismatch aborts and rolls back.
        verify_matched_rows(&mut tx, bookkeeping).await?;

        let mut counts = Counts::default();
        for entry in &bookkeeping.entries {
            match self.apply_entry(&mut tx, entry, &root.id, at).await? {
                Change::Added => counts.added += 1,
                Change::Updated => counts.updated += 1,
                Change::Unchanged => counts.unchanged += 1,
                Change::Restored => counts.restored += 1,
            }
        }
        counts.removed = sweep_missing(&mut tx, root, bookkeeping, at).await?;

        // The duration is measured *before* the summary is built so the value
        // persisted into `last_scan_result` and the value returned to the
        // caller are the same number. Measuring after the commit would make the
        // stored summary disagree with the returned one.
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let summary = ScanSummary {
            root_id: root.id.clone(),
            root_path: canonical.to_string_lossy().into_owned(),
            scanned_at: at,
            duration_ms,
            files_seen: bookkeeping.entries.len() as u64,
            added: counts.added,
            updated: counts.updated,
            unchanged: counts.unchanged,
            restored: counts.restored,
            removed: counts.removed,
            skipped,
            rejected: bookkeeping.rejected,
        };
        write_root_bookkeeping(&mut tx, root, at, &summary).await?;
        append_scan_audit(&mut tx, &summary, at).await?;

        tx.commit()
            .await
            .map_err(|e| storage_error(&e, "commit scan transaction"))?;

        Ok(summary)
    }

    /// Write one artifact row and classify the change.
    async fn apply_entry(
        &self,
        tx: &mut sqlx::SqliteTransaction<'_>,
        entry: &ScanEntry,
        root_id: &str,
        at: DateTime<Utc>,
    ) -> Result<Change> {
        let seen_at = ts_string(at);
        let mtime = ts_string(entry.mtime);
        let Some(existing) = &entry.existing else {
            // A first sighting. `bookkeeping_for` reserved every key already in
            // `models` (including another root's soft-deleted rows, whose
            // `UNIQUE` key still exists), so a lost `ON CONFLICT` here means
            // the reservation and the ledger disagreed — the scan would
            // otherwise fall through and update a row it never matched by path,
            // adopting that row's `id` and admin fields. Fail closed instead.
            let inserted = sqlx::query(INSERT_MODEL)
                .bind(&entry.id)
                .bind(&entry.key)
                .bind(&entry.path)
                .bind(&entry.display_name)
                .bind(wire_token(&entry.artifact_kind))
                .bind(i64::try_from(entry.size_bytes).unwrap_or(i64::MAX))
                .bind(&mtime)
                .bind(root_id)
                .bind(&seen_at)
                .execute(&mut **tx)
                .await
                .map_err(|e| storage_error(&e, "insert model"))?
                .rows_affected();
            if inserted > 0 {
                append_model_audit(tx, entry, AuditKind::ModelUpserted, "added", at).await?;
                return Ok(Change::Added);
            }
            return Err(crate::error::reconcile(
                "insert model",
                &format!(
                    "key `{}` for `{}` is already taken by a row this scan did not \
                     match by path (root {root_id})",
                    entry.key, entry.path
                ),
            ));
        };

        if is_unchanged(existing, entry.size_bytes, entry.mtime) {
            // Still touch the bookkeeping: a scan is the only writer of
            // `root_id` / `last_seen_at`.
            self.update_entry_rows(tx, entry, root_id, at, true).await?;
            return Ok(Change::Unchanged);
        }
        let restored = existing.deleted;
        let rows = self
            .update_entry_rows(tx, entry, root_id, at, false)
            .await?;
        if rows == 0 {
            return Ok(Change::Unchanged);
        }
        if restored {
            append_model_audit(tx, entry, AuditKind::ModelRestored, "restored", at).await?;
            return Ok(Change::Restored);
        }
        Ok(Change::Updated)
    }

    /// The identity-preserving update: writes the scan-owned columns (plus the
    /// never-clobbered bookkeeping) and leaves `id`, `key`, `display_name`,
    /// `default_runtime_id`, `default_load_config` and `metadata` untouched.
    async fn update_entry_rows(
        &self,
        tx: &mut sqlx::SqliteTransaction<'_>,
        entry: &ScanEntry,
        root_id: &str,
        at: DateTime<Utc>,
        unchanged: bool,
    ) -> Result<u64> {
        let sql = if unchanged {
            UPDATE_BOOKKEEPING
        } else {
            UPDATE_MODEL
        };
        let result = sqlx::query(sql)
            .bind(&entry.key)
            .bind(&entry.path)
            .bind(wire_token(&entry.artifact_kind))
            .bind(i64::try_from(entry.size_bytes).unwrap_or(i64::MAX))
            .bind(ts_string(entry.mtime))
            .bind(root_id)
            .bind(ts_string(at))
            .execute(&mut **tx)
            .await
            .map_err(|e| storage_error(&e, "update model"))?;
        if !unchanged && result.rows_affected() > 0 {
            append_model_audit(tx, entry, AuditKind::ModelUpserted, "updated", at).await?;
        }
        Ok(result.rows_affected())
    }
}

async fn append_model_audit(
    tx: &mut sqlx::SqliteTransaction<'_>,
    entry: &ScanEntry,
    kind: AuditKind,
    change: &str,
    at: DateTime<Utc>,
) -> Result<()> {
    AuditRepo::append(
        &mut **tx,
        &AuditEvent::new(
            kind,
            Some("model".to_owned()),
            Some(entry.id.clone()),
            None,
            serde_json::json!({
                "key": entry.key,
                "path": entry.path,
                "artifact_kind": entry.artifact_kind,
                "size_bytes": entry.size_bytes,
                "change": change,
            }),
            at,
        ),
    )
    .await
}

/// Soft-delete every model that this root previously contributed and that the
/// scan did not re-encounter (`docs/development-plan.md` M1 job 4: 已删除标记).
///
/// Rows are never physically removed, so a re-appearing file restores the same
/// `id` / `key` row (and keeps any admin edits attached to it).
async fn sweep_missing(
    tx: &mut sqlx::SqliteTransaction<'_>,
    root: &ModelRoot,
    bookkeeping: &Bookkeeping,
    at: DateTime<Utc>,
) -> Result<u64> {
    let mut keys: Vec<&str> = bookkeeping
        .present_keys
        .iter()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let json = serde_json::to_string(&keys)
        .map_err(|e| crate::error::reconcile("scan reserved keys", &e.to_string()))?;

    let result = sqlx::query(DELETE_MISSING)
        .bind(&root.id)
        .bind(&json)
        .bind(ts_string(at))
        .execute(&mut **tx)
        .await
        .map_err(|e| storage_error(&e, "soft-delete missing models"))?;
    let removed = result.rows_affected();
    if removed > 0 {
        AuditRepo::append(
            &mut **tx,
            &AuditEvent::new(
                AuditKind::ModelDeleted,
                Some("model_root".to_owned()),
                Some(root.id.clone()),
                None,
                serde_json::json!({ "root_id": root.id, "count": removed }),
                at,
            ),
        )
        .await?;
    }
    Ok(removed)
}

/// Confirm that every artifact this scan matched by path still has the identity
/// the snapshot recorded.
///
/// This runs at the very start of the scan transaction and is what
/// disambiguates a legitimate no-op from a lost update. An `UPDATE` affecting 0
/// rows is normal for the `UPDATE_BOOKKEEPING` statement (the row already
/// carries this root and is not deleted), so the row's own `UPDATE` cannot be
/// used to detect that the snapshot went stale. Comparing `key`/`id`/`path` up
/// front can.
///
/// Only rows matched *by path* are verified: those are exactly the rows the
/// update statements address by `key`, and they are the rows whose loss would
/// otherwise be misread as "unchanged" and then swept.
async fn verify_matched_rows(
    tx: &mut sqlx::SqliteTransaction<'_>,
    bookkeeping: &Bookkeeping,
) -> Result<()> {
    for entry in &bookkeeping.entries {
        if entry.existing.is_none() {
            continue;
        }
        let row = sqlx::query("SELECT id, path FROM models WHERE key = ?1")
            .bind(&entry.key)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| storage_error(&e, "verify matched model"))?;
        let Some(row) = row else {
            return Err(crate::error::reconcile(
                "verify matched model",
                &format!(
                    "key `{}` disappeared while the scan was in flight; \
                     refusing to sweep the row it identified",
                    entry.key
                ),
            ));
        };
        let id: String = row
            .try_get("id")
            .map_err(|e| decode_error("models.id", &e))?;
        let path: String = row
            .try_get("path")
            .map_err(|e| decode_error("models.path", &e))?;
        if id != entry.id || path != entry.path {
            return Err(crate::error::reconcile(
                "verify matched model",
                &format!(
                    "key `{}` no longer identifies model {} at `{}` \
                     (now id `{id}` at `{path}`); refusing to overwrite a \
                     concurrent change",
                    entry.key, entry.id, entry.path
                ),
            ));
        }
    }
    Ok(())
}

/// Write the root's `last_scan_at` / `last_scan_result` columns **inside the
/// scan transaction** (`docs/api.md` §5 `model-roots`).
///
/// This doubles as the compare-and-swap that makes a scan's snapshot valid: the
/// root record was read *before* the walk and the model states were loaded, so
/// between that read and this statement another actor may have re-pointed the
/// root's `path`, disabled it, or completed a newer scan of the same root. Any
/// of those changes invalidates the work done by this transaction, so the update
/// only matches while the row still looks exactly as it did when loaded, and a
/// non-match rolls the whole transaction back.
///
/// The `last_scan_at` comparison is NULL-safe (`IS` rather than `=`): a root that
/// has never been scanned has `NULL` there, and `NULL = NULL` is `NULL`, which
/// would fail to match its own snapshot.
async fn write_root_bookkeeping(
    tx: &mut sqlx::SqliteTransaction<'_>,
    root: &ModelRoot,
    at: DateTime<Utc>,
    summary: &ScanSummary,
) -> Result<()> {
    let payload = serde_json::to_string(summary)
        .map_err(|e| crate::error::reconcile("scan summary", &e.to_string()))?;
    let expected_last_scan_at = root.last_scan_at.map(ts_string);
    let result = sqlx::query(
        "UPDATE model_roots \
         SET last_scan_at = ?2, last_scan_result = ?3 \
         WHERE id = ?1 \
           AND path = ?4 \
           AND enabled = ?5 \
           AND last_scan_at IS ?6",
    )
    .bind(&root.id)
    .bind(ts_string(at))
    .bind(payload)
    .bind(&root.path)
    .bind(root.enabled)
    .bind(expected_last_scan_at)
    .execute(&mut **tx)
    .await
    .map_err(|e| storage_error(&e, "write model root scan result"))?;
    if result.rows_affected() == 0 {
        // Either the root was deleted between validation and the transaction,
        // or it changed while this scan was in flight. Both invalidate this
        // scan's snapshot, so the whole transaction rolls back rather than
        // overwriting a newer scan or writing an orphaned index update.
        return Err(crate::error::reconcile(
            "write model root scan result",
            &format!(
                "model root {} changed while the scan was in flight \
                 (deleted, re-pointed, disabled, or already scanned again); \
                 refusing to overwrite newer state",
                root.id
            ),
        ));
    }
    Ok(())
}

/// Append the scan-level audit event (`docs/architecture.md` §9) with the
/// transaction's executor.
async fn append_scan_audit(
    tx: &mut sqlx::SqliteTransaction<'_>,
    summary: &ScanSummary,
    at: DateTime<Utc>,
) -> Result<()> {
    let payload = serde_json::to_value(summary)
        .map_err(|e| crate::error::reconcile("scan summary", &e.to_string()))?;
    AuditRepo::append(
        &mut **tx,
        &AuditEvent::new(
            AuditKind::ModelRootUpserted,
            Some("model_root".to_owned()),
            Some(summary.root_id.clone()),
            None,
            payload,
            at,
        ),
    )
    .await
}

/// The scan summary classifier, exposed for the incremental-scan tests.
#[must_use]
pub fn is_unchanged(existing: &Existing, size_bytes: u64, mtime: DateTime<Utc>) -> bool {
    existing.size_bytes == size_bytes && existing.mtime == mtime && !existing.deleted
}
