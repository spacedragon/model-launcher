use std::{
    collections::{BTreeSet, HashSet},
    fs::{self, File},
    path::{Path, PathBuf},
    time::Duration,
};

use gguf_rs_lib::{format::MetadataValue, reader::GGUFFileReader};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use walkdir::WalkDir;

use crate::{LaunchProfile, LauncherConfig, ModelId, ModelKey, ModelRecord, ModelState};

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
        let Ok(metadata) = fs::metadata(path) else {
            return Self::Unavailable;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(windows)]
        {
            windows_identity(path).unwrap_or(Self::Unavailable)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = metadata;
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
}

/// Recursively scans `root` without following symlinks. This prevents both directory loops and
/// traversal through a link outside the configured root. Entry failures are retained as diagnostics.
#[must_use]
pub fn scan(root: &Path) -> ScanResult {
    let mut result = ScanResult::default();
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        match entry {
            Ok(entry) if entry.file_type().is_file() && is_gguf(entry.path()) => {
                files.push(entry.into_path())
            }
            Ok(_) => {}
            Err(error) => result.diagnostics.push(CatalogDiagnostic {
                kind: CatalogDiagnosticKind::Scan,
                path: error.path().unwrap_or(root).to_path_buf(),
                message: error.to_string(),
            }),
        }
    }
    files.sort();

    let shard = Regex::new(r"(?i)^(.*)-(\d{5})-of-(\d{5})\.gguf$").expect("constant regex");
    let file_set = files.iter().cloned().collect::<HashSet<_>>();
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
                let candidates = (1..=total)
                    .map(|part| {
                        path.with_file_name(format!("{prefix}-{part:05}-of-{total:05}.gguf"))
                    })
                    .collect::<Vec<_>>();
                if candidates
                    .iter()
                    .all(|candidate| file_set.contains(candidate))
                {
                    logical_files = candidates;
                }
            }
        }
        consumed.extend(logical_files.iter().cloned());
        result
            .models
            .push(read_model(path, &logical_files, &mut result.diagnostics));
    }
    result
}

fn is_gguf(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("gguf"))
}

fn read_model(
    path: &Path,
    shards: &[PathBuf],
    diagnostics: &mut Vec<CatalogDiagnostic>,
) -> ScannedModel {
    let fallback = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("model");
    let fallback = Regex::new(r"(?i)-\d{5}-of-\d{5}$")
        .expect("constant regex")
        .replace(fallback, "")
        .into_owned();
    let size_bytes = shards
        .iter()
        .filter_map(|shard| fs::metadata(shard).ok().map(|metadata| metadata.len()))
        .sum();
    let (display_name, metadata) = match File::open(path)
        .and_then(|file| GGUFFileReader::new(file).map_err(std::io::Error::other))
    {
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
            let context_length = architecture
                .as_deref()
                .and_then(|arch| integer(&format!("{arch}.context_length")));
            (
                string("general.name").unwrap_or_else(|| fallback.clone()),
                CatalogMetadata {
                    architecture,
                    parameter_count: integer("general.parameter_count"),
                    quantization: string("general.quantization"),
                    context_length,
                },
            )
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
    ScannedModel {
        display_name,
        path: path.to_path_buf(),
        size_bytes,
        identity: CatalogIdentity::for_path(path),
        metadata,
    }
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

/// `notify` adapter. It reports a stable, sorted batch after the filesystem has been quiet for the
/// configured interval; callers decide when to invoke pure `scan` and persist through ConfigStore.
pub struct CatalogWatcher {
    _watcher: RecommendedWatcher,
    events: mpsc::UnboundedReceiver<PathBuf>,
    delay: Duration,
}

impl CatalogWatcher {
    pub fn watch(root: &Path, delay: Duration) -> notify::Result<Self> {
        let (sender, events) = mpsc::unbounded_channel();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event {
                    for path in event.paths {
                        let _ = sender.send(path);
                    }
                }
            })?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            events,
            delay,
        })
    }

    pub async fn next(&mut self) -> Option<Vec<PathBuf>> {
        let first = self.events.recv().await?;
        let mut paths = BTreeSet::from([first]);
        loop {
            match tokio::time::timeout(self.delay, self.events.recv()).await {
                Ok(Some(path)) => {
                    paths.insert(path);
                }
                Ok(None) | Err(_) => return Some(paths.into_iter().collect()),
            }
        }
    }
}
