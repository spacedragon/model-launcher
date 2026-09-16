//! Secure model-artifact discovery (`docs/development-plan.md` M1 job 4,
//! `docs/product-requirements.md` §3.1).
//!
//! The scanner walks a controlled model root, finds `*.gguf` / `*.ninfer`
//! artifacts and reconciles them into the `models` index
//! (`docs/architecture.md` §3 "Model") in a single SQLite transaction.
//!
//! # Threat model
//!
//! A model root is an *administrator-controlled* directory that may still
//! contain untrusted content: an unpacked archive, a user-writable share under
//! `/mnt/...`, or a hostile symlink. The scan therefore never trusts the
//! filesystem:
//!
//! - the root must be an existing, absolute, symlink-free directory, and it is
//!   canonicalized up front ([`validate_root`]);
//! - every path the walk emits is canonicalized and re-checked for
//!   *containment* inside the canonical root, so a symlink (or Windows
//!   directory junction / reparse point) can never pull a file from outside the
//!   root into the index;
//! - symlinks, Windows reparse points, non-regular files (sockets, FIFOs,
//!   devices) and directories are skipped, and the walk never descends through
//!   a symlinked directory;
//! - traversal is depth-first with lexically sorted entries and a visited-set
//!   of canonical directories, so the result — including the deterministic
//!   collision keys — does not depend on filesystem enumeration order.
//!
//! # Reconciliation
//!
//! [`ScanService::scan_root`] (and the wrappers [`scan_root`] /
//! [`scan_all_roots`]) performs the whole scan in **one** SQLite transaction:
//! model upserts, soft-deletes of vanished artifacts and the root's scan
//! bookkeeping either all commit or all roll back, so a failed scan can never
//! leave a half-updated index.
//!
//! Identity is stable across restarts: the row's `id` / `key` and the
//! `root_id` / `first_seen_at` bookkeeping are *preserved* by the update, so an
//! unchanged file keeps its identity, a changed file (new `mtime` or `size`) is
//! updated in place, a vanished file is marked `deleted` (never removed), and a
//! re-appearing file restores the same row.
//!
//! Admin-owned fields (`display_name`, `default_runtime_id`,
//! `default_load_config`, `metadata`, and an admin-overridden `key`) survive
//! every scan: reconciliation writes only the fields the scan actually owns
//! (see [`persistence`] for the statement-level contract).
//!
//! # Module layout
//!
//! - [`error`] — the domain-error constructors this job uses (no new codes);
//! - [`walk`] — root validation, the secure recursive walk and GGUF display
//!   names;
//! - [`key`] — deterministic key / UUID derivation and collision resolution;
//! - [`persistence`] — the SQLite statements, the incremental bookkeeping read
//!   and the admin-preserving upsert.

pub mod error;
pub mod key;
pub mod persistence;
pub mod walk;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use chrono::{DateTime, Utc};
use model_serving_domain::model::{ArtifactKind, Model};
use model_serving_persistence::SqliteStore;
use model_serving_persistence::repos::{ModelRoot, ModelRootsRepo};
use serde::{Deserialize, Serialize};

pub use crate::persistence::{Bookkeeping, ScanEntry, is_unchanged};
pub use crate::walk::{ScannedFile, ScannedRoot, normalize_mtime, scan_tree, validate_root};

/// Outcome of a successful scan of one model root. Serialized into the root's
/// `last_scan_result` column (`docs/api.md` §5 `model-roots`) and returned to
/// the caller, so an operator sees what a scan did without reading logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanSummary {
    /// The root that was scanned.
    pub root_id: String,
    /// Canonical path of the root.
    pub root_path: String,
    /// When the scan ran (RFC 3339 UTC).
    pub scanned_at: DateTime<Utc>,
    /// How long the scan took.
    pub duration_ms: u64,
    /// Artifacts found on disk in this scan.
    pub files_seen: u64,
    /// Rows inserted into the index for the first time.
    pub added: u64,
    /// Existing rows whose `size_bytes` / `mtime` changed.
    pub updated: u64,
    /// Rows that were already current (unchanged `size_bytes` + `mtime`).
    pub unchanged: u64,
    /// Rows restored from `deleted` (a re-appearing artifact).
    pub restored: u64,
    /// Rows newly marked `deleted` (the artifact is gone).
    pub removed: u64,
    /// Paths skipped because they are symlinks / reparse points or are not
    /// regular files.
    pub skipped: u64,
    /// Paths refused because they resolved outside the canonical root (a
    /// symlink escape, or the root being swapped mid-walk).
    pub rejected: u64,
}

