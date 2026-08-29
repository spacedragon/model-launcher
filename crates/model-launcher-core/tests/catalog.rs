use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use gguf_rs_lib::{builder::GGUFBuilder, format::MetadataValue};
use model_launcher_core::{
    CatalogDebouncer, CatalogDiagnosticKind, CatalogIdentity, LauncherConfig, ModelKey, ModelState,
    ReconcileOptions, reconcile_catalog, scan,
};
use uuid::Uuid;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("model-launcher-catalog-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tiny_gguf(name: &str) -> Vec<u8> {
    GGUFBuilder::new()
        .add_metadata("general.name", MetadataValue::String(name.into()))
        .add_metadata(
            "general.architecture",
            MetadataValue::String("llama".into()),
        )
        .build_to_bytes()
        .unwrap()
        .0
}

#[test]
fn scan_recurses_and_matches_extension_case_insensitively() {
    let root = TestDir::new();
    root.file("nested/Alpha.GGuF", &tiny_gguf("Alpha"));
    root.file("ignored.bin", b"not a model");

    let result = scan(root.path());

    assert_eq!(result.models.len(), 1);
    assert_eq!(result.models[0].display_name, "Alpha");
    assert!(result.diagnostics.is_empty());
}

#[test]
fn scan_groups_only_complete_recognized_shard_sets() {
    let root = TestDir::new();
    let one = root.file("orca-00001-of-00002.gguf", &tiny_gguf("Orca"));
    let two = root.file("orca-00002-of-00002.gguf", b"second shard");
    root.file("ordinary-hyphen-name.gguf", &tiny_gguf("Ordinary"));
    root.file("broken-00001-of-00003.gguf", &tiny_gguf("Broken"));

    let result = scan(root.path());
    let orca = result
        .models
        .iter()
        .find(|model| model.display_name == "Orca")
        .unwrap();

    assert_eq!(orca.path, one);
    assert_eq!(
        orca.size_bytes,
        fs::metadata(one).unwrap().len() + fs::metadata(two).unwrap().len()
    );
    assert_eq!(
        result.models.len(),
        3,
        "incomplete shards stay independent and unrelated names never group"
    );
}

#[test]
fn malformed_metadata_falls_back_to_filename_with_visible_diagnostic() {
    let root = TestDir::new();
    root.file("read-me.gguf", include_bytes!("fixtures/read-me.gguf"));

    let result = scan(root.path());

    assert_eq!(result.models[0].display_name, "read-me");
    assert_eq!(result.diagnostics[0].kind, CatalogDiagnosticKind::Metadata);
    assert_eq!(result.diagnostics[0].path, result.models[0].path);
}

#[test]
fn reconciliation_generates_unique_url_safe_keys_for_duplicates() {
    let root = TestDir::new();
    root.file("a/model.gguf", &tiny_gguf("Same name!"));
    root.file("b/model.gguf", &tiny_gguf("Same name!"));

    let output = reconcile_catalog(
        &LauncherConfig::default(),
        scan(root.path()),
        ReconcileOptions::default(),
    );
    let keys = output
        .config
        .models
        .iter()
        .map(|record| record.key.as_str())
        .collect::<Vec<_>>();

    assert_eq!(keys, vec!["same-name", "same-name-2"]);
    assert!(keys.iter().all(|key| ModelKey::parse(*key).is_ok()));
}

#[test]
fn reconciliation_retains_user_key_and_reconnects_a_moved_file() {
    let root = TestDir::new();
    let original = root.file("old/model.gguf", &tiny_gguf("Movable"));
    let first = reconcile_catalog(
        &LauncherConfig::default(),
        scan(root.path()),
        ReconcileOptions::default(),
    );
    let mut saved = first.config;
    let id = saved.models[0].id;
    saved.models[0].key = ModelKey::parse("my-custom-key").unwrap();
    let moved = root.path().join("new/model.gguf");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(original, &moved).unwrap();

    let second = reconcile_catalog(&saved, scan(root.path()), ReconcileOptions::default());

    assert_eq!(second.config.models[0].id, id);
    assert_eq!(second.config.models[0].key.as_str(), "my-custom-key");
    assert_eq!(second.config.models[0].path, moved);
}

#[test]
fn missing_records_are_preserved_until_explicitly_removed() {
    let root = TestDir::new();
    root.file("gone.gguf", &tiny_gguf("Gone"));
    let first = reconcile_catalog(
        &LauncherConfig::default(),
        scan(root.path()),
        ReconcileOptions::default(),
    );
    fs::remove_file(root.path().join("gone.gguf")).unwrap();

    let missing = reconcile_catalog(
        &first.config,
        scan(root.path()),
        ReconcileOptions::default(),
    );
    assert_eq!(missing.config.models[0].state, ModelState::Missing);

    let removed = reconcile_catalog(
        &missing.config,
        scan(root.path()),
        ReconcileOptions {
            remove_missing: vec![missing.config.models[0].id],
        },
    );
    assert!(removed.config.models.is_empty());
}

#[test]
fn file_identity_is_bounded_and_best_effort() {
    let root = TestDir::new();
    let path = root.file("model.gguf", &tiny_gguf("Identity"));
    let model = &scan(root.path()).models[0];

    assert_ne!(model.identity, CatalogIdentity::Unavailable);
    assert_eq!(model.identity, CatalogIdentity::for_path(&path));
}

#[tokio::test(start_paused = true)]
async fn debounce_coalesces_changes_deterministically() {
    let mut debounce = CatalogDebouncer::new(Duration::from_millis(250));
    debounce.changed(PathBuf::from("one.gguf"));
    tokio::time::advance(Duration::from_millis(200)).await;
    debounce.changed(PathBuf::from("two.gguf"));

    let pending = debounce.next().await;

    assert_eq!(
        pending,
        vec![PathBuf::from("one.gguf"), PathBuf::from("two.gguf")]
    );
}
