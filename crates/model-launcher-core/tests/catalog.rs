use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use gguf_rs_lib::{builder::GGUFBuilder, format::MetadataValue};
use model_launcher_core::{
    CatalogDebouncer, CatalogDiagnosticKind, CatalogIdentity, CatalogService, CatalogWatchEvent,
    ConfigStore, LauncherConfig, ModelKey, ModelState, ReconcileOptions, catalog_watch_channel,
    reconcile_catalog, scan,
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

fn metadata_gguf(name: MetadataValue) -> Vec<u8> {
    GGUFBuilder::new()
        .add_metadata("general.name", name)
        .add_metadata(
            "general.architecture",
            MetadataValue::String("llama".into()),
        )
        .add_metadata("general.parameter_count", MetadataValue::U64(7_000_000_000))
        .add_metadata(
            "general.quantization",
            MetadataValue::String("Q4_K_M".into()),
        )
        .add_metadata("llama.context_length", MetadataValue::U32(8192))
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
    assert!(result.complete);
}

#[test]
fn valid_gguf_extracts_metadata_and_missing_name_uses_filename() {
    let root = TestDir::new();
    root.file(
        "metadata.gguf",
        &metadata_gguf(MetadataValue::String("Seven".into())),
    );
    root.file(
        "filename-only.gguf",
        &GGUFBuilder::new()
            .add_metadata(
                "general.architecture",
                MetadataValue::String("llama".into()),
            )
            .build_to_bytes()
            .unwrap()
            .0,
    );

    let result = scan(root.path());
    let metadata = result
        .models
        .iter()
        .find(|model| model.display_name == "Seven")
        .unwrap();
    assert_eq!(metadata.metadata.architecture.as_deref(), Some("llama"));
    assert_eq!(metadata.metadata.parameter_count, Some(7_000_000_000));
    assert_eq!(metadata.metadata.quantization.as_deref(), Some("Q4_K_M"));
    assert_eq!(metadata.metadata.context_length, Some(8192));
    assert!(
        result
            .models
            .iter()
            .any(|model| model.display_name == "filename-only")
    );
    assert!(result.complete);
}

#[test]
fn wrong_type_name_falls_back_with_metadata_diagnostic() {
    let root = TestDir::new();
    root.file("typed.gguf", &metadata_gguf(MetadataValue::U32(42)));
    let result = scan(root.path());
    assert_eq!(result.models[0].display_name, "typed");
    assert!(
        result.complete,
        "a per-file metadata problem does not invalidate traversal"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item.kind == CatalogDiagnosticKind::Metadata)
    );
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
fn mixed_case_shard_extensions_group_using_actual_paths() {
    let root = TestDir::new();
    let first = root.file("Mix-00001-of-00002.GGUF", &tiny_gguf("Mixed"));
    let second = root.file("Mix-00002-of-00002.gGuF", b"payload");
    let result = scan(root.path());
    assert_eq!(result.models.len(), 1);
    assert_eq!(result.models[0].path, first);
    assert_eq!(
        result.models[0].size_bytes,
        fs::metadata(first).unwrap().len() + fs::metadata(second).unwrap().len()
    );
}

#[test]
fn invalid_roots_are_incomplete_and_visible() {
    let missing = TestDir::new().path().join("missing");
    let missing_result = scan(&missing);
    assert!(!missing_result.complete);
    assert!(!missing_result.diagnostics.is_empty());

    let root = TestDir::new();
    let file = root.file("not-a-directory", b"x");
    let file_result = scan(&file);
    assert!(!file_result.complete);
    assert!(!file_result.diagnostics.is_empty());
}

#[cfg(unix)]
#[test]
fn unreadable_root_is_incomplete_and_visible() {
    use std::os::unix::fs::PermissionsExt;
    let root = TestDir::new();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o000)).unwrap();
    let result = scan(root.path());
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert!(!result.complete);
        assert!(!result.diagnostics.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn root_and_child_symlinks_are_never_followed() {
    use std::os::unix::fs::symlink;
    let outside = TestDir::new();
    outside.file("outside.gguf", &tiny_gguf("Outside"));
    let root = TestDir::new();
    symlink(outside.path(), root.path().join("child-link")).unwrap();
    let child = scan(root.path());
    assert!(child.complete);
    assert!(child.models.is_empty());

    let holder = TestDir::new();
    let root_link = holder.path().join("root-link");
    symlink(outside.path(), &root_link).unwrap();
    let linked = scan(&root_link);
    assert!(!linked.complete);
    assert!(linked.models.is_empty());
    assert!(!linked.diagnostics.is_empty());
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
fn incomplete_scan_preserves_existing_availability() {
    let root = TestDir::new();
    root.file("model.gguf", &tiny_gguf("Existing"));
    let first = reconcile_catalog(
        &LauncherConfig::default(),
        scan(root.path()),
        Default::default(),
    );
    let failed = scan(&root.path().join("missing"));
    let output = reconcile_catalog(&first.config, failed, Default::default());
    assert_eq!(output.config, first.config);
}

#[test]
fn service_does_not_save_an_incomplete_scan() {
    let config = TestDir::new();
    let store = ConfigStore::new(config.path());
    let models = TestDir::new();
    models.file("existing.gguf", &tiny_gguf("Existing"));
    let initial = CatalogService::new(models.path(), store.clone())
        .reconcile_now()
        .unwrap();
    let missing_root = models.path().join("missing-root");
    let failed = CatalogService::new(&missing_root, store.clone())
        .reconcile_now()
        .unwrap();
    assert!(!failed.diagnostics.is_empty());
    assert_eq!(store.load().unwrap(), initial.config);
}

#[test]
fn file_identity_is_bounded_and_best_effort() {
    let root = TestDir::new();
    let path = root.file("model.gguf", &tiny_gguf("Identity"));
    let model = &scan(root.path()).models[0];

    #[cfg(any(unix, windows))]
    assert_ne!(model.identity, CatalogIdentity::Unavailable);
    #[cfg(not(any(unix, windows)))]
    assert_eq!(model.identity, CatalogIdentity::Unavailable);
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

#[tokio::test(start_paused = true)]
async fn service_resets_quiet_period_then_scans_saves_and_reloads_once() {
    let models = TestDir::new();
    models.file("model.gguf", &tiny_gguf("Persisted"));
    let config = TestDir::new();
    let store = ConfigStore::new(config.path());
    let service = CatalogService::new(models.path(), store.clone());
    let (sender, mut receiver) = catalog_watch_channel(Duration::from_millis(250));
    sender.emit(CatalogWatchEvent::Changed(PathBuf::from("one.gguf")));
    let task = tokio::spawn(async move { service.process_next(&mut receiver).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(200)).await;
    sender.emit(CatalogWatchEvent::Rescan);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(249)).await;
    assert!(
        !task.is_finished(),
        "timer resets when an event arrives during the quiet period"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    let output = task.await.unwrap().unwrap().unwrap();
    assert_eq!(output.config.models.len(), 1);
    assert_eq!(store.load().unwrap(), output.config);
}

#[tokio::test(start_paused = true)]
async fn watcher_errors_reach_service_diagnostics() {
    let models = TestDir::new();
    let config = TestDir::new();
    let service = CatalogService::new(models.path(), ConfigStore::new(config.path()));
    let (sender, mut receiver) = catalog_watch_channel(Duration::from_millis(10));
    sender.emit(CatalogWatchEvent::Error("backend stopped".into()));
    let task = tokio::spawn(async move { service.process_next(&mut receiver).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(10)).await;
    let output = task.await.unwrap().unwrap().unwrap();
    assert!(
        output
            .diagnostics
            .iter()
            .any(|item| item.message.contains("backend stopped"))
    );
}