/// Policy for the recursive walk.
///
/// Production code uses [`ScanOptions::default`]. The fault-injection field is
/// used only by tests, so the walk's fail-closed behaviour can be exercised
/// deterministically without relying on permission bits, which an elevated
/// Windows account ignores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOptions {
    /// Maximum directory depth below the root (`0` = the root itself only).
    pub max_depth: usize,
    /// Upper bound on visited directories, so a pathological tree can never
    /// make a scan run unboundedly.
    pub max_directories: usize,
    /// Fault-injection hook: make the walk report an inspection failure for the
    /// first path whose name contains this fragment, as if `stat` / `read_dir`
    /// had failed there. `None` everywhere in production. A `&'static str`
    /// keeps [`ScanOptions`] `Copy`.
    pub fail_on_path_containing: Option<&'static str>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_directories: 100_000,
            fail_on_path_containing: None,
        }
    }
}

/// Scanner facade over the SQLite [`SqliteStore`].
///
/// The service keeps no state of its own — the database holds all bookkeeping
/// — so constructing one per scan is fine.
#[derive(Debug, Clone, Copy)]
pub struct ScanService<'a> {
    store: &'a SqliteStore,
    options: ScanOptions,
}

impl<'a> ScanService<'a> {
    /// A service with the default [`ScanOptions`].
    #[must_use]
    pub fn new(store: &'a SqliteStore) -> Self {
        Self {
            store,
            options: ScanOptions::default(),
        }
    }

    /// A service with explicit [`ScanOptions`].
    #[must_use]
    pub fn with_options(store: &'a SqliteStore, options: ScanOptions) -> Self {
        Self { store, options }
    }

    /// Scan one registered root by `id` and reconcile the index.
    ///
    /// # Errors
    ///
    /// - [`ErrorCode::ModelNotFound`](model_serving_domain::error::ErrorCode::ModelNotFound)
    ///   when no root has that `id`;
    /// - [`ErrorCode::InvalidRequest`](model_serving_domain::error::ErrorCode::InvalidRequest)
    ///   for a disabled root or an unusable root path (missing, relative,
    ///   symlinked — see [`validate_root`]);
    /// - [`ErrorCode::Internal`](model_serving_domain::error::ErrorCode::Internal)
    ///   on a storage or I/O failure, in which case the whole reconciliation is
    ///   rolled back.
    pub async fn scan_root(&self, root_id: &str) -> error::Result<ScanSummary> {
        let root = ModelRootsRepo::get(self.store.pool(), root_id)
            .await?
            .ok_or_else(|| error::root_not_found(root_id))?;
        self.scan_root_record(&root).await
    }

    /// Scan every **enabled** registered root, in `id` order. One summary per
    /// root; a root that fails validation surfaces as an error (the scan is
    /// never silently partial).
    ///
    /// # Errors
    ///
    /// As [`Self::scan_root`], for the first root that fails.
    pub async fn scan_all_roots(&self) -> error::Result<Vec<ScanSummary>> {
        let roots = ModelRootsRepo::list(self.store.pool()).await?;
        let mut summaries = Vec::new();
        for root in roots.into_iter().filter(|root| root.enabled) {
            summaries.push(self.scan_root_record(&root).await?);
        }
        Ok(summaries)
    }

    /// Scan one already-loaded root record (used by [`Self::scan_root`] and by
    /// API layers that already hold the record).
    ///
    /// # Errors
    ///
    /// As [`Self::scan_root`].
    pub async fn scan_root_record(&self, root: &ModelRoot) -> error::Result<ScanSummary> {
        if !root.enabled {
            return Err(error::root_disabled(&root.id));
        }
        let canonical = validate_root(Path::new(&root.path))?;
        let scanned = scan_tree(&canonical, self.options)?;
        let (states, occupied) = persistence::load_states(self.store, &root.id).await?;
        let bookkeeping = bookkeeping_for(&scanned.files, &states, &occupied, scanned.discarded);
        let summary = persistence::ScanWrite { store: self.store }
            .record_scan(root, &canonical, &bookkeeping, scanned.skipped)
            .await?;
        tracing::info!(
            root_id = %summary.root_id,
            added = summary.added,
            updated = summary.updated,
            unchanged = summary.unchanged,
            restored = summary.restored,
            removed = summary.removed,
            skipped = summary.skipped,
            rejected = summary.rejected,
            "model root scanned"
        );
        Ok(summary)
    }
}

