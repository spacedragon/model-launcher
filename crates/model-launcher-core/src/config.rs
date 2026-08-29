use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppError, ModelRecord};

const CURRENT_VERSION: u32 = 1;
const CONFIG_FILE: &str = "config.json";
const BACKUP_FILE: &str = "config.json.backup";
const TEMP_FILE: &str = "config.json.tmp";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherConfig {
    #[serde(default)]
    pub models: Vec<ModelRecord>,
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

pub trait FileReplacer: Send + Sync {
    fn replace(&self, source: &Path, destination: &Path) -> io::Result<()>;
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

#[derive(Clone)]
pub struct ConfigStore {
    directory: PathBuf,
    replacer: Arc<dyn FileReplacer>,
}

impl ConfigStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            replacer: Arc::new(SystemFileReplacer),
        }
    }

    pub fn with_replacer(directory: impl Into<PathBuf>, replacer: Arc<dyn FileReplacer>) -> Self {
        Self {
            directory: directory.into(),
            replacer,
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
                    self.save(&config)?;
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
        let temporary = self.directory.join(TEMP_FILE);
        let mut file = File::create(&temporary).map_err(config_io)?;
        file.write_all(&bytes).map_err(config_io)?;
        file.sync_all().map_err(config_io)?;
        drop(file);

        if main.exists() {
            let backup_temporary = self.directory.join("config.json.backup.tmp");
            fs::copy(&main, &backup_temporary).map_err(config_io)?;
            File::open(&backup_temporary)
                .and_then(|file| file.sync_all())
                .map_err(config_io)?;
            if let Err(error) = self
                .replacer
                .replace(&backup_temporary, &self.backup_path())
            {
                cleanup_files([&temporary, &backup_temporary]);
                return Err(config_io(error));
            }
        }
        if let Err(error) = self.replacer.replace(&temporary, &main) {
            cleanup_files([&temporary, &self.directory.join("config.json.backup.tmp")]);
            return Err(config_io(error));
        }
        sync_directory(&self.directory).map_err(config_io)?;
        Ok(())
    }

    pub fn quarantined_files(&self) -> Result<Vec<PathBuf>, AppError> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let mut paths = fs::read_dir(&self.directory)
            .map_err(config_io)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
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
                Ok((LauncherConfig { models: old.models }, true))
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

fn cleanup_files<const N: usize>(paths: [&Path; N]) {
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
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
