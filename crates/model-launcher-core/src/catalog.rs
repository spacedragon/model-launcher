use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use gguf_rs_lib::{
    format::MetadataValue,
    reader::{GGUFFileReader, GGUFReaderConfig},
};
use notify::{Config as NotifyConfig, PollWatcher, RecursiveMode, Watcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use walkdir::WalkDir;

use crate::{
    AppError, ConfigStore, LaunchProfile, LauncherConfig, ModelId, ModelKey, ModelRecord,
    ModelState,
};

/// Stable when the host filesystem exposes an identity, otherwise deliberately unavailable.
/// No model contents are hashed: even multi-gigabyte files have O(1) identity cost.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogIdentity {
    Unix {
        device: u64,
        #[serde(alias = "inode")]
        inode_fingerprint: u64,
    },
    Windows {
        volume: u64,
        #[serde(alias = "file_index")]
        file_fingerprint: u64,
    },
    #[default]
    Unavailable,
}

impl CatalogIdentity {
    #[must_use]
    pub fn for_path(path: &Path) -> Self {
        let Ok(file) = File::open(path) else {
            return Self::Unavailable;
        };
        Self::for_file(&file)
    }

    fn for_file(file: &File) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let Ok(metadata) = file.metadata() else {
                return Self::Unavailable;
            };
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            let change_nanos = {
                use std::os::darwin::fs::MetadataExt as _;
                i128::from(metadata.st_birthtime()) * 1_000_000_000
                    + i128::from(metadata.st_birthtime_nsec())
            };
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
            let change_nanos = 0_i128;
            Self::Unix {
                device: metadata.dev(),
                inode_fingerprint: stable_fingerprint(&[
                    metadata.ino(),
                    metadata.len(),
                    metadata.mtime() as u64,
                    metadata.mtime_nsec() as u64,
                    change_nanos as u64,
                    (change_nanos >> 64) as u64,
                ]),
            }
        }
        #[cfg(windows)]
        {
            windows_identity(file).unwrap_or(Self::Unavailable)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = file;
            Self::Unavailable
        }
    }
}

#[cfg(windows)]
fn windows_identity(file: &File) -> Option<CatalogIdentity> {
    use std::{mem::MaybeUninit, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the handle remains open for the call and Windows initializes the output on success.
    let success =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return None;
    }
    // SAFETY: a successful call initialized the structure.
    let information = unsafe { information.assume_init() };
    let metadata = file.metadata().ok()?;
    Some(CatalogIdentity::Windows {
        volume: u64::from(information.dwVolumeSerialNumber),
        file_fingerprint: stable_fingerprint(&[
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
            metadata.len(),
            system_time_nanos(metadata.modified().ok()?) as u64,
            metadata
                .created()
                .map(system_time_nanos)
                .unwrap_or_default() as u64,
        ]),
    })
}

#[cfg(windows)]
fn windows_directory_identity(path: &Path) -> Option<(u32, u64)> {
    use std::{
        mem::MaybeUninit,
        os::windows::{
            ffi::OsStrExt as _,
            io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
        },
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
        },
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: the UTF-16 path is NUL terminated and all remaining arguments are documented values.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: CreateFileW returned a unique valid owned handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the handle is valid and the output is initialized on success.
    let success =
        unsafe { GetFileInformationByHandle(handle.as_raw_handle(), information.as_mut_ptr()) };
    if success == 0 {
        return None;
    }
    // SAFETY: a successful call initialized the structure.
    let information = unsafe { information.assume_init() };
    Some((
        information.dwVolumeSerialNumber,
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    ))
}

