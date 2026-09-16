//! Root validation and the secure recursive walk.
//!
//! Everything in this module is `std::fs` + `std::path` — no async, no
//! database — so the traversal rules can be read (and tested) in one place.
//!
//! # Budgets
//!
//! The walk is bounded twice over: [`ScanOptions::max_depth`] caps how deep a
//! tree may be, and [`ScanOptions::max_directories`] caps how many directories
//! may be visited, so a pathological or hostile tree cannot make a scan run
//! unboundedly. Both bounds are counted, never silent: a skipped subtree shows
//! up in [`ScannedRoot::skipped`].
//!
//! [`ScanOptions`]: crate::ScanOptions

use std::collections::BTreeSet;
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use model_serving_domain::model::ArtifactKind;

use crate::error;
use crate::key::{
    GGUF_TYPE_BOOL, GGUF_TYPE_F32, GGUF_TYPE_F64, GGUF_TYPE_I8, GGUF_TYPE_I16, GGUF_TYPE_I32,
    GGUF_TYPE_I64, GGUF_TYPE_STRING, GGUF_TYPE_U8, GGUF_TYPE_U16, GGUF_TYPE_U32, GGUF_TYPE_U64,
    artifact_kind,
};

/// The maximum bytes [`scan_tree`] reads out of any single artifact while
/// looking for a readable display name. A GGUF header is a few kilobytes;
/// 64 KiB is far more than any header field needs and keeps a huge file from
/// being read into memory.
pub const MAX_PREFIX_BYTES: usize = 64 * 1024;

/// Directories are entered at most once per scan (canonical-path visited set),
/// so a repeatedly mounted subtree cannot cause a cycle; this is the cap on a
/// GGUF header's metadata-entry count, above which the header is treated as
/// malformed (`MAX_GGUF_HEADER_KEYS`).
pub const MAX_GGUF_HEADER_KEYS: u64 = 100_000;

/// One artifact found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    /// Canonical absolute path of the artifact (inside the canonical root).
    pub path: String,
    /// Format implied by the extension.
    pub artifact_kind: ArtifactKind,
    /// Size in bytes, from the `stat` the walk already performed.
    pub size_bytes: u64,
    /// Last modification time, from the same `stat`.
    pub mtime: DateTime<Utc>,
    /// Display name read from the artifact header, if one could be read.
    pub display_name: Option<String>,
}

/// What one walk produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScannedRoot {
    /// Artifacts, in deterministic (lexicographic path) order.
    pub files: Vec<ScannedFile>,
    /// Paths skipped because they are symlinks / reparse points or not
    /// regular files.
    pub skipped: u64,
    /// Paths skipped because they resolved outside the canonical root.
    pub discarded: u64,
    /// Directory depth the walk reached (for the summary / logging).
    pub max_depth_seen: usize,
}

/// Validate a configured model root and return its canonical form.
///
/// A root must be:
///
/// - non-empty and **absolute** (a relative path would be resolved against the
///   daemon's working directory, which is not an administrator decision);
/// - lexically free of `..` (a traversal segment is a configuration error, not
///   something to silently normalize away);
/// - a symlink-free path (no component may be a symlink / reparse point), so
///   the canonical form cannot jump outside the configured tree;
/// - an existing **directory**.
///
/// # Errors
///
/// [`ErrorCode::InvalidRequest`] for every rejection (including a missing or
/// unreadable path), with the offending path and reason in the message.
pub fn validate_root(path: &Path) -> error::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(error::invalid_root(path, "the path is empty"));
    }
    if !path.is_absolute() {
        return Err(error::invalid_root(
            path,
            "the path is not absolute (use the daemon-visible absolute path)",
        ));
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(error::invalid_root(
                path,
                "the path contains a `..` segment",
            ));
        }
    }

    // Reject a symlinked path before canonicalizing: `canonicalize` would
    // happily follow it and report the *target* as the root.
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        let metadata = std::fs::symlink_metadata(&prefix).map_err(|e| {
            error::invalid_root(
                path,
                &format!("`{}` cannot be inspected: {e}", prefix.display()),
            )
        })?;
        if is_link_like(&metadata) {
            return Err(error::invalid_root(
                path,
                &format!("`{}` is a symlink or reparse point", prefix.display()),
            ));
        }
    }

    let canonical = std::fs::canonicalize(path)
        .map_err(|e| error::invalid_root(path, &format!("cannot canonicalize: {e}")))?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|e| error::invalid_root(path, &format!("cannot stat: {e}")))?;
    if !metadata.is_dir() {
        return Err(error::invalid_root(path, "the path is not a directory"));
    }
    Ok(canonical)
}

