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
    ConfigDiagnosticKind, ConfigStore, FileReplacer, LauncherConfig, ModelId, ModelKey,
    ModelRecord, ModelState,
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
    };

    store.save(&config).expect("save config");

    assert_eq!(store.load().expect("reload config"), config);
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

#[test]
fn replacement_retains_the_last_valid_backup() {
    let dir = TestDir::new();
    let store = ConfigStore::new(&dir.0);
    let previous = LauncherConfig {
        models: vec![model(ModelState::Missing)],
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
