use std::{
    error::Error as _,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use model_launcher_core::{
    CatalogIdentity, ConfigDiagnosticKind, ConfigIoStage, ConfigStore, FileReplacer,
    LauncherConfig, ModelId, ModelKey, ModelRecord, ModelState,
};
use uuid::Uuid;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("model-launcher-config-{}", Uuid::new_v4()));
        fs::create_dir(&path).expect("create test directory");
        Self(path)
    }
}

struct FailNthReplace {
    calls: AtomicUsize,
    fail_on: usize,
}

impl FileReplacer for FailNthReplace {
    fn replace(&self, source: &Path, destination: &Path) -> io::Result<()> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.fail_on {
            Err(io::Error::other("injected replacement failure"))
        } else {
            fs::rename(source, destination)
        }
    }
}

struct FailStage(ConfigIoStage);

impl FileReplacer for FailStage {
    fn replace(&self, source: &Path, destination: &Path) -> io::Result<()> {
        fs::rename(source, destination)
    }

    fn before_stage(&self, stage: ConfigIoStage) -> io::Result<()> {
        if stage == self.0 {
            Err(io::Error::other(format!("injected {stage:?} failure")))
        } else {
            Ok(())
        }
    }
}

struct PanicStage;

impl FileReplacer for PanicStage {
    fn replace(&self, source: &Path, destination: &Path) -> io::Result<()> {
        fs::rename(source, destination)
    }

    fn before_stage(&self, stage: ConfigIoStage) -> io::Result<()> {
        if stage == ConfigIoStage::WriteMainTemp {
            panic!("injected transaction panic");
        }
        Ok(())
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn model(state: ModelState) -> ModelRecord {
    ModelRecord {
        id: ModelId::from_uuid(
            Uuid::parse_str("f91eaee1-7914-4eb9-a633-663f691fab41").expect("fixture UUID"),
        ),
        key: ModelKey::parse("Qwen/qwen3-8b-q4").expect("fixture key"),
        display_name: "Qwen 3 8B".into(),
        path: PathBuf::from("models/qwen.gguf"),
        file_identity: CatalogIdentity::Unavailable,
        size_bytes: 4_294_967_296,
        state,
        launch_profile: Default::default(),
    }
}

#[test]
fn missing_config_loads_as_default() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);

    assert_eq!(
        store.load().expect("load default"),
        LauncherConfig::default()
    );
    assert!(!store.config_path().exists());
}

#[test]
fn configuration_round_trips() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    let config = LauncherConfig {
        models: vec![model(ModelState::Available)],
        ..LauncherConfig::default()
    };

    store.save(&config).expect("save config");

    assert_eq!(store.load().expect("reload config"), config);
}

#[test]
fn update_loads_latest_mutates_and_saves_under_one_transaction() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    store
        .save(&LauncherConfig {
            models: vec![model(ModelState::Available)],
            ..LauncherConfig::default()
        })
        .unwrap();

    let updated = store
        .update(|config| {
            config.models[0].key = ModelKey::parse("user-key")?;
            Ok(())
        })
        .unwrap();

    assert_eq!(updated.models[0].key.as_str(), "user-key");
    assert_eq!(store.load().unwrap(), updated);
}

#[test]
fn update_propagates_mutator_error_without_saving() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    let initial = LauncherConfig {
        models: vec![model(ModelState::Available)],
        ..LauncherConfig::default()
    };
    store.save(&initial).unwrap();

    let error = store
        .update(|_| {
            Err(model_launcher_core::AppError::EngineProcess(Box::new(
                io::Error::other("mutator failed"),
            )))
        })
        .unwrap_err();

    assert_eq!(error.source().unwrap().to_string(), "mutator failed");
    assert_eq!(store.load().unwrap(), initial);
}

#[test]
fn save_atomically_replaces_the_main_file() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    store
        .save(&LauncherConfig::default())
        .expect("initial save");
    let updated = LauncherConfig {
        models: vec![model(ModelState::Available)],
        ..LauncherConfig::default()
    };

    store.save(&updated).expect("replacement save");

    assert_eq!(store.load().expect("load replacement"), updated);
    assert!(!dir.0.join("config.json.tmp").exists());
}

#[test]
fn replacement_failure_preserves_main_and_cleans_temporary_file() {
    let dir = TestDir::new();
    let replacer = Arc::new(FailNthReplace {
        calls: AtomicUsize::new(0),
        fail_on: 3,
    });
    let store = ConfigStore::with_replacer(&dir.0, replacer);
    let previous = LauncherConfig {
        models: vec![model(ModelState::Missing)],
        ..LauncherConfig::default()
    };
    store.save(&previous).expect("initial save");
    let original_bytes = fs::read(store.config_path()).expect("read original config");

    let error = store
        .save(&LauncherConfig::default())
        .expect_err("replacement must fail");

    assert_eq!(error.code(), "config_io");
    assert_eq!(
        error.source().expect("replacement source").to_string(),
        "injected replacement failure"
    );
    assert_eq!(fs::read(store.config_path()).unwrap(), original_bytes);
    assert_eq!(store.load().unwrap(), previous);
    assert!(!dir.0.join("config.json.tmp").exists());
}