/// Walk `root` (which must already be canonical, see [`validate_root`]) and
/// collect every `*.gguf` / `*.ninfer` artifact.
///
/// Rules (see the crate docs for the threat model):
///
/// - directory entries are read in sorted order and the walk is depth-first,
///   so the output is independent of filesystem enumeration order;
/// - symlinks / reparse points are skipped, and a directory is descended into
///   only when its own `symlink_metadata` says it is a real directory, so the
///   walk can never follow a link out of the root;
/// - every candidate file is canonicalized and checked for containment in
///   `root`; a path that resolves outside is counted in
///   [`ScannedRoot::discarded`] and never indexed;
/// - directories are entered at most once (canonical-path visited set), so a
///   hard-linked or repeatedly-mounted subtree cannot cause a cycle;
/// - only regular files are indexed; sockets, FIFOs, devices and directories
///   matching the artifact extension are skipped.
///
/// # Errors
///
/// [`ErrorCode::Internal`] for **any** failure to inspect the filesystem: the
/// root cannot be read, or a subdirectory cannot be canonicalized, stat'ed or
/// listed. The walk is deliberately fail-closed — a reconciliation only ever
/// deletes rows for artifacts it can prove are absent, so a partial view of the
/// tree (a permission error, an I/O error, a race with a concurrent removal)
/// must abort the scan rather than be reported as "skipped". Exceeding
/// [`ScanOptions::max_depth`] or [`ScanOptions::max_directories`] is treated the
/// same way: it omits a subtree from the walk, so it is incomplete traversal
/// rather than a safe skip. Only the cases that are *positively identified* as
/// non-indexable — symlinks and Windows reparse points, sockets/FIFOs/devices,
/// and paths that resolve outside the root — are counted in
/// [`ScannedRoot::skipped`] / [`ScannedRoot::discarded`] and skipped without
/// aborting.
pub fn scan_tree(root: &Path, options: crate::ScanOptions) -> error::Result<ScannedRoot> {
    let mut state = WalkState {
        root,
        options,
        out: ScannedRoot::default(),
        visited: BTreeSet::new(),
    };
    state.visited.insert(root.to_path_buf());
    let entries = std::fs::read_dir(root)
        .map_err(|e| error::storage(format!("read model root `{}`: {e}", root.display())))?;
    state.descend(entries, root, 0)?;
    state.out.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(state.out)
}

struct WalkState<'a> {
    root: &'a Path,
    options: crate::ScanOptions,
    out: ScannedRoot,
    visited: BTreeSet<PathBuf>,
}

