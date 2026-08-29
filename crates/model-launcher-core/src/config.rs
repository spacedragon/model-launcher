use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppError, ModelRecord};

const CURRENT_VERSION: u32 = 1;
const CONFIG_FILE: &str = "config.json";
const BACKUP_FILE: &str = "config.json.backup";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherConfig {
    #[serde(default)]
    pub models: Vec<ModelRecord>,
    #[serde(default)]
    pub auth_token_hashes: Vec<String>,
    #[serde(default)]
    pub engine_distribution: Option<String>,
    #[serde(default)]
    pub engine_executable: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigDiagnosticKind {
    Corrupt,
    UnsupportedVersion { version: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub kind: ConfigDiagnosticKind,
    pub quarantine_path: PathBuf,
}

impl ConfigDiagnostic {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self.kind {
            ConfigDiagnosticKind::Corrupt => "config_corrupt",
            ConfigDiagnosticKind::UnsupportedVersion { .. } => "config_unsupported_version",
        }
    }
}

impl std::fmt::Display for ConfigDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            ConfigDiagnosticKind::Corrupt => formatter.write_str("configuration was corrupt"),
            ConfigDiagnosticKind::UnsupportedVersion { version } => {
                write!(formatter, "configuration version {version} is unsupported")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigLoadOutcome {
    pub config: LauncherConfig,
    pub diagnostic: Option<ConfigDiagnostic>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigIoStage {
    WriteMainTemp,
    SyncMainTemp,
    CopyBackupTemp,
    SyncBackupTemp,
    ReplaceBackup,
    ReplaceMain,
    ReadDirectoryEntry,
}

pub trait FileReplacer: Send + Sync {
    fn replace(&self, source: &Path, destination: &Path) -> io::Result<()>;

    fn before_stage(&self, _stage: ConfigIoStage) -> io::Result<()> {
        Ok(())
    }
}

struct SystemFileReplacer;

impl FileReplacer for SystemFileReplacer {
    fn replace(&self, source: &Path, destination: &Path) -> io::Result<()> {
        replace_file(source, destination)
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    version: u32,
    config: LauncherConfig,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

#[derive(Deserialize)]
struct VersionZero {
    version: u32,
    #[serde(default)]
    models: Vec<ModelRecord>,
}

/// A configuration store with process-local writer serialization.
///
/// Clones share a transaction mutex. Cross-process writers are not supported.
#[derive(Clone)]
pub struct ConfigStore {
    directory: PathBuf,
    replacer: Arc<dyn FileReplacer>,
    transaction: Arc<Mutex<()>>,
}

impl ConfigStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            replacer: Arc::new(SystemFileReplacer),
            transaction: Arc::new(Mutex::new(())),
        }
    }

    pub fn with_replacer(directory: impl Into<PathBuf>, replacer: Arc<dyn FileReplacer>) -> Self {
        Self {
            directory: directory.into(),
            replacer,
            transaction: Arc::new(Mutex::new(())),
        }
    }

    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.directory.join(CONFIG_FILE)
    }

    #[must_use]
    pub fn backup_path(&self) -> PathBuf {
        self.directory.join(BACKUP_FILE)
    }

    pub fn load(&self) -> Result<LauncherConfig, AppError> {
        self.load_with_diagnostic().map(|outcome| outcome.config)
    }

    pub fn load_with_diagnostic(&self) -> Result<ConfigLoadOutcome, AppError> {
        let _transaction = self.lock_transaction()?;
        self.load_with_diagnostic_locked()
    }

    fn load_with_diagnostic_locked(&self) -> Result<ConfigLoadOutcome, AppError> {
        let path = self.config_path();
        if !path.exists() {
            return Ok(ConfigLoadOutcome {
                config: LauncherConfig::default(),
                diagnostic: None,
            });
        }

        match Self::decode_file(&path) {
            Ok((config, migrated)) => {
                if migrated {
                    self.save_locked(&config)?;
                }
                Ok(ConfigLoadOutcome {
                    config,
                    diagnostic: None,
                })
            }
            Err(AppError::ConfigFormat(_)) => {
                let kind = classify_format(&path)?;
                let quarantine_path = self.quarantine(&path)?;
                Ok(ConfigLoadOutcome {
                    config: LauncherConfig::default(),
                    diagnostic: Some(ConfigDiagnostic {
                        kind,
                        quarantine_path,
                    }),
                })
            }
            Err(error) => Err(error),
        }
    }

    pub fn load_file(path: impl AsRef<Path>) -> Result<LauncherConfig, AppError> {
        Self::decode_file(path.as_ref()).map(|(config, _)| config)
    }

    pub fn save(&self, config: &LauncherConfig) -> Result<(), AppError> {
        let _transaction = self.lock_transaction()?;
        self.save_locked(config)
    }

    /// Atomically loads the latest configuration, applies `mutate`, and persists the result.
    /// The mutator and save share the process-local transaction lock.
    pub fn update(
        &self,
        mutate: impl FnOnce(&mut LauncherConfig) -> Result<(), AppError>,
    ) -> Result<LauncherConfig, AppError> {
        let _transaction = self.lock_transaction()?;
        let mut config = self.load_with_diagnostic_locked()?.config;
        mutate(&mut config)?;
        self.save_locked(&config)?;
        Ok(config)
    }

    fn save_locked(&self, config: &LauncherConfig) -> Result<(), AppError> {
        fs::create_dir_all(&self.directory).map_err(config_io)?;
        let main = self.config_path();
        if main.exists() {
            match Self::decode_file(&main) {
                Ok(_) => {}
                Err(error @ AppError::ConfigFormat(_)) => {
                    self.quarantine(&main)?;
                    return Err(error);
                }
                Err(error) => return Err(error),
            }
        }
        let bytes = serde_json::to_vec_pretty(&Envelope {
            version: CURRENT_VERSION,
            config: config.clone(),
        })
        .map_err(config_format)?;
        let mut temporary = TempGuard::create(&self.directory, "main").map_err(config_io)?;
        self.before_stage(ConfigIoStage::WriteMainTemp)?;
        temporary.file_mut().write_all(&bytes).map_err(config_io)?;
        self.before_stage(ConfigIoStage::SyncMainTemp)?;
        temporary.file_mut().sync_all().map_err(config_io)?;
        temporary.close();

        if main.exists() {
            let mut backup_temporary =
                TempGuard::create(&self.directory, "backup").map_err(config_io)?;
            let mut source = File::open(&main).map_err(config_io)?;
            self.before_stage(ConfigIoStage::CopyBackupTemp)?;
            io::copy(&mut source, backup_temporary.file_mut()).map_err(config_io)?;
            self.before_stage(ConfigIoStage::SyncBackupTemp)?;
            backup_temporary.file_mut().sync_all().map_err(config_io)?;
            backup_temporary.close();
            self.before_stage(ConfigIoStage::ReplaceBackup)?;
            self.replacer
                .replace(backup_temporary.path(), &self.backup_path())
                .map_err(config_io)?;
            backup_temporary.disarm();
        }
        self.before_stage(ConfigIoStage::ReplaceMain)?;
        self.replacer
            .replace(temporary.path(), &main)
            .map_err(config_io)?;
        temporary.disarm();
        sync_directory(&self.directory).map_err(config_io)?;
        Ok(())
    }

    pub fn quarantined_files(&self) -> Result<Vec<PathBuf>, AppError> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let entries = fs::read_dir(&self.directory)
            .map_err(config_io)?
            .map(|entry| {
                let entry = entry.map_err(config_io)?;
                self.before_stage(ConfigIoStage::ReadDirectoryEntry)?;
                Ok(entry.path())
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let mut paths = entries
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("config.json.quarantine-"))
            })
            .collect::<Vec<_>>();
        paths.sort();
        Ok(paths)
    }

    fn decode_file(path: &Path) -> Result<(LauncherConfig, bool), AppError> {
        let bytes = fs::read(path).map_err(config_io)?;
        let probe: VersionProbe = serde_json::from_slice(&bytes).map_err(config_format)?;
        match probe.version {
            CURRENT_VERSION => {
                let envelope: Envelope = serde_json::from_slice(&bytes).map_err(config_format)?;
                Ok((envelope.config, false))
            }
            0 => {
                let old: VersionZero = serde_json::from_slice(&bytes).map_err(config_format)?;
                debug_assert_eq!(old.version, 0);
                Ok((
                    LauncherConfig {
                        models: old.models,
                        ..LauncherConfig::default()
                    },
                    true,
                ))
            }
            version => Err(config_format(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported configuration version {version}"),
            ))),
        }
    }

    fn quarantine(&self, path: &Path) -> Result<PathBuf, AppError> {
        let destination = self
            .directory
            .join(format!("config.json.quarantine-{}", Uuid::new_v4()));
        fs::rename(path, &destination).map_err(config_io)?;
        sync_directory(&self.directory).map_err(config_io)?;
        Ok(destination)
    }

    fn before_stage(&self, stage: ConfigIoStage) -> Result<(), AppError> {
        self.replacer.before_stage(stage).map_err(config_io)
    }

    fn lock_transaction(&self) -> Result<MutexGuard<'_, ()>, AppError> {
        self.transaction.lock().map_err(|_| {
            config_io(io::Error::other(
                "configuration transaction lock was poisoned",
            ))
        })
    }
}