fn temporary_artifacts(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".model-launcher-") && name.contains(".tmp-"))
        })
        .collect()
}

#[test]
fn write_sync_copy_and_replace_failures_clean_unique_temps_and_preserve_main() {
    for stage in [
        ConfigIoStage::WriteMainTemp,
        ConfigIoStage::SyncMainTemp,
        ConfigIoStage::CopyBackupTemp,
        ConfigIoStage::SyncBackupTemp,
        ConfigIoStage::ReplaceBackup,
        ConfigIoStage::ReplaceMain,
    ] {
        let dir = TestDir::new();
        let initial_store = ConfigStore::new(&dir.0);
        let previous = LauncherConfig {
            models: vec![model(ModelState::Missing)],
            ..LauncherConfig::default()
        };
        initial_store.save(&previous).unwrap();
        let original = fs::read(initial_store.config_path()).unwrap();
        let store = ConfigStore::with_replacer(&dir.0, Arc::new(FailStage(stage)));

        let error = store
            .save(&LauncherConfig::default())
            .expect_err("injected failure");

        assert_eq!(error.code(), "config_io", "stage {stage:?}");
        assert_eq!(
            fs::read(store.config_path()).unwrap(),
            original,
            "stage {stage:?}"
        );
        assert!(temporary_artifacts(&dir.0).is_empty(), "stage {stage:?}");
    }
}

#[cfg(unix)]
#[test]
fn preplanted_fixed_temp_symlinks_are_never_followed() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new();
    let external = dir
        .0
        .parent()
        .unwrap()
        .join(format!("external-{}", Uuid::new_v4()));
    fs::write(&external, b"outside").unwrap();
    symlink(&external, dir.0.join("config.json.tmp")).unwrap();
    symlink(&external, dir.0.join("config.json.backup.tmp")).unwrap();

    ConfigStore::new(&dir.0)
        .save(&LauncherConfig::default())
        .unwrap();

    assert_eq!(fs::read(&external).unwrap(), b"outside");
    assert!(temporary_artifacts(&dir.0).is_empty());
    fs::remove_file(external).unwrap();
}

#[test]
fn cloned_store_serializes_concurrent_saves_into_complete_versions() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    store.save(&LauncherConfig::default()).unwrap();
    let configs = (0..8)
        .map(|index| {
            let mut record = model(ModelState::Available);
            record.display_name = format!("model-{index}");
            LauncherConfig {
                models: vec![record],
                ..LauncherConfig::default()
            }
        })
        .collect::<Vec<_>>();
    let barrier = Arc::new(std::sync::Barrier::new(configs.len()));
    let handles = configs
        .iter()
        .cloned()
        .map(|config| {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store.save(&config)
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    let main = store.load().unwrap();
    let backup = ConfigStore::load_file(store.backup_path()).unwrap();
    assert!(configs.contains(&main));
    assert!(configs.contains(&backup));
    assert_ne!(main, backup);
    assert!(temporary_artifacts(&dir.0).is_empty());
}

#[test]
fn poisoned_transaction_lock_returns_config_io_instead_of_panicking() {
    let dir = TestDir::new();
    let store = ConfigStore::with_replacer(&dir.0, Arc::new(PanicStage));
    let panicking_store = store.clone();
    assert!(
        std::thread::spawn(move || panicking_store.save(&LauncherConfig::default()))
            .join()
            .is_err()
    );

    let error = store
        .save(&LauncherConfig::default())
        .expect_err("poison maps to application error");

    assert_eq!(error.code(), "config_io");
    assert_eq!(
        error.source().unwrap().to_string(),
        "configuration transaction lock was poisoned"
    );
    let update_error = store
        .update(|_| Ok(()))
        .expect_err("poisoned update maps to application error");
    assert_eq!(update_error.code(), "config_io");
    assert_eq!(
        update_error.source().unwrap().to_string(),
        "configuration transaction lock was poisoned"
    );
    assert!(temporary_artifacts(&dir.0).is_empty());
}

#[test]
fn replacement_retains_the_last_valid_backup() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    let previous = LauncherConfig {
        models: vec![model(ModelState::Missing)],
        ..LauncherConfig::default()
    };
    store.save(&previous).expect("initial save");

    store
        .save(&LauncherConfig::default())
        .expect("replacement save");

    assert_eq!(
        ConfigStore::load_file(store.backup_path()).expect("load backup"),
        previous
    );
}