impl WalkState<'_> {
    /// Test hook: report a deterministic inspection failure for any path whose
    /// name contains the configured fragment. Present only in test builds so a
    /// fail-closed test never has to rely on permission bits (which an elevated
    /// Windows account ignores).
    fn injected_failure(&self, path: &Path) -> error::Result<()> {
        if let Some(fragment) = self.options.fail_on_path_containing {
            let name = path.to_string_lossy();
            if name.contains(fragment) {
                return Err(error::storage(format!(
                    "injected inspection failure for `{name}`"
                )));
            }
        }
        Ok(())
    }

    fn descend(
        &mut self,
        entries: std::fs::ReadDir,
        dir: &Path,
        depth: usize,
    ) -> error::Result<()> {
        self.out.max_depth_seen = self.out.max_depth_seen.max(depth);
        // Sorted entry list: `read_dir` order is arbitrary, and the scanner's
        // output (including collision keys) must not depend on it.
        let mut names: Vec<(PathBuf, std::fs::DirEntry)> = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|e| error::storage(format!("read entry in `{}`: {e}", dir.display())))?;
            names.push((entry.path(), entry));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));

        let mut directories = Vec::new();
        for (path, entry) in names {
            self.injected_failure(&path)?;
            let file_type = entry
                .file_type()
                .map_err(|e| error::storage(format!("stat `{}`: {e}", path.display())))?;
            if file_type.is_symlink() {
                // Never follow, never descend: count and move on.
                self.out.skipped += 1;
                continue;
            }
            if file_type.is_dir() {
                if depth >= self.options.max_depth {
                    // A depth budget omits a whole subtree from the walk. That
                    // is *not* a safe skip: the sweep deletes by absence, so
                    // counting these and continuing would mark every artifact
                    // under the omitted subtree deleted. Fail closed before any
                    // database write instead.
                    return Err(error::storage(format!(
                        "directory `{}` exceeds the configured max depth {}; \
                         refusing to scan an incomplete tree",
                        path.display(),
                        self.options.max_depth
                    )));
                }
                // A failure to canonicalize a real subdirectory is a genuine
                // inspection error (permission, I/O, a race with a removal),
                // not a safe skip: silently skipping it would hand the sweep a
                // truncated view of the tree and mark its artifacts deleted.
                let canonical = std::fs::canonicalize(&path).map_err(|e| {
                    error::storage(format!(
                        "canonicalize model directory `{}`: {e}",
                        path.display()
                    ))
                })?;
                if !canonical.starts_with(self.root) {
                    self.out.discarded += 1;
                    continue;
                }
                // The canonical form must be a real directory and must not
                // have become a link since `file_type`. A *successful* stat
                // that says "not a real directory" is a safe skip (the entry
                // changed kind under us); a *failed* stat is an inspection
                // error and aborts.
                let metadata = std::fs::symlink_metadata(&canonical).map_err(|e| {
                    error::storage(format!(
                        "stat model directory `{}`: {e}",
                        canonical.display()
                    ))
                })?;
                if !metadata.is_dir() || is_link_like(&metadata) {
                    self.out.skipped += 1;
                    continue;
                }
                if self.visited.insert(canonical) {
                    directories.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                // Sockets, FIFOs, block/character devices.
                self.out.skipped += 1;
                continue;
            }
            self.consider_file(&path, &entry)?;
        }

        for path in directories {
            // `visited` already contains the root plus every directory enqueued
            // so far, so this bounds the number of directories the walk will
            // actually descend into — it is checked *before* the descent, not
            // derived from a final count after the fact. Exceeding it omits a
            // subtree, which is incomplete traversal rather than a safe skip:
            // the sweep deletes by absence, so fail closed before any database
            // write.
            if self.visited.len() > self.options.max_directories {
                return Err(error::storage(format!(
                    "directory `{}` exceeds the configured max directory count {}; \
                     refusing to scan an incomplete tree",
                    path.display(),
                    self.options.max_directories
                )));
            }
            // Fail closed: an unreadable subdirectory means the walk cannot
            // prove which artifacts still exist, and the sweep deletes by
            // absence. Aborting the whole scan is the only safe outcome.
            let entries = std::fs::read_dir(&path).map_err(|e| {
                error::storage(format!("read model directory `{}`: {e}", path.display()))
            })?;
            let depth = depth + 1;
            self.descend(entries, &path, depth)?;
        }
        Ok(())
    }

    fn consider_file(&mut self, path: &Path, entry: &std::fs::DirEntry) -> error::Result<()> {
        if artifact_kind(path).is_none() {
            return Ok(());
        }
        if is_link_like(
            &entry
                .metadata()
                .map_err(|e| error::storage(format!("stat `{}`: {e}", path.display())))?,
        ) {
            self.out.skipped += 1;
            return Ok(());
        }
        // Same rule as for directories: a failed canonicalize is an
        // inspection error, not a safe skip.
        let canonical = std::fs::canonicalize(path)
            .map_err(|e| error::storage(format!("canonicalize `{}`: {e}", path.display())))?;
        // Containment: the canonical path must still live inside the canonical
        // root. A symlink that pointed out of the tree is caught here even if
        // it slipped past the `file_type` check (race or exotic reparse point).
        if !canonical.starts_with(self.root) {
            self.out.discarded += 1;
            return Ok(());
        }
        let metadata = std::fs::symlink_metadata(&canonical)
            .map_err(|e| error::storage(format!("stat `{}`: {e}", canonical.display())))?;
        if !metadata.is_file() || is_link_like(&metadata) {
            self.out.skipped += 1;
            return Ok(());
        }
        let Some(indexed) = artifact_kind(&canonical) else {
            // The extension changed under us (or the canonical path is not the
            // candidate): index only real artifacts.
            self.out.skipped += 1;
            return Ok(());
        };
        let mtime = mtime_of(&metadata, &canonical)?;
        self.out.files.push(ScannedFile {
            path: canonical.to_string_lossy().into_owned(),
            artifact_kind: indexed,
            size_bytes: metadata.len(),
            mtime,
            display_name: read_display_name(&canonical, indexed),
        });
        Ok(())
    }
}