struct TempGuard {
    path: Option<PathBuf>,
    file: Option<File>,
}

impl TempGuard {
    fn create(directory: &Path, purpose: &str) -> io::Result<Self> {
        for _ in 0..16 {
            let path = directory.join(format!(".model-launcher-{purpose}.tmp-{}", Uuid::new_v4()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path: Some(path),
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique configuration temporary file",
        ))
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("armed temporary path")
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("open temporary file")
    }

    fn close(&mut self) {
        self.file.take();
    }

    fn disarm(mut self) {
        self.path.take();
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        self.file.take();
        if let Some(path) = self.path.take() {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {}
            }
        }
    }
}

fn config_io(error: impl std::error::Error + Send + Sync + 'static) -> AppError {
    AppError::ConfigIo(Box::new(error))
}

fn config_format(error: impl std::error::Error + Send + Sync + 'static) -> AppError {
    AppError::ConfigFormat(Box::new(error))
}

fn classify_format(path: &Path) -> Result<ConfigDiagnosticKind, AppError> {
    let bytes = fs::read(path).map_err(config_io)?;
    match serde_json::from_slice::<VersionProbe>(&bytes) {
        Ok(VersionProbe { version }) if version > CURRENT_VERSION => {
            Ok(ConfigDiagnosticKind::UnsupportedVersion { version })
        }
        _ => Ok(ConfigDiagnosticKind::Corrupt),
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ptr;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_WRITE_THROUGH, MoveFileExW, REPLACEFILE_WRITE_THROUGH, ReplaceFileW,
    };

    let destination_exists = destination.exists();
    let source = wide_path(source)?;
    let destination = wide_path(destination)?;
    let succeeded = unsafe {
        if destination_exists {
            ReplaceFileW(
                destination.as_ptr(),
                source.as_ptr(),
                ptr::null(),
                REPLACEFILE_WRITE_THROUGH,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        } else {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        }
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows path contains a NUL code unit",
        ));
    }
    encoded.push(0);
    Ok(encoded)
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::{ffi::OsString, io, os::windows::ffi::OsStringExt, path::Path};

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_WRITE_THROUGH, REPLACEFILE_WRITE_THROUGH,
    };

    use super::wide_path;

    #[test]
    fn replacement_flags_request_durable_windows_operations() {
        assert_ne!(MOVEFILE_WRITE_THROUGH, 0);
        assert_ne!(REPLACEFILE_WRITE_THROUGH, 0);
    }

    #[test]
    fn wide_paths_are_terminated_and_reject_interior_nuls() {
        assert_eq!(
            wide_path(Path::new("config.json")).unwrap().last(),
            Some(&0)
        );
        let invalid = OsString::from_wide(&[b'a' as u16, 0, b'b' as u16]);
        assert_eq!(
            wide_path(Path::new(&invalid)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}