#[test]
fn corrupt_file_is_quarantined_without_being_overwritten() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    let corrupt = b"{ definitely not json";
    fs::write(store.config_path(), corrupt).expect("write corrupt fixture");

    assert_eq!(
        store.load().expect("quarantine corrupt file"),
        LauncherConfig::default()
    );

    let quarantined = store.quarantined_files().expect("list quarantined files");
    assert_eq!(quarantined.len(), 1);
    assert_eq!(fs::read(&quarantined[0]).expect("read quarantine"), corrupt);
    assert!(!store.config_path().exists());
}

#[test]
fn corrupt_load_exposes_a_typed_quarantine_diagnostic() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    fs::write(store.config_path(), b"secret invalid bytes").unwrap();

    let outcome = store
        .load_with_diagnostic()
        .expect("recover corrupt config");

    assert_eq!(outcome.config, LauncherConfig::default());
    let diagnostic = outcome.diagnostic.expect("visible diagnostic");
    assert_eq!(diagnostic.kind, ConfigDiagnosticKind::Corrupt);
    assert_eq!(diagnostic.code(), "config_corrupt");
    assert!(diagnostic.quarantine_path.exists());
    assert!(!diagnostic.to_string().contains("secret invalid bytes"));
}

#[test]
fn save_does_not_overwrite_an_unloaded_corrupt_file() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    let corrupt = b"not a configuration";
    fs::write(store.config_path(), corrupt).expect("write corrupt fixture");

    let error = store
        .save(&LauncherConfig::default())
        .expect_err("refuse to overwrite corrupt config");

    assert_eq!(error.code(), "config_format");
    assert!(!store.config_path().exists());
    let quarantined = store.quarantined_files().unwrap();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(fs::read(&quarantined[0]).unwrap(), corrupt);
}

#[test]
fn model_uuid_and_key_survive_persistence() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    let expected = model(ModelState::Available);
    store
        .save(&LauncherConfig {
            models: vec![expected.clone()],
            ..LauncherConfig::default()
        })
        .expect("save model");

    let actual = store.load().expect("load model").models.remove(0);

    assert_eq!(actual.id, expected.id);
    assert_eq!(actual.key, expected.key);
}

#[test]
fn missing_models_are_not_discarded() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    let missing = model(ModelState::Missing);

    store
        .save(&LauncherConfig {
            models: vec![missing.clone()],
            ..LauncherConfig::default()
        })
        .expect("save missing model");

    assert_eq!(
        store.load().expect("load missing model").models,
        vec![missing]
    );
}

#[test]
fn migrates_a_version_zero_fixture() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    let fixture = serde_json::json!({ "version": 0, "models": [model(ModelState::Missing)] });
    fs::write(
        store.config_path(),
        serde_json::to_vec_pretty(&fixture).unwrap(),
    )
    .unwrap();

    let migrated = store.load().expect("migrate v0 fixture");

    assert_eq!(migrated.models, vec![model(ModelState::Missing)]);
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(store.config_path()).unwrap()).unwrap();
    assert_eq!(persisted["version"], 1);
}

#[test]
fn unsupported_version_is_quarantined() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    let fixture = br#"{"version":99,"config":{"models":[]}}"#;
    fs::write(store.config_path(), fixture).unwrap();

    assert_eq!(
        store.load().expect("quarantine future version"),
        LauncherConfig::default()
    );

    let quarantined = store.quarantined_files().unwrap();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(fs::read(&quarantined[0]).unwrap(), fixture);
}

#[test]
fn unsupported_load_diagnostic_includes_the_version() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    fs::write(
        store.config_path(),
        br#"{"version":99,"config":{"models":[]}}"#,
    )
    .unwrap();

    let outcome = store.load_with_diagnostic().expect("recover future config");

    let diagnostic = outcome.diagnostic.expect("visible diagnostic");
    assert_eq!(
        diagnostic.kind,
        ConfigDiagnosticKind::UnsupportedVersion { version: 99 }
    );
    assert_eq!(diagnostic.code(), "config_unsupported_version");
    assert!(diagnostic.quarantine_path.exists());
}

#[test]
fn quarantine_directory_entry_errors_are_propagated() {
    let dir = TestDir::new();
    fs::write(dir.0.join("config.json.quarantine-fixture"), b"fixture").unwrap();
    let store = ConfigStore::with_replacer(
        &dir.0,
        Arc::new(FailStage(ConfigIoStage::ReadDirectoryEntry)),
    );

    let error = store
        .quarantined_files()
        .expect_err("entry failure must propagate");

    assert_eq!(error.code(), "config_io");
}