fn stable_fingerprint(values: &[u64]) -> u64 {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

#[cfg(windows)]
fn system_time_nanos(value: std::time::SystemTime) -> i128 {
    value
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX))
        .unwrap_or_default()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogMetadata {
    pub architecture: Option<String>,
    pub parameter_count: Option<u64>,
    pub quantization: Option<String>,
    pub quantization_version: Option<u64>,
    pub context_length: Option<u64>,
    pub block_count: Option<u64>,
    pub embedding_length: Option<u64>,
    pub attention_head_count: Option<u64>,
    pub attention_head_count_kv: Option<u64>,
    pub attention_key_length: Option<u64>,
    pub attention_value_length: Option<u64>,
    pub full_attention_interval: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedModel {
    pub display_name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub identity: CatalogIdentity,
    pub metadata: CatalogMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogDiagnosticKind {
    Scan,
    Metadata,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogDiagnostic {
    pub kind: CatalogDiagnosticKind,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanResult {
    pub models: Vec<ScannedModel>,
    pub diagnostics: Vec<CatalogDiagnostic>,
    /// True only when the entire configured root was traversed reliably.
    pub complete: bool,
}

/// Maximum files considered in one scan. Hitting the cap makes the scan incomplete.
pub const MAX_DISCOVERED_GGUF_FILES: usize = 1_024;
/// Maximum logical records emitted by one scan.
pub const MAX_DISCOVERED_MODELS: usize = 1_024;
/// Maximum user-visible diagnostics retained by one scan.
pub const MAX_CATALOG_DIAGNOSTICS: usize = 256;
/// Practical shard cap; valid sets above this are rejected before candidate expansion.
pub const MAX_LOGICAL_MODEL_SHARDS: usize = 1_024;
/// Catalog metadata entry budget, intentionally below the general-purpose parser maximum.
pub const MAX_CATALOG_METADATA_ENTRIES: u64 = 16_384;
/// Aggregate metadata entry budget across one scan.
pub const MAX_TOTAL_CATALOG_METADATA_ENTRIES: u64 = 65_536;
/// Catalog tensor descriptor budget; catalog reads descriptors but never tensor payloads.
pub const MAX_CATALOG_TENSORS: u64 = 100_000;
/// Aggregate tensor descriptor budget across a scan. Catalog never reads tensor payloads, but the
/// pinned GGUF reader still validates descriptors, so storms of otherwise-small files are capped.
pub const MAX_TOTAL_CATALOG_TENSORS: u64 = 250_000;
// Modern GGUF tokenizers can legitimately make the metadata section larger than 10 MiB. Keep the
// parser bounded, but leave enough room for current llama.cpp vocabularies.
pub const MAX_CATALOG_METADATA_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CATALOG_DECODED_METADATA_BYTES: usize = 32 * 1024 * 1024;

/// Recursively scans `root` without following symlinks. This prevents both directory loops and
/// traversal through a link outside the configured root. Entry failures are retained as diagnostics.
#[must_use]
pub fn scan(root: &Path) -> ScanResult {
    scan_impl(root, &|| {}, &|_| {}, &|_, actual| actual)
}

/// Scans with a hook after each logical model's shard handles are opened and snapshotted.
/// The hook is an injection seam for deterministic filesystem replacement tests.
#[doc(hidden)]
#[must_use]
pub fn scan_with_hook(root: &Path, after_open: &dyn Fn(&[PathBuf])) -> ScanResult {
    scan_impl(root, &|| {}, after_open, &|_, actual| actual)
}

#[doc(hidden)]
#[must_use]
pub fn scan_with_discovery_hook(root: &Path, after_discovery: &dyn Fn()) -> ScanResult {
    scan_impl(root, after_discovery, &|_| {}, &|_, actual| actual)
}

#[doc(hidden)]
#[must_use]
pub fn scan_with_size_hook(root: &Path, size: &dyn Fn(&Path, u64) -> u64) -> ScanResult {
    scan_impl(root, &|| {}, &|_| {}, size)
}

fn scan_impl(
    root: &Path,
    after_discovery: &dyn Fn(),
    after_open: &dyn Fn(&[PathBuf]),
    size: &dyn Fn(&Path, u64) -> u64,
) -> ScanResult {
    let mut result = ScanResult::default();
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) => {
            result.diagnostics.push(scan_diagnostic(root, error));
            return result;
        }
    };
    if root_metadata.file_type().is_symlink() {
        result.diagnostics.push(CatalogDiagnostic {
            kind: CatalogDiagnosticKind::Scan,
            path: root.to_path_buf(),
            message: "catalog root must not be a symbolic link".into(),
        });
        return result;
    }
    if !root_metadata.is_dir() {
        result.diagnostics.push(CatalogDiagnostic {
            kind: CatalogDiagnosticKind::Scan,
            path: root.to_path_buf(),
            message: "catalog root is not a directory".into(),
        });
        return result;
    }
    let root_snapshot = match RootSnapshot::capture(root) {
        Ok(snapshot) => snapshot,
        Err(diagnostic) => {
            result.diagnostics.push(diagnostic);
            return result;
        }
    };
    result.complete = true;
    let mut files = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
    {
        match entry {
            Ok(entry) if entry.file_type().is_file() && is_gguf(entry.path()) => {
                if files.len() == MAX_DISCOVERED_GGUF_FILES {
                    result.complete = false;
                    push_scan_diagnostic(
                        &mut result,
                        CatalogDiagnostic {
                            kind: CatalogDiagnosticKind::Scan,
                            path: root.to_path_buf(),
                            message: format!(
                                "catalog file limit of {MAX_DISCOVERED_GGUF_FILES} was reached"
                            ),
                        },
                    );
                    break;
                }
                let path = entry.into_path();
                match canonical_within(&path, &root_snapshot.canonical) {
                    Ok(()) => files.push(path),
                    Err(diagnostic) => {
                        result.complete = false;
                        push_scan_diagnostic(&mut result, diagnostic);
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                result.complete = false;
                push_scan_diagnostic(
                    &mut result,
                    CatalogDiagnostic {
                        kind: CatalogDiagnosticKind::Scan,
                        path: error.path().unwrap_or(root).to_path_buf(),
                        message: error.to_string(),
                    },
                );
            }
        }
    }
    after_discovery();
    if let Err(diagnostic) = root_snapshot.validate(root) {
        result.complete = false;
        push_scan_diagnostic(&mut result, diagnostic);
        return result;
    }
    files.sort();

    let shard = Regex::new(r"(?i)^(.*)-(\d{5})-of-(\d{5})\.gguf$").expect("constant regex");
    let mut actual_files = std::collections::HashMap::<_, Vec<PathBuf>>::new();
    for path in &files {
        if let (Some(parent), Some(name)) = (
            path.parent(),
            path.file_name().and_then(|name| name.to_str()),
        ) {
            actual_files
                .entry((parent.to_path_buf(), name.to_ascii_lowercase()))
                .or_default()
                .push(path.clone());
        }
    }
    let ambiguous = actual_files
        .iter()
        .filter_map(|(key, paths)| (paths.len() > 1).then_some((key.clone(), paths.clone())))
        .collect::<Vec<_>>();
    for ((parent, name), paths) in &ambiguous {
        result.complete = false;
        push_scan_diagnostic(
            &mut result,
            CatalogDiagnostic {
                kind: CatalogDiagnosticKind::Scan,
                path: parent.join(name),
                message: format!("ambiguous case-insensitive GGUF filenames: {paths:?}"),
            },
        );
    }
    let ambiguous_keys = ambiguous
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<HashSet<_>>();
    let ambiguous_shard_groups = ambiguous
        .into_iter()
        .flat_map(|(_, paths)| paths)
        .filter_map(|path| {
            let parent = path.parent()?.to_path_buf();
            let captures = shard.captures(path.file_name()?.to_str()?)?;
            Some((
                parent,
                captures[1].to_ascii_lowercase(),
                captures[3].to_string(),
            ))
        })
        .collect::<HashSet<_>>();
    let mut consumed = HashSet::new();
    let mut budgets = ScanBudgets {
        remaining_metadata_entries: MAX_TOTAL_CATALOG_METADATA_ENTRIES,
        remaining_tensors: MAX_TOTAL_CATALOG_TENSORS,
    };
    for path in &files {
        if consumed.contains(path) {
            continue;
        }
        let normalized_key = path
            .parent()
            .zip(path.file_name().and_then(|name| name.to_str()))
            .map(|(parent, name)| (parent.to_path_buf(), name.to_ascii_lowercase()));
        if normalized_key
            .as_ref()
            .is_some_and(|key| ambiguous_keys.contains(key))
            || path
                .parent()
                .zip(path.file_name().and_then(|name| name.to_str()))
                .and_then(|(parent, name)| {
                    let captures = shard.captures(name)?;
                    Some((
                        parent.to_path_buf(),
                        captures[1].to_ascii_lowercase(),
                        captures[3].to_string(),
                    ))
                })
                .is_some_and(|key| ambiguous_shard_groups.contains(&key))
        {
            consumed.insert(path.clone());
            continue;
        }
        if result.models.len() == MAX_DISCOVERED_MODELS {
            result.complete = false;
            push_scan_diagnostic(
                &mut result,
                CatalogDiagnostic {
                    kind: CatalogDiagnosticKind::Scan,
                    path: root.to_path_buf(),
                    message: format!("catalog model limit of {MAX_DISCOVERED_MODELS} was reached"),
                },
            );
            break;
        }
        let mut logical_files = vec![path.clone()];
        if let Some(captures) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| shard.captures(name))
        {
            let index = captures[2].parse::<usize>().unwrap_or(0);
            let total = captures[3].parse::<usize>().unwrap_or(0);
            if total > MAX_LOGICAL_MODEL_SHARDS {
                consumed.insert(path.clone());
                result.complete = false;
                push_scan_diagnostic(
                    &mut result,
                    CatalogDiagnostic {
                        kind: CatalogDiagnosticKind::Scan,
                        path: path.clone(),
                        message: format!(
                            "declared shard total {total} exceeds limit {MAX_LOGICAL_MODEL_SHARDS}"
                        ),
                    },
                );
                continue;
            }
            if index == 1 && total > 1 {
                let prefix = &captures[1];
                let candidate_names = (1..=total)
                    .map(|part| {
                        format!("{prefix}-{part:05}-of-{total:05}.gguf").to_ascii_lowercase()
                    })
                    .collect::<Vec<_>>();
                let parent = path.parent().unwrap_or_else(|| Path::new(""));
                if candidate_names.iter().all(|name| {
                    actual_files
                        .get(&(parent.to_path_buf(), name.clone()))
                        .is_some_and(|paths| paths.len() == 1)
                }) {
                    logical_files = candidate_names
                        .iter()
                        .filter_map(|name| actual_files.get(&(parent.to_path_buf(), name.clone())))
                        .filter_map(|paths| paths.first().cloned())
                        .collect();
                }
            }
        }
        consumed.extend(logical_files.iter().cloned());
        let mut snapshots = Vec::with_capacity(logical_files.len());
        let mut launch_file = None;
        let mut size_bytes = 0_u64;
        let mut open_error = None;
        for shard_path in &logical_files {
            if let Err(diagnostic) = canonical_within(shard_path, &root_snapshot.canonical) {
                open_error = Some(diagnostic);
                break;
            }
            match OpenedShard::open(shard_path) {
                Ok((mut shard, file)) => {
                    shard.before.size = size(shard_path, shard.before.size);
                    let Some(total) = size_bytes.checked_add(shard.before.size) else {
                        open_error = Some(CatalogDiagnostic {
                            kind: CatalogDiagnosticKind::Scan,
                            path: shard_path.clone(),
                            message: "logical model size overflowed u64".into(),
                        });
                        break;
                    };
                    size_bytes = total;
                    if shard_path == path {
                        launch_file = Some(file);
                    }
                    snapshots.push(shard);
                }
                Err(error) => {
                    open_error = Some(error);
                    break;
                }
            }
        }
        if let Some(diagnostic) = open_error {
            result.complete = false;
            push_scan_diagnostic(&mut result, diagnostic);
            continue;
        }
        after_open(&logical_files);
        let launch = launch_file
            .as_ref()
            .expect("first logical shard was opened");
        let launch_snapshot = snapshots
            .iter()
            .find(|shard| shard.path == *path)
            .expect("first logical shard was snapshotted");
        let mut model_diagnostics = Vec::new();
        let parsed = read_model(
            path,
            size_bytes,
            launch_snapshot.before.size,
            launch_snapshot.before.identity.clone(),
            launch,
            &mut budgets,
            &mut model_diagnostics,
        );
        for diagnostic in model_diagnostics {
            push_scan_diagnostic(&mut result, diagnostic);
        }
        let validation_error = snapshots
            .iter()
            .find_map(|shard| shard.validate_unchanged((shard.path == *path).then_some(launch)));
        if let Some(diagnostic) = validation_error {
            result.complete = false;
            push_scan_diagnostic(&mut result, diagnostic);
            continue;
        }
        match parsed {
            Ok(model) => result.models.push(model),
            Err(diagnostic) => {
                result.complete = false;
                push_scan_diagnostic(&mut result, diagnostic);
            }
        }
    }
    if let Err(diagnostic) = root_snapshot.validate(root) {
        result.complete = false;
        result.models.clear();
        push_scan_diagnostic(&mut result, diagnostic);
    }
    result
}

struct RootSnapshot {
    canonical: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
}

impl RootSnapshot {
    fn capture(root: &Path) -> Result<Self, CatalogDiagnostic> {
        let canonical = root
            .canonicalize()
            .map_err(|error| scan_diagnostic(root, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(root).map_err(|error| scan_diagnostic(root, error))?;
            Ok(Self {
                canonical,
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            let (volume, file_index) = windows_directory_identity(root)
                .ok_or_else(|| scan_diagnostic(root, std::io::Error::last_os_error()))?;
            Ok(Self {
                canonical,
                volume: Some(volume),
                file_index: Some(file_index),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self { canonical })
        }
    }

    fn validate(&self, root: &Path) -> Result<(), CatalogDiagnostic> {
        let metadata = fs::symlink_metadata(root).map_err(|error| scan_diagnostic(root, error))?;
        let canonical = root
            .canonicalize()
            .map_err(|error| scan_diagnostic(root, error))?;
        let unchanged = !metadata.file_type().is_symlink() && canonical == self.canonical;
        #[cfg(unix)]
        let unchanged = {
            use std::os::unix::fs::MetadataExt;
            unchanged && metadata.dev() == self.device && metadata.ino() == self.inode
        };
        #[cfg(windows)]
        let unchanged =
            { unchanged && windows_directory_identity(root) == self.volume.zip(self.file_index) };
        if unchanged {
            Ok(())
        } else {
            Err(CatalogDiagnostic {
                kind: CatalogDiagnosticKind::Scan,
                path: root.to_path_buf(),
                message: "catalog root changed while it was being scanned".into(),
            })
        }
    }
}

fn canonical_within(path: &Path, canonical_root: &Path) -> Result<(), CatalogDiagnostic> {
    let canonical = path
        .canonicalize()
        .map_err(|error| scan_diagnostic(path, error))?;
    if canonical.starts_with(canonical_root) {
        Ok(())
    } else {
        Err(CatalogDiagnostic {
            kind: CatalogDiagnosticKind::Scan,
            path: path.to_path_buf(),
            message: "catalog entry resolves outside the configured root".into(),
        })
    }
}

struct FileSnapshot {
    size: u64,
    modified: std::time::SystemTime,
    identity: CatalogIdentity,
}

struct OpenedShard {
    path: PathBuf,
    before: FileSnapshot,
}

impl OpenedShard {
    fn open(path: &Path) -> Result<(Self, File), CatalogDiagnostic> {
        let file = File::open(path).map_err(|error| scan_diagnostic(path, error))?;
        let before = snapshot(&file, path)?;
        Ok((
            Self {
                path: path.to_path_buf(),
                before,
            },
            file,
        ))
    }

    fn validate_unchanged(&self, retained_file: Option<&File>) -> Option<CatalogDiagnostic> {
        let retained = retained_file.is_some();
        let reopened;
        let file = if let Some(file) = retained_file {
            file
        } else {
            reopened = match File::open(&self.path) {
                Ok(file) => file,
                Err(error) => return Some(scan_diagnostic(&self.path, error)),
            };
            &reopened
        };
        let after = match snapshot(file, &self.path) {
            Ok(snapshot) => snapshot,
            Err(diagnostic) => return Some(diagnostic),
        };
        let path_identity = if retained {
            CatalogIdentity::for_path(&self.path)
        } else {
            after.identity.clone()
        };
        if after.size != self.before.size
            || after.modified != self.before.modified
            || after.identity != self.before.identity
            || (self.before.identity != CatalogIdentity::Unavailable
                && path_identity != self.before.identity)
        {
            Some(CatalogDiagnostic {
                kind: CatalogDiagnosticKind::Scan,
                path: self.path.clone(),
                message: "model shard changed while it was being scanned".into(),
            })
        } else {
            None
        }
    }
}

fn snapshot(file: &File, path: &Path) -> Result<FileSnapshot, CatalogDiagnostic> {
    let metadata = file
        .metadata()
        .map_err(|error| scan_diagnostic(path, error))?;
    let modified = metadata
        .modified()
        .map_err(|error| scan_diagnostic(path, error))?;
    Ok(FileSnapshot {
        size: metadata.len(),
        modified,
        identity: CatalogIdentity::for_file(file),
    })
}

fn scan_diagnostic(path: &Path, error: std::io::Error) -> CatalogDiagnostic {
    CatalogDiagnostic {
        kind: CatalogDiagnosticKind::Scan,
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn push_scan_diagnostic(result: &mut ScanResult, diagnostic: CatalogDiagnostic) {
    if result.diagnostics.len() < MAX_CATALOG_DIAGNOSTICS {
        result.diagnostics.push(diagnostic);
    } else {
        result.complete = false;
    }
}

fn is_gguf(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("gguf"))
}

fn read_model(
    path: &Path,
    size_bytes: u64,
    expected_launch_size: u64,
    identity: CatalogIdentity,
    file: &File,
    budgets: &mut ScanBudgets,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) -> Result<ScannedModel, CatalogDiagnostic> {
    let fallback = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("model");
    let fallback = Regex::new(r"(?i)-\d{5}-of-\d{5}$")
        .expect("constant regex")
        .replace(fallback, "")
        .into_owned();
    if let Some(limit) = catalog_header_limit(file, budgets) {
        let (kind, message) = match limit {
            CatalogHeaderLimit::Metadata(message) => (CatalogDiagnosticKind::Metadata, message),
            CatalogHeaderLimit::Incomplete(message) => {
                return Err(CatalogDiagnostic {
                    kind: CatalogDiagnosticKind::Scan,
                    path: path.to_path_buf(),
                    message,
                });
            }
        };
        diagnostics.push(CatalogDiagnostic {
            kind,
            path: path.to_path_buf(),
            message,
        });
        return Ok(ScannedModel {
            display_name: fallback,
            path: path.to_path_buf(),
            size_bytes,
            identity,
            metadata: CatalogMetadata::default(),
        });
    }
    let config = GGUFReaderConfig {
        validate_integrity: true,
        eager_load_tensors: false,
        max_file_size: 0,
        max_metadata_size: MAX_CATALOG_METADATA_BYTES,
        max_decoded_metadata_size: MAX_CATALOG_DECODED_METADATA_BYTES,
        buffer_size: 64 * 1024,
        use_mmap: false,
    };
    let (display_name, metadata) = match GGUFFileReader::with_config(file, config) {
        Ok(reader) => {
            let values = reader.metadata();
            let string = |key: &str| match values.get(key) {
                Some(MetadataValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            let integer = |key: &str| match values.get(key) {
                Some(MetadataValue::U64(value)) => Some(*value),
                Some(MetadataValue::U32(value)) => Some(u64::from(*value)),
                _ => None,
            };
            let architecture = string("general.architecture");
            if values.get("general.name").is_some()
                && !matches!(values.get("general.name"), Some(MetadataValue::String(_)))
            {
                diagnostics.push(CatalogDiagnostic {
                    kind: CatalogDiagnosticKind::Metadata,
                    path: path.to_path_buf(),
                    message: "general.name has the wrong metadata type".into(),
                });
            }
            let context_length = architecture
                .as_deref()
                .and_then(|arch| integer(&format!("{arch}.context_length")));
            let quantization = match integer("general.file_type") {
                Some(value) => {
                    let (display, known) = llama_file_type(value);
                    if !known {
                        diagnostics.push(CatalogDiagnostic {
                            kind: CatalogDiagnosticKind::Metadata,
                            path: path.to_path_buf(),
                            message: format!("unknown general.file_type value {value}"),
                        });
                    }
                    Some(display)
                }
                None => None,
            };
            (
                string("general.name").unwrap_or_else(|| fallback.clone()),
                CatalogMetadata {
                    architecture: architecture.clone(),
                    parameter_count: integer("general.parameter_count"),
                    quantization,
                    quantization_version: integer("general.quantization_version"),
                    context_length,
                    block_count: architecture
                        .as_deref()
                        .and_then(|arch| integer(&format!("{arch}.block_count"))),
                    embedding_length: architecture
                        .as_deref()
                        .and_then(|arch| integer(&format!("{arch}.embedding_length"))),
                    attention_head_count: architecture
                        .as_deref()
                        .and_then(|arch| integer(&format!("{arch}.attention.head_count"))),
                    attention_head_count_kv: architecture
                        .as_deref()
                        .and_then(|arch| integer(&format!("{arch}.attention.head_count_kv"))),
                    attention_key_length: architecture
                        .as_deref()
                        .and_then(|arch| integer(&format!("{arch}.attention.key_length"))),
                    attention_value_length: architecture
                        .as_deref()
                        .and_then(|arch| integer(&format!("{arch}.attention.value_length"))),
                    full_attention_interval: architecture
                        .as_deref()
                        .and_then(|arch| integer(&format!("{arch}.full_attention_interval"))),
                },
            )
        }
        Err(gguf_rs_lib::GGUFError::Io(io_error))
            if io_error.kind() != std::io::ErrorKind::UnexpectedEof
                || fs::metadata(path).map(|metadata| metadata.len()).ok()
                    != Some(expected_launch_size) =>
        {
            return Err(CatalogDiagnostic {
                kind: CatalogDiagnosticKind::Scan,
                path: path.to_path_buf(),
                message: io_error.to_string(),
            });
        }
        Err(error @ gguf_rs_lib::GGUFError::UnexpectedEof)
            if fs::metadata(path).map(|metadata| metadata.len()).ok()
                != Some(expected_launch_size) =>
        {
            return Err(CatalogDiagnostic {
                kind: CatalogDiagnosticKind::Scan,
                path: path.to_path_buf(),
                message: error.to_string(),
            });
        }
        Err(error) => {
            diagnostics.push(CatalogDiagnostic {
                kind: CatalogDiagnosticKind::Metadata,
                path: path.to_path_buf(),
                message: error.to_string(),
            });
            (fallback, CatalogMetadata::default())
        }
    };
    Ok(ScannedModel {
        display_name,
        path: path.to_path_buf(),
        size_bytes,
        identity,
        metadata,
    })
}

enum CatalogHeaderLimit {
    Metadata(String),
    Incomplete(String),
}

struct ScanBudgets {
    remaining_metadata_entries: u64,
    remaining_tensors: u64,
}

fn catalog_header_limit(file: &File, budgets: &mut ScanBudgets) -> Option<CatalogHeaderLimit> {
    let mut reader = file;
    let mut header = [0_u8; 24];
    reader.seek(SeekFrom::Start(0)).ok()?;
    let read = reader.read_exact(&mut header);
    let _ = reader.seek(SeekFrom::Start(0));
    if read.is_err() || u32::from_le_bytes(header[0..4].try_into().ok()?) != 0x4655_4747 {
        return None;
    }
    let tensor_count = u64::from_le_bytes(header[8..16].try_into().ok()?);
    let metadata_count = u64::from_le_bytes(header[16..24].try_into().ok()?);
    if tensor_count > MAX_CATALOG_TENSORS {
        Some(CatalogHeaderLimit::Metadata(format!(
            "catalog tensor count limit {MAX_CATALOG_TENSORS} exceeded by {tensor_count}"
        )))
    } else if tensor_count > budgets.remaining_tensors {
        Some(CatalogHeaderLimit::Incomplete(format!(
            "catalog total tensor descriptor budget {MAX_TOTAL_CATALOG_TENSORS} exhausted"
        )))
    } else {
        budgets.remaining_tensors -= tensor_count;
        if metadata_count > MAX_CATALOG_METADATA_ENTRIES {
            Some(CatalogHeaderLimit::Metadata(format!(
                "catalog metadata entry limit {MAX_CATALOG_METADATA_ENTRIES} exceeded by {metadata_count}"
            )))
        } else if metadata_count > budgets.remaining_metadata_entries {
            Some(CatalogHeaderLimit::Metadata(format!(
                "catalog total metadata entry budget {MAX_TOTAL_CATALOG_METADATA_ENTRIES} exhausted"
            )))
        } else {
            budgets.remaining_metadata_entries -= metadata_count;
            None
        }
    }
}

fn llama_file_type(value: u64) -> (String, bool) {
    // Compatibility snapshot: llama.cpp include/llama.h `enum llama_ftype` at
    // cc83d7b4824f73cfdda4dfbb47ee39804f71b328 (captured 2026-08-29).
    // These are model file types, not ggml tensor type enum values.
    let display = match value {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        4 => "Q4_1_SOME_F16",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        36 => "TQ1_0",
        37 => "TQ2_0",
        38 => "MXFP4_MOE",
        39 => "NVFP4",
        40 => "Q1_0",
        41 => "Q2_0",
        _ => return (format!("FILE_TYPE_{value}"), false),
    };
    (display.into(), true)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileOptions {
    pub remove_missing: Vec<ModelId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileResult {
    pub config: LauncherConfig,
    pub diagnostics: Vec<CatalogDiagnostic>,
}

#[must_use]
pub fn reconcile_catalog(
    saved: &LauncherConfig,
    scanned: ScanResult,
    options: ReconcileOptions,
) -> ReconcileResult {
    if !scanned.complete {
        return ReconcileResult {
            config: saved.clone(),
            diagnostics: scanned.diagnostics,
        };
    }
    let removed = options.remove_missing.into_iter().collect::<HashSet<_>>();
    let mut records = saved
        .models
        .iter()
        .filter(|record| !removed.contains(&record.id))
        .cloned()
        .collect::<Vec<_>>();
    let saved_len = records.len();
    for record in &mut records {
        record.state = ModelState::Missing;
    }
    let mut matched = HashSet::new();
    let mut used_keys = records
        .iter()
        .map(|record| record.key.as_str().to_owned())
        .collect::<HashSet<_>>();
    let scanned_identity_counts = scanned
        .models
        .iter()
        .filter(|model| model.identity != CatalogIdentity::Unavailable)
        .fold(std::collections::HashMap::new(), |mut counts, model| {
            *counts.entry(model.identity.clone()).or_insert(0_usize) += 1;
            counts
        });
    let saved_identity_counts = records[..saved_len]
        .iter()
        .filter(|record| record.file_identity != CatalogIdentity::Unavailable)
        .fold(std::collections::HashMap::new(), |mut counts, record| {
            *counts
                .entry(record.file_identity.clone())
                .or_insert(0_usize) += 1;
            counts
        });

    for model in scanned.models {
        let normalized = normalized_catalog_path(&model.path);
        let path_match = (0..saved_len).find(|index| {
            !matched.contains(index) && normalized_catalog_path(&records[*index].path) == normalized
        });
        let identity_match = path_match.or_else(|| {
            (model.identity != CatalogIdentity::Unavailable
                && scanned_identity_counts.get(&model.identity) == Some(&1)
                && saved_identity_counts.get(&model.identity) == Some(&1))
            .then(|| {
                (0..saved_len).find(|index| {
                    !matched.contains(index) && records[*index].file_identity == model.identity
                })
            })
            .flatten()
        });
        if let Some(index) = identity_match {
            matched.insert(index);
            let record = &mut records[index];
            record.path = model.path;
            record.file_identity = model.identity;
            record.size_bytes = model.size_bytes;
            record.metadata = model.metadata;
            record.state = ModelState::Available;
        } else {
            let key = unique_key(&model.display_name, &mut used_keys);
            records.push(ModelRecord {
                id: ModelId::new(),
                key,
                display_name: model.display_name,
                path: model.path,
                file_identity: model.identity,
                size_bytes: model.size_bytes,
                metadata: model.metadata,
                state: ModelState::Available,
                launch_profile: LaunchProfile {
                    settings: saved.default_launch_settings.clone(),
                },
            });
        }
    }
    let mut config = saved.clone();
    config.models = records;
    ReconcileResult {
        config,
        diagnostics: scanned.diagnostics,
    }
}

fn normalized_catalog_path(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

fn unique_key(name: &str, used: &mut HashSet<String>) -> ModelKey {
    let mut base = name
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    base = base.trim_matches('-').to_owned();
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    if base.is_empty() {
        base = "model".into();
    }
    let mut candidate = base.clone();
    let mut suffix = 2;
    while used.contains(&candidate) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    used.insert(candidate.clone());
    ModelKey::parse(candidate).expect("generated key is URL safe")
}

/// Deterministic debounce primitive used by the filesystem watcher adapter.
pub struct CatalogDebouncer {
    delay: Duration,
    paths: BTreeSet<PathBuf>,
}

impl CatalogDebouncer {
    #[must_use]
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            paths: BTreeSet::new(),
        }
    }
    pub fn changed(&mut self, path: PathBuf) {
        self.paths.insert(path);
    }
    pub async fn next(&mut self) -> Vec<PathBuf> {
        tokio::time::sleep(self.delay).await;
        std::mem::take(&mut self.paths).into_iter().collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogWatchEvent {
    Changed(PathBuf),
    Rescan,
    Error(String),
}

#[derive(Clone)]
pub struct CatalogWatchSender {
    events: mpsc::Sender<CatalogWatchEvent>,
    dropped: Arc<AtomicU64>,
}

impl CatalogWatchSender {
    pub fn emit(&self, event: CatalogWatchEvent) {
        if matches!(
            self.events.try_send(event),
            Err(mpsc::error::TrySendError::Full(_))
        ) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub struct CatalogWatchReceiver {
    events: mpsc::Receiver<CatalogWatchEvent>,
    dropped: Arc<AtomicU64>,
    delay: Duration,
    max_latency: Duration,
}

/// Maximum callback events waiting in memory. Path events are coalesced into a full rescan.
pub const WATCH_CHANNEL_CAPACITY: usize = 64;
/// Maximum individual backend error strings retained per reconciliation batch.
pub const WATCH_MAX_BATCH_DIAGNOSTICS: usize = 32;
/// A storm cannot postpone reconciliation beyond this duration.
pub const WATCH_MAX_LATENCY: Duration = Duration::from_secs(2);

#[must_use]
pub fn catalog_watch_channel(delay: Duration) -> (CatalogWatchSender, CatalogWatchReceiver) {
    catalog_watch_channel_with_limits(delay, WATCH_MAX_LATENCY, WATCH_CHANNEL_CAPACITY)
}

#[doc(hidden)]
#[must_use]
pub fn catalog_watch_channel_with_limits(
    delay: Duration,
    max_latency: Duration,
    capacity: usize,
) -> (CatalogWatchSender, CatalogWatchReceiver) {
    let (sender, events) = mpsc::channel(capacity.max(1));
    let dropped = Arc::new(AtomicU64::new(0));
    (
        CatalogWatchSender {
            events: sender,
            dropped: dropped.clone(),
        },
        CatalogWatchReceiver {
            events,
            dropped,
            delay,
            max_latency,
        },
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogWatchBatch {
    pub rescan_required: bool,
    pub errors: Vec<String>,
    pub dropped_count: u64,
}

impl CatalogWatchReceiver {
    pub async fn wait_next_batch(&mut self) -> Option<CatalogWatchBatch> {
        let first = self.events.recv().await?;
        let mut errors = Vec::new();
        let mut locally_dropped = 0_u64;
        retain_watch_event(first, &mut errors, &mut locally_dropped);
        let quiet = tokio::time::sleep(self.delay);
        let hard = tokio::time::sleep(self.max_latency);
        tokio::pin!(quiet);
        tokio::pin!(hard);
        loop {
            tokio::select! {
                () = &mut quiet => break,
                () = &mut hard => break,
                event = self.events.recv() => match event {
                    Some(event) => {
                        retain_watch_event(event, &mut errors, &mut locally_dropped);
                        quiet.as_mut().reset(tokio::time::Instant::now() + self.delay);
                    }
                    None => break,
                },
            }
        }
        Some(CatalogWatchBatch {
            rescan_required: true,
            errors,
            dropped_count: locally_dropped + self.dropped.swap(0, Ordering::Relaxed),
        })
    }
}

fn retain_watch_event(event: CatalogWatchEvent, errors: &mut Vec<String>, dropped: &mut u64) {
    if let CatalogWatchEvent::Error(message) = event {
        if errors.len() < WATCH_MAX_BATCH_DIAGNOSTICS {
            errors.push(message);
        } else {
            *dropped = dropped.saturating_add(1);
        }
    }
}

/// Reconciliation pipeline. Loading, scanning, and saving are separate calls, so the ConfigStore's
/// internal transaction lock is never held across directory traversal or metadata I/O.
#[derive(Clone)]
pub struct CatalogService {
    root: PathBuf,
    store: ConfigStore,
}

impl CatalogService {
    pub fn new(root: impl Into<PathBuf>, store: ConfigStore) -> Self {
        Self {
            root: root.into(),
            store,
        }
    }

    pub fn reconcile_now(&self) -> Result<ReconcileResult, AppError> {
        let scanned = scan(&self.root);
        self.reconcile_scan(scanned)
    }

    pub fn reconcile_scan(&self, scanned: ScanResult) -> Result<ReconcileResult, AppError> {
        let complete = scanned.complete;
        if !complete {
            let saved = self.store.load()?;
            return Ok(reconcile_catalog(
                &saved,
                scanned,
                ReconcileOptions::default(),
            ));
        }
        let mut diagnostics = Vec::new();
        let config = self.store.update(|latest| {
            let output = reconcile_catalog(latest, scanned, ReconcileOptions::default());
            diagnostics = output.diagnostics;
            *latest = output.config;
            Ok(())
        })?;
        Ok(ReconcileResult {
            config,
            diagnostics,
        })
    }

    pub async fn process_next(
        &self,
        receiver: &mut CatalogWatchReceiver,
    ) -> Result<Option<ReconcileResult>, AppError> {
        let Some(batch) = receiver.wait_next_batch().await else {
            return Ok(None);
        };
        self.process_batch(batch).map(Some)
    }

    /// Applies an already-debounced watcher batch. Callers can place the complete scan, durable
    /// reconciliation, and their in-memory assignment under one application mutation gate.
    pub fn process_batch(&self, batch: CatalogWatchBatch) -> Result<ReconcileResult, AppError> {
        let mut watch_diagnostics = batch
            .errors
            .into_iter()
            .map(|message| CatalogDiagnostic {
                kind: CatalogDiagnosticKind::Scan,
                path: self.root.clone(),
                message,
            })
            .collect::<Vec<_>>();
        if batch.dropped_count > 0 {
            watch_diagnostics.push(CatalogDiagnostic {
                kind: CatalogDiagnosticKind::Scan,
                path: self.root.clone(),
                message: format!(
                    "watch event buffer overflow: dropped {}; full rescan required",
                    batch.dropped_count
                ),
            });
        }
        let mut output = self.reconcile_now()?;
        watch_diagnostics.append(&mut output.diagnostics);
        output.diagnostics = watch_diagnostics;
        Ok(output)
    }
}

/// `notify` adapter. It reports typed events after the filesystem has been quiet for the configured
/// interval; callback failures remain visible rather than being silently dropped.
pub struct CatalogWatcher {
    _watcher: PollWatcher,
    events: CatalogWatchReceiver,
}

impl CatalogWatcher {
    pub fn watch(root: &Path, delay: Duration) -> notify::Result<Self> {
        let (sender, events) = catalog_watch_channel(delay);
        let mut watcher = PollWatcher::new(
            move |event: notify::Result<notify::Event>| match event {
                Ok(event) => {
                    for path in event.paths {
                        sender.emit(CatalogWatchEvent::Changed(path));
                    }
                }
                Err(error) => sender.emit(CatalogWatchEvent::Error(error.to_string())),
            },
            NotifyConfig::default().with_poll_interval(Duration::from_millis(50)),
        )?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            events,
        })
    }

    pub async fn next(&mut self) -> Option<CatalogWatchBatch> {
        self.wait_next_batch().await
    }

    pub async fn wait_next_batch(&mut self) -> Option<CatalogWatchBatch> {
        self.events.wait_next_batch().await
    }

    pub async fn process_next(
        &mut self,
        service: &CatalogService,
    ) -> Result<Option<ReconcileResult>, AppError> {
        service.process_next(&mut self.events).await
    }
}