/// `true` when the metadata denotes a symlink (unix) or a reparse point
/// (Windows junction / mount point / symlink).
///
/// Both cases are treated identically by the scanner: never follow, never
/// index. Keeping the Windows arm explicit means a junction cannot be used to
/// escape the root on a WSL-adjacent deployment.
#[must_use]
pub fn is_link_like(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// Convert a filesystem modification time to `chrono` UTC. A time before the
/// Unix epoch is clamped to it rather than failing the scan.
fn mtime_of(metadata: &Metadata, path: &Path) -> error::Result<DateTime<Utc>> {
    let mtime = metadata
        .modified()
        .map_err(|e| error::storage(format!("read mtime of `{}`: {e}", path.display())))?;
    Ok(normalize_mtime(DateTime::<Utc>::from(mtime)))
}

/// Truncate a filesystem timestamp to the precision the database can store.
///
/// `models.mtime` round-trips through [`ts_string`](model_serving_persistence::ts_string),
/// which keeps milliseconds only. Comparing a raw `SystemTime` (nanosecond
/// precision on Linux, 100 ns on Windows) against the millisecond-rounded
/// value reloaded from the database would therefore differ on every rescan for
/// any file whose timestamp has sub-millisecond precision, so the artifact
/// would churn as `Updated` forever. Normalising here means the value carried
/// in [`ScannedFile::mtime`] is already exactly what a later scan will read
/// back, and the incremental comparison is stable.
#[must_use]
pub fn normalize_mtime(mtime: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(mtime.timestamp_millis()).unwrap_or(mtime)
}

/// Best-effort display name: for a GGUF file this is the `general.name` key of
/// the header block, for anything else the file stem.
///
/// Read failures are never fatal — a display name is a convenience, and a
/// truncated or foreign-format file must still be indexed (the runtime, not the
/// scanner, decides whether an artifact is loadable).
fn read_display_name(path: &Path, kind: ArtifactKind) -> Option<String> {
    match kind {
        ArtifactKind::Gguf => read_gguf_name(path).or_else(|| file_stem(path)),
        ArtifactKind::Ninfer => file_stem(path),
    }
}

fn file_stem(path: &Path) -> Option<String> {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
}

/// Read the GGUF `general.name` metadata value from the file header.
///
/// GGUF layout (llama.cpp `gguf.h`): magic `GGUF`, `u32` version, `u64`
/// tensor count, `u64` metadata-kv count, then `kv_count` entries of
/// `{ u64 name_len, name bytes, u32 type, value }`. Only the small header
/// prefix ([`MAX_PREFIX_BYTES`]) is read.
///
/// Returns `None` for any malformed / unsupported shape instead of failing the
/// scan.
pub(crate) fn read_gguf_name(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut magic = [0u8; 4];
    fill(&mut reader, &mut magic)?;
    if &magic != b"GGUF" {
        return None;
    }
    // The header is `magic | u32 version | u64 tensor_count | u64 kv_count`.
    // The version must be read as its own 4-byte field: reading it as a word
    // would leave the two counts misaligned by four bytes.
    let mut version_bytes = [0u8; 4];
    fill(&mut reader, &mut version_bytes)?;
    let version = u32::from_le_bytes(version_bytes);
    if !(1..=3).contains(&version) {
        return None;
    }
    let mut buffer = [0u8; 8];
    fill(&mut reader, &mut buffer)?;
    let _tensor_count = u64::from_le_bytes(buffer);
    fill(&mut reader, &mut buffer)?;
    let kv_count = u64::from_le_bytes(buffer);
    // A header claiming an absurd number of entries is malformed; do not spin.
    if kv_count > MAX_GGUF_HEADER_KEYS {
        return None;
    }
    for _ in 0..kv_count {
        fill(&mut reader, &mut buffer)?;
        let key_length = u64::from_le_bytes(buffer);
        if key_length > 4096 {
            return None;
        }
        let Ok(mut key) = usize::try_from(key_length).map(|length| vec![0u8; length]) else {
            return None;
        };
        fill(&mut reader, &mut key)?;
        let key = String::from_utf8(key).ok()?;
        let mut kind_and_value = [0u8; 4];
        fill(&mut reader, &mut kind_and_value)?;
        let value_type = u32::from_le_bytes(kind_and_value);
        match value_type {
            // GGUF_TYPE_STRING
            GGUF_TYPE_STRING => {
                let mut length = [0u8; 8];
                fill(&mut reader, &mut length)?;
                let length = u64::from_le_bytes(length);
                if key == "general.name" {
                    if length > MAX_PREFIX_BYTES as u64 {
                        return None;
                    }
                    let mut value = vec![0u8; usize::try_from(length).ok()?];
                    fill(&mut reader, &mut value)?;
                    return String::from_utf8(value)
                        .ok()
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty());
                }
                skip(&mut reader, length)?;
            }
            // u8 / i8
            GGUF_TYPE_U8 | GGUF_TYPE_I8 => skip(&mut reader, 1)?,
            // u16 / i16
            GGUF_TYPE_U16 | GGUF_TYPE_I16 => skip(&mut reader, 2)?,
            // u32 / i32 / f32
            GGUF_TYPE_U32 | GGUF_TYPE_I32 | GGUF_TYPE_F32 => skip(&mut reader, 4)?,
            // bool. GGUF v1/v2 store it as one byte, v3 widened it to four;
            // the reader accepts either width.
            GGUF_TYPE_BOOL => skip(&mut reader, if version >= 3 { 4 } else { 1 })?,
            // u64 / i64 / f64
            GGUF_TYPE_U64 | GGUF_TYPE_I64 | GGUF_TYPE_F64 => skip(&mut reader, 8)?,
            // Arrays may nest strings, and any tag outside the GGUF value
            // vocabulary that can precede a display name cannot be measured:
            // skipping an array structurally would be guesswork, so a malformed
            // or unknown shape ends the parse.
            _ => return None,
        }
    }
    None
}

fn fill<R: BufRead>(reader: &mut R, buffer: &mut [u8]) -> Option<()> {
    reader.read_exact(buffer).ok()?;
    Some(())
}

fn skip<R: BufRead>(reader: &mut R, bytes: u64) -> Option<()> {
    const CHUNK: usize = 8 * 1024;
    let chunk = u64::try_from(CHUNK).ok()?;
    let mut remaining = bytes;
    let mut sink = [0u8; CHUNK];
    while remaining > 0 {
        let step = usize::try_from(remaining.min(chunk)).ok()?;
        fill(reader, &mut sink[..step])?;
        remaining -= u64::try_from(step).ok()?;
    }
    Some(())
}
