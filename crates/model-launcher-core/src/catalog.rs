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
        let Ok(_metadata) = fs::metadata(path) else {
            return Self::Unavailable;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self::Unix {
                device: _metadata.dev(),
                inode: _metadata.ino(),
            }
        }
        #[cfg(windows)]
        {
            windows_identity(path).unwrap_or(Self::Unavailable)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = _metadata;
            Self::Unavailable
        }
    }
}

#[cfg(windows)]
fn windows_identity(path: &Path) -> Option<CatalogIdentity> {
    use std::{mem::MaybeUninit, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let file = File::open(path).ok()?;
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
        let mut size_bytes = 0_u64;
        let mut launch_size_bytes = None;
        let mut size_error = None;
        for shard_path in &logical_files {
            match fs::metadata(shard_path) {
                Ok(metadata) => {
                    if let Err(error) = File::open(shard_path) {
                        size_error = Some(scan_diagnostic(shard_path, error));
                        break;
                    }
                    if shard_path == path {
                        launch_size_bytes = Some(metadata.len());
                    }
                    size_bytes = size_bytes.saturating_add(metadata.len());
                }
                Err(error) => {
                    size_error = Some(scan_diagnostic(shard_path, error));
                    break;
                }
            }
        }
        if let Some(diagnostic) = size_error {
            result.complete = false;
            result.diagnostics.push(diagnostic);
            continue;
        }
        match File::open(path) {
            Ok(file) => match read_model(
                path,
                size_bytes,
                launch_size_bytes.unwrap_or_default(),
                file,
                &mut result.diagnostics,
            ) {
                Ok(model) => result.models.push(model),
                Err(diagnostic) => {
                    result.complete = false;
                    result.diagnostics.push(diagnostic);
                }
            },
            Err(error) => {
                result.complete = false;
                result.diagnostics.push(scan_diagnostic(path, error));
            }
        }
    }
    result
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
    file: File,
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
                    let (display, known) = ggml_file_type(value);
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
        identity: CatalogIdentity::for_path(path),
        metadata,
    })
}

fn ggml_file_type(value: u64) -> (String, bool) {
    let display = match value {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        4 => "Q4_2",
        5 => "Q4_3",
        6 => "Q5_0",
        7 => "Q5_1",
        8 => "Q8_0",
        9 => "Q8_1",
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
        21 => "IQ3_XXS",
        22 => "IQ1_S",
        23 => "IQ4_NL",
        24 => "IQ3_S",
        25 => "IQ2_S",
        26 => "IQ4_XS",
        27 => "IQ1_M",
        28 => "BF16",
        29 => "TQ1_0",
        30 => "TQ2_0",
        31 => "MXFP4",
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
        let saved = self.store.load()?;
        let scanned = scan(&self.root);
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
