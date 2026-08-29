use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File},
    path::{Path, PathBuf},
    time::Duration,
};

use gguf_rs_lib::{format::MetadataValue, reader::GGUFFileReader};
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
        inode: u64,
    },
    Windows {
        volume: u64,
        file_index: u64,
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
            Self::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
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
    Some(CatalogIdentity::Windows {
        volume: u64::from(information.dwVolumeSerialNumber),
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogMetadata {
    pub architecture: Option<String>,
    pub parameter_count: Option<u64>,
    pub quantization: Option<String>,
    pub quantization_version: Option<u64>,
    pub context_length: Option<u64>,
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

/// Recursively scans `root` without following symlinks. This prevents both directory loops and
/// traversal through a link outside the configured root. Entry failures are retained as diagnostics.
#[must_use]
pub fn scan(root: &Path) -> ScanResult {
    scan_with_hook(root, &|_| {})
}

/// Scans with a hook after each logical model's shard handles are opened and snapshotted.
/// The hook is an injection seam for deterministic filesystem replacement tests.
#[doc(hidden)]
#[must_use]
pub fn scan_with_hook(root: &Path, after_open: &dyn Fn(&[PathBuf])) -> ScanResult {
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
    result.complete = true;
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        match entry {
            Ok(entry) if entry.file_type().is_file() && is_gguf(entry.path()) => {
                files.push(entry.into_path())
            }
            Ok(_) => {}
            Err(error) => {
                result.complete = false;
                result.diagnostics.push(CatalogDiagnostic {
                    kind: CatalogDiagnosticKind::Scan,
                    path: error.path().unwrap_or(root).to_path_buf(),
                    message: error.to_string(),
                });
            }
        }
    }
    files.sort();

    let shard = Regex::new(r"(?i)^(.*)-(\d{5})-of-(\d{5})\.gguf$").expect("constant regex");
    let actual_files = files
        .iter()
        .filter_map(|path| {
            Some((
                (
                    path.parent()?.to_path_buf(),
                    path.file_name()?.to_str()?.to_ascii_lowercase(),
                ),
                path.clone(),
            ))
        })
        .collect::<std::collections::HashMap<_, _>>();
    let mut consumed = HashSet::new();
    for path in &files {
        if consumed.contains(path) {
            continue;
        }
        let mut logical_files = vec![path.clone()];
        if let Some(captures) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| shard.captures(name))
        {
            let index = captures[2].parse::<usize>().unwrap_or(0);
            let total = captures[3].parse::<usize>().unwrap_or(0);
            if index == 1 && total > 1 {
                let prefix = &captures[1];
                let candidate_names = (1..=total)
                    .map(|part| {
                        format!("{prefix}-{part:05}-of-{total:05}.gguf").to_ascii_lowercase()
                    })
                    .collect::<Vec<_>>();
                let parent = path.parent().unwrap_or_else(|| Path::new(""));
                if candidate_names
                    .iter()
                    .all(|name| actual_files.contains_key(&(parent.to_path_buf(), name.clone())))
                {
                    logical_files = candidate_names
                        .iter()
                        .filter_map(|name| actual_files.get(&(parent.to_path_buf(), name.clone())))
                        .cloned()
                        .collect();
                }
            }
        }
        consumed.extend(logical_files.iter().cloned());
        let mut opened = Vec::with_capacity(logical_files.len());
        let mut open_error = None;
        for shard_path in &logical_files {
            match OpenedShard::open(shard_path) {
                Ok(shard) => opened.push(shard),
                Err(error) => {
                    open_error = Some(error);
                    break;
                }
            }
        }
        if let Some(diagnostic) = open_error {
            result.complete = false;
            result.diagnostics.push(diagnostic);
            continue;
        }
        after_open(&logical_files);
        let size_bytes = opened.iter().map(|shard| shard.before.size).sum();
        let launch = &opened[0];
        let parsed = read_model(
            path,
            size_bytes,
            launch.before.size,
            launch.before.identity.clone(),
            &launch.file,
            &mut result.diagnostics,
        );
        let validation_error = opened.iter().find_map(OpenedShard::validate_unchanged);
        if let Some(diagnostic) = validation_error {
            result.complete = false;
            result.diagnostics.push(diagnostic);
            continue;
        }
        match parsed {
            Ok(model) => result.models.push(model),
            Err(diagnostic) => {
                result.complete = false;
                result.diagnostics.push(diagnostic);
            }
        }
    }
    result
}

struct FileSnapshot {
    size: u64,
    modified: std::time::SystemTime,
    identity: CatalogIdentity,
}

struct OpenedShard {
    path: PathBuf,
    file: File,
    before: FileSnapshot,
}

impl OpenedShard {
    fn open(path: &Path) -> Result<Self, CatalogDiagnostic> {
        let file = File::open(path).map_err(|error| scan_diagnostic(path, error))?;
        let before = snapshot(&file, path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            before,
        })
    }

    fn validate_unchanged(&self) -> Option<CatalogDiagnostic> {
        let after = match snapshot(&self.file, &self.path) {
            Ok(snapshot) => snapshot,
            Err(diagnostic) => return Some(diagnostic),
        };
        let path_identity = CatalogIdentity::for_path(&self.path);
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
    let (display_name, metadata) = match GGUFFileReader::new(file) {
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
                    architecture,
                    parameter_count: integer("general.parameter_count"),
                    quantization,
                    quantization_version: integer("general.quantization_version"),
                    context_length,
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
    for record in &mut records {
        record.state = ModelState::Missing;
    }
    let mut matched = HashSet::new();
    let mut used_keys = records
        .iter()
        .map(|record| record.key.as_str().to_owned())
        .collect::<HashSet<_>>();

    for model in scanned.models {
        let existing = records.iter_mut().enumerate().find(|(index, record)| {
            !matched.contains(index)
                && ((model.identity != CatalogIdentity::Unavailable
                    && record.file_identity == model.identity)
                    || record.path == model.path)
        });
        if let Some((index, record)) = existing {
            matched.insert(index);
            record.path = model.path;
            record.file_identity = model.identity;
            record.size_bytes = model.size_bytes;
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
                state: ModelState::Available,
                launch_profile: LaunchProfile::default(),
            });
        }
    }
    ReconcileResult {
        config: LauncherConfig { models: records },
        diagnostics: scanned.diagnostics,
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
pub struct CatalogWatchSender(mpsc::UnboundedSender<CatalogWatchEvent>);

impl CatalogWatchSender {
    pub fn emit(&self, event: CatalogWatchEvent) {
        let _ = self.0.send(event);
    }
}

pub struct CatalogWatchReceiver {
    events: mpsc::UnboundedReceiver<CatalogWatchEvent>,
    delay: Duration,
}

#[must_use]
pub fn catalog_watch_channel(delay: Duration) -> (CatalogWatchSender, CatalogWatchReceiver) {
    let (sender, events) = mpsc::unbounded_channel();
    (
        CatalogWatchSender(sender),
        CatalogWatchReceiver { events, delay },
    )
}

impl CatalogWatchReceiver {
    async fn next_batch(&mut self) -> Option<Vec<CatalogWatchEvent>> {
        let first = self.events.recv().await?;
        let mut batch = vec![first];
        loop {
            match tokio::time::timeout(self.delay, self.events.recv()).await {
                Ok(Some(event)) => batch.push(event),
                Ok(None) | Err(_) => return Some(batch),
            }
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
        let saved = self.store.load()?;
        let complete = scanned.complete;
        let output = reconcile_catalog(&saved, scanned, ReconcileOptions::default());
        if complete {
            self.store.save(&output.config)?;
        }
        Ok(output)
    }

    pub async fn process_next(
        &self,
        receiver: &mut CatalogWatchReceiver,
    ) -> Result<Option<ReconcileResult>, AppError> {
        let Some(batch) = receiver.next_batch().await else {
            return Ok(None);
        };
        let mut watch_diagnostics = batch
            .into_iter()
            .filter_map(|event| match event {
                CatalogWatchEvent::Error(message) => Some(CatalogDiagnostic {
                    kind: CatalogDiagnosticKind::Scan,
                    path: self.root.clone(),
                    message,
                }),
                CatalogWatchEvent::Changed(_) | CatalogWatchEvent::Rescan => None,
            })
            .collect::<Vec<_>>();
        let mut output = self.reconcile_now()?;
        watch_diagnostics.append(&mut output.diagnostics);
        output.diagnostics = watch_diagnostics;
        Ok(Some(output))
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

    pub async fn next(&mut self) -> Option<Vec<CatalogWatchEvent>> {
        self.events.next_batch().await
    }

    pub async fn process_next(
        &mut self,
        service: &CatalogService,
    ) -> Result<Option<ReconcileResult>, AppError> {
        service.process_next(&mut self.events).await
    }
}