/// Convenience wrapper: scan one root by `id` with the default options.
///
/// # Errors
///
/// As [`ScanService::scan_root`].
pub async fn scan_root(store: &SqliteStore, root_id: &str) -> error::Result<ScanSummary> {
    ScanService::new(store).scan_root(root_id).await
}

/// Convenience wrapper: scan every enabled root with the default options.
///
/// # Errors
///
/// As [`ScanService::scan_all_roots`].
pub async fn scan_all_roots(store: &SqliteStore) -> error::Result<Vec<ScanSummary>> {
    ScanService::new(store).scan_all_roots().await
}

/// Build the reconciliation work list: one entry per artifact on disk, plus the
/// snapshot of *occupied* keys (existing rows plus the keys claimed during this
/// scan).
///
/// Identity assignment happens **before** any SQL runs:
///
/// - an artifact an existing row already tracks by path keeps that row's `id`
///   and `key` (the key may be an admin override, so it is never regenerated);
/// - a new artifact gets a deterministic [`model_id`](key::model_id);
/// - a key wanted by more than one new artifact — or by a new artifact when an
///   existing row already holds it — is resolved by appending a deterministic
///   suffix derived from the artifact's absolute path, so the result is
///   byte-identical no matter in which order the tree was traversed (see
///   [`key`]).
#[must_use]
pub fn bookkeeping_for<S: std::hash::BuildHasher>(
    files: &[ScannedFile],
    states: &HashMap<String, PersistenceState, S>,
    occupied: &BTreeSet<String>,
    rejected: u64,
) -> Bookkeeping {
    let candidates: Vec<String> = files.iter().map(|file| key::base_key(&file.path)).collect();
    // Path -> existing row, for the identity-preserving branch. A persisted
    // row's `path` is the canonical path the scan recorded earlier, so this
    // lookup is exact.
    let by_path: HashMap<&str, (&String, &PersistenceState)> = states
        .iter()
        .map(|(key, state)| (state.path.as_str(), (key, state)))
        .collect();

    // `reserved` is the authority on which keys may not be taken. It is
    // seeded from **every** occupied key — live rows, this root's soft-deleted
    // rows, and crucially the soft-deleted rows owned by *another* root, whose
    // `UNIQUE` key still exists in `models` even though `states` cannot see
    // them. Reserving only `states` keys would let a new artifact of this root
    // select such a key, lose the `INSERT … ON CONFLICT DO NOTHING`, and then
    // re-home the unrelated row through the update path (adopting its `id` and
    // admin fields).
    let mut reserved: BTreeSet<String> = occupied.clone();
    reserved.extend(states.keys().cloned());

    // Positions that still need a key. A file already indexed **by path**
    // always keeps the key its row owns — including an admin-overridden key —
    // so it never competes for allocation, whatever key that row holds. Letting
    // it compete would spend a suffix on a row that will not use it and push a
    // genuinely new artifact past a free base key. The row's own persisted key
    // is already blocked for everyone else, because `reserved` was seeded from
    // `occupied`; that is what stops a new artifact from taking an
    // admin-renamed key, and it does not require the indexed position to
    // participate in allocation.
    let mut indexable: Vec<usize> = Vec::with_capacity(files.len());
    let mut unresolvable: Vec<usize> = Vec::new();
    for (position, file) in files.iter().enumerate() {
        if by_path.contains_key(file.path.as_str()) {
            continue;
        }
        // A new artifact whose base key is already occupied (by a live row, by
        // this root's soft-deleted row, by another root's soft-deleted row, or
        // by an unowned row) must take a suffix instead of reusing it.
        if reserved.contains(&candidates[position]) {
            unresolvable.push(position);
        } else {
            indexable.push(position);
        }
    }
    let resolvable: Vec<String> = indexable
        .iter()
        .map(|position| candidates[*position].clone())
        .collect();
    let resolved = key::build_index(&resolvable, &reserved);
    // Keys claimed by *this* scan must also block later duplicates.
    for key in &resolved {
        reserved.insert(key.clone());
    }
    // Original position -> the key resolved for it. A position that is
    // `contested` is not in `indexable`, so it falls through to the collision
    // pass below.
    let key_for_position: HashMap<usize, &String> = indexable
        .iter()
        .zip(resolved.iter())
        .map(|(position, key)| (*position, key))
        .collect();

    // Second pass: positions whose base key is already occupied. Each artifact
    // here wants a key that another row holds, so it must receive a fresh
    // suffixed key rather than reuse the occupied one. Grouping by base key and
    // ranking on the canonical path keeps the result independent of traversal
    // order.
    let mut contested: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for position in unresolvable {
        contested
            .entry(candidates[position].as_str())
            .or_default()
            .push(position);
    }
    let mut extra_keys: HashMap<usize, String> = HashMap::new();
    for (base, mut positions) in contested {
        positions.sort_by(|a, b| files[*a].path.cmp(&files[*b].path));
        let suffixes = key::collision_candidates(base, positions.len(), &reserved);
        for (position, key) in positions.into_iter().zip(suffixes) {
            reserved.insert(key.clone());
            extra_keys.insert(position, key);
        }
    }

    // Every artifact's key is decided here, before any SQL runs. A position is
    // in exactly one of the three groups, so the loop below never needs a
    // fallback: an indexed artifact keeps its row's key, an indexable one gets
    // its base key from `key_for_position`, and a contested one gets a fresh
    // suffix from `extra_keys`.
    let mut keys_by_position: Vec<Option<String>> = vec![None; files.len()];
    for (position, key) in &key_for_position {
        keys_by_position[*position] = Some((*key).clone());
    }
    for (position, key) in extra_keys {
        keys_by_position[position] = Some(key);
    }

    let mut entries = Vec::with_capacity(files.len());
    for (position, file) in files.iter().enumerate() {
        let (id, key, existing) = if let Some((key, state)) = by_path.get(file.path.as_str()) {
            (
                state.id.clone(),
                (*key).clone(),
                Some(Existing {
                    size_bytes: state.size_bytes,
                    mtime: state.mtime,
                    deleted: state.deleted,
                }),
            )
        } else {
            // A file with no row at its path is always indexable or contested,
            // so its key is present; a `None` here would mean an occupied key
            // was about to be written a second time — the exact re-homing bug
            // this allocator exists to prevent.
            let key = keys_by_position[position]
                .take()
                .unwrap_or_else(|| unreachable!("unindexed artifact without an assigned key"));
            (key::model_id(&file.path), key, None)
        };
        entries.push(ScanEntry {
            id,
            key,
            path: file.path.clone(),
            artifact_kind: file.artifact_kind,
            size_bytes: file.size_bytes,
            mtime: file.mtime,
            display_name: file.display_name.clone(),
            existing,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.key.cmp(&b.key)));
    let present_keys = entries.iter().map(|entry| entry.key.clone()).collect();
    Bookkeeping {
        entries,
        reserved,
        present_keys,
        rejected,
    }
}

/// The persisted state of a row the scanner may already track. Loaded once per
/// scan into a `key -> state` map, so reconciliation needs a single read of the
/// `models` table (the incremental-scan input). Re-exported from
/// [`persistence`] where the load statement lives.
pub use crate::persistence::PersistenceState;

/// An existing row tracked by path (the identity-preservation input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Existing {
    /// Size recorded by the last scan.
    pub size_bytes: u64,
    /// Modification time recorded by the last scan.
    pub mtime: DateTime<Utc>,
    /// Whether the row was soft-deleted before this scan.
    pub deleted: bool,
}

