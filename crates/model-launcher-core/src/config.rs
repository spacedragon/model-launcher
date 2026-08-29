use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
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

#[derive(Clone, Debug)]
pub struct ConfigStore {
    directory: PathBuf,
}

impl ConfigStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
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
        let path = self.config_path();
        if !path.exists() {
            return Ok(LauncherConfig::default());
        }

        match Self::decode_file(&path) {
            Ok((config, migrated)) => {
                if migrated {
                    self.save(&config)?;
                }
                Ok(config)
            }
            Err(AppError::ConfigFormat(_)) => {
                self.quarantine(&path)?;
                Ok(LauncherConfig::default())
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
            replace_file(&backup_temporary, &self.backup_path()).map_err(config_io)?;
        }
        replace_file(&temporary, &main).map_err(config_io)?;
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

    fn quarantine(&self, path: &Path) -> Result<(), AppError> {
        let destination = self
            .directory
            .join(format!("config.json.quarantine-{}", Uuid::new_v4()));
        fs::rename(path, destination).map_err(config_io)?;
        sync_directory(&self.directory).map_err(config_io)
    }
}

fn config_io(error: impl std::error::Error + Send + Sync + 'static) -> AppError {
    AppError::ConfigIo(Box::new(error))
}

fn config_format(error: impl std::error::Error + Send + Sync + 'static) -> AppError {
    AppError::ConfigFormat(Box::new(error))
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(source, destination)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}