/// A model row to write, with its persisted identity. Kept next to
/// [`ScanEntry`] because the reconciliation statement binds exactly these
/// fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelWrite {
    /// The row to upsert.
    pub model: Model,
    /// The root that owns the row now.
    pub root_id: Option<String>,
    /// The row's first-sighting timestamp (preserved when already set).
    pub first_seen_at: Option<DateTime<Utc>>,
}

/// The artifact kind implied by a file extension, or `None` when the file is
/// not a model artifact. See [`key::artifact_kind`].
#[must_use]
pub fn artifact_kind_of(path: &Path) -> Option<ArtifactKind> {
    key::artifact_kind(path)
}

/// A scan writes only the root's bookkeeping columns; this applies the
/// bookkeeping to a root record for callers that keep the record in memory
/// (the database write happens inside the scan transaction).
#[must_use]
pub fn root_with_bookkeeping(
    root: &ModelRoot,
    at: DateTime<Utc>,
    result: &ScanSummary,
) -> ModelRoot {
    let mut updated = root.clone();
    updated.last_scan_at = Some(at);
    updated.last_scan_result = serde_json::to_value(result).ok();
    updated
}

/// The `key -> base-key` grouping used by the collision tests: proves that a
/// traversal-order change cannot move a key.
#[must_use]
pub fn base_key_groups(files: &[ScannedFile]) -> BTreeMap<String, Vec<String>> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in files {
        groups
            .entry(key::base_key(&file.path))
            .or_default()
            .push(file.path.clone());
    }
    groups
}

#[cfg(test)]
mod tests;
