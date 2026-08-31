use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use gguf_rs_lib::{builder::GGUFBuilder, format::MetadataValue};
use model_launcher_core::{
    CatalogDebouncer, CatalogDiagnosticKind, CatalogIdentity, CatalogService, CatalogWatchEvent,
    CatalogWatcher, ConfigStore, ContextLength, LaunchSettings, LauncherConfig,
    MAX_CATALOG_DIAGNOSTICS, MAX_DISCOVERED_GGUF_FILES, MAX_DISCOVERED_MODELS,
    MAX_TOTAL_CATALOG_TENSORS, ModelKey, ModelState, ReconcileOptions, WATCH_MAX_BATCH_DIAGNOSTICS,
    catalog_watch_channel, catalog_watch_channel_with_limits, reconcile_catalog, scan,
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
        .add_metadata("general.file_type", MetadataValue::U32(15))
        .add_metadata("general.quantization_version", MetadataValue::U32(2))
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
    assert_eq!(metadata.metadata.quantization_version, Some(2));
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
fn unknown_standard_file_type_is_retained_with_diagnostic() {
    let root = TestDir::new();
    let bytes = GGUFBuilder::new()
        .add_metadata("general.name", MetadataValue::String("Future".into()))
        .add_metadata("general.file_type", MetadataValue::U32(999))
        .build_to_bytes()
        .unwrap()
        .0;
    root.file("future.gguf", &bytes);
    let result = scan(root.path());
    assert_eq!(
        result.models[0].metadata.quantization.as_deref(),
        Some("FILE_TYPE_999")
    );
    assert!(result.complete);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item.kind == CatalogDiagnosticKind::Metadata)
    );
}

#[test]
fn standard_file_types_have_stable_display_names() {
    let root = TestDir::new();
    let cases = [
        (0, "F32"),
        (1, "F16"),
        (2, "Q4_0"),
        (3, "Q4_1"),
        (4, "Q4_1_SOME_F16"),
        (7, "Q8_0"),
        (8, "Q5_0"),
        (9, "Q5_1"),
        (10, "Q2_K"),
        (12, "Q3_K_M"),
        (15, "Q4_K_M"),
        (17, "Q5_K_M"),
        (18, "Q6_K"),
        (19, "IQ2_XXS"),
        (23, "IQ3_XXS"),
        (31, "IQ1_M"),
        (32, "BF16"),
        (36, "TQ1_0"),
        (37, "TQ2_0"),
        (38, "MXFP4_MOE"),
    ];
    for (value, expected) in cases {
        let bytes = GGUFBuilder::new()
            .add_metadata("general.name", MetadataValue::String(expected.into()))
            .add_metadata("general.file_type", MetadataValue::U32(value))
            .build_to_bytes()
            .unwrap()
            .0;
        root.file(&format!("{value}.gguf"), &bytes);
    }
    let result = scan(root.path());
    for (_, expected) in cases {
        let model = result
            .models
            .iter()
            .find(|model| model.display_name == expected)
            .unwrap();
        assert_eq!(model.metadata.quantization.as_deref(), Some(expected));
    }
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
fn excessive_declared_shard_total_is_rejected_without_expansion() {
    let root = TestDir::new();
    root.file("huge-00001-of-99999.gguf", &tiny_gguf("Huge"));
    let result = scan(root.path());
    assert!(!result.complete);
    assert!(result.models.is_empty());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item.message.contains("shard total"))
    );
}

#[test]
fn injected_sparse_shard_sizes_cannot_overflow_total() {
    let root = TestDir::new();
    root.file("overflow-00001-of-00002.gguf", &tiny_gguf("Overflow"));
    root.file("overflow-00002-of-00002.gguf", b"payload");
    let result = model_launcher_core::scan_with_size_hook(root.path(), &|path, actual| {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("00001"))
        {
            u64::MAX
        } else {
            actual
        }
    });
    assert!(!result.complete);
    assert!(result.models.is_empty());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item.message.contains("overflow"))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn normalized_filename_collision_is_incomplete_and_ambiguous_group_is_skipped() {
    let root = TestDir::new();
    root.file("Mix-00001-of-00002.GGUF", &tiny_gguf("One"));
    root.file("mix-00001-of-00002.gguf", &tiny_gguf("Other one"));
    root.file("Mix-00002-of-00002.GGUF", b"payload");
    let result = scan(root.path());
    assert!(!result.complete);
    assert!(result.models.is_empty());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item.message.contains("ambiguous"))
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
fn unreadable_discovered_file_makes_scan_incomplete_without_partial_model() {
    use std::os::unix::fs::PermissionsExt;
    let root = TestDir::new();
    let path = root.file("unreadable.gguf", &tiny_gguf("Unsafe partial"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let result = scan(root.path());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert!(!result.complete);
        assert!(result.models.is_empty());
        assert!(result.diagnostics.iter().any(|item| item.path == path));
    }
}

#[cfg(unix)]
#[test]
fn atomic_replacement_after_open_is_incomplete_and_never_saved() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let models = TestDir::new();
    let path = models.file("model.gguf", &tiny_gguf("Original"));
    let config = TestDir::new();
    let store = ConfigStore::new(config.path());
    let service = CatalogService::new(models.path(), store.clone());
    let initial = service.reconcile_now().unwrap();
    let replacement = models.file("replacement.tmp", &tiny_gguf("Replacement"));
    let replaced = AtomicBool::new(false);

    let scanned = model_launcher_core::scan_with_hook(models.path(), &|opened| {
        if opened.contains(&path) && !replaced.swap(true, Ordering::SeqCst) {
            fs::rename(&replacement, &path).unwrap();
        }
    });
    assert!(!scanned.complete);
    assert!(scanned.models.is_empty());
    let output = service.reconcile_scan(scanned).unwrap();
    assert_eq!(output.config, initial.config);
    assert_eq!(store.load().unwrap(), initial.config);
}

#[cfg(unix)]
#[test]
fn root_swap_to_outside_symlink_is_rejected_before_outside_open() {
    use std::os::unix::fs::symlink;
    let holder = TestDir::new();
    let root = holder.path().join("models");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("inside.gguf"), tiny_gguf("Inside")).unwrap();
    let outside = TestDir::new();
    outside.file("inside.gguf", &tiny_gguf("Outside secret"));
    let parked = holder.path().join("models-parked");

    let result = model_launcher_core::scan_with_discovery_hook(&root, &|| {
        fs::rename(&root, &parked).unwrap();
        symlink(outside.path(), &root).unwrap();
    });
    fs::remove_file(&root).unwrap();
    fs::rename(&parked, &root).unwrap();

    assert!(!result.complete);
    assert!(result.models.is_empty());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item.message.contains("root changed"))
    );
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
fn malicious_gguf_header_counts_are_rejected_by_catalog_budgets() {
    fn header(tensors: u64, metadata: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x4655_4747_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(&tensors.to_le_bytes());
        bytes.extend_from_slice(&metadata.to_le_bytes());
        bytes
    }
    let root = TestDir::new();
    root.file("metadata-bomb.gguf", &header(0, u64::MAX));
    root.file("tensor-bomb.gguf", &header(u64::MAX, 0));

    let result = scan(root.path());

    assert!(
        result.complete,
        "stable malicious files degrade without invalidating traversal"
    );
    assert_eq!(result.models.len(), 2);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item.message.contains("catalog metadata entry limit"))
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item.message.contains("catalog tensor count limit"))
    );
}

#[test]
fn total_metadata_budget_and_diagnostic_count_are_bounded() {
    fn header(metadata: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x4655_4747_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&metadata.to_le_bytes());
        bytes
    }
    let root = TestDir::new();
    for index in 0..5 {
        root.file(&format!("budget-{index}.gguf"), &header(16_000));
    }
    for index in 0..(MAX_CATALOG_DIAGNOSTICS + 20) {
        root.file(
            &format!("diagnostic-{index}.gguf"),
            &metadata_gguf(MetadataValue::U32(index as u32)),
        );
    }
    let result = scan(root.path());
    assert!(!result.complete);
    assert_eq!(result.diagnostics.len(), MAX_CATALOG_DIAGNOSTICS);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|item| item.message.contains("total metadata entry budget"))
    );
}

#[test]
fn cumulative_tensor_budget_is_incomplete_and_preserves_saved_config() {
    fn header(tensors: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x4655_4747_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(&tensors.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes
    }
    let models = TestDir::new();
    models.file("existing.gguf", &tiny_gguf("Existing"));
    let config = TestDir::new();
    let store = ConfigStore::new(config.path());
    let service = CatalogService::new(models.path(), store.clone());
    let initial = service.reconcile_now().unwrap().config;
    let per_file = MAX_TOTAL_CATALOG_TENSORS / 3;
    for index in 0..4 {
        models.file(&format!("tensor-budget-{index}.gguf"), &header(per_file));
    }

    let scanned = scan(models.path());
    assert!(!scanned.complete);
    assert!(
        scanned
            .diagnostics
            .iter()
            .any(|item| { item.message.contains("total tensor descriptor budget") })
    );
    let output = service.reconcile_scan(scanned).unwrap();
    assert_eq!(output.config, initial);
    assert_eq!(store.load().unwrap(), initial);
}

#[test]
fn discovered_file_and_model_counts_are_globally_bounded() {
    let root = TestDir::new();
    for index in 0..=MAX_DISCOVERED_GGUF_FILES {
        root.file(&format!("model-{index:05}.gguf"), b"x");
    }
    let result = scan(root.path());
    assert!(!result.complete);
    assert!(result.models.len() <= MAX_DISCOVERED_MODELS);
    assert!(result.diagnostics.len() <= MAX_CATALOG_DIAGNOSTICS);
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

#[cfg(any(unix, windows))]
#[test]
fn hardlinks_are_distinct_records_and_do_not_reuse_new_uuid() {
    let root = TestDir::new();
    let first = root.file("first.gguf", &tiny_gguf("Hardlinked"));
    fs::hard_link(&first, root.path().join("second.gguf")).unwrap();
    let output = reconcile_catalog(
        &LauncherConfig::default(),
        scan(root.path()),
        Default::default(),
    );
    assert_eq!(output.config.models.len(), 2);
    assert_ne!(output.config.models[0].id, output.config.models[1].id);
    assert_ne!(output.config.models[0].key, output.config.models[1].key);
}

#[cfg(any(unix, windows))]
#[test]
fn identity_fingerprint_changes_when_same_inode_contents_change() {
    use std::io::Write as _;
    let root = TestDir::new();
    let path = root.file("mutable.gguf", &tiny_gguf("Before"));
    let before = CatalogIdentity::for_path(&path);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"change")
        .unwrap();
    let after = CatalogIdentity::for_path(&path);
    assert_ne!(before, after);
}

#[cfg(unix)]
#[test]
fn reused_inode_with_changed_fingerprint_does_not_inherit_saved_settings() {
    let root = TestDir::new();
    root.file("old.gguf", &tiny_gguf("Old"));
    let mut saved = reconcile_catalog(
        &LauncherConfig::default(),
        scan(root.path()),
        Default::default(),
    )
    .config;
    saved.models[0].key = ModelKey::parse("old-user-key").unwrap();
    let old_id = saved.models[0].id;
    let changed_identity = match saved.models[0].file_identity.clone() {
        CatalogIdentity::Unix {
            device,
            inode_fingerprint,
        } => CatalogIdentity::Unix {
            device,
            inode_fingerprint: inode_fingerprint.wrapping_add(1),
        },
        CatalogIdentity::Unavailable | CatalogIdentity::Windows { .. } => unreachable!(),
    };
    let scan = model_launcher_core::ScanResult {
        complete: true,
        diagnostics: Vec::new(),
        models: vec![model_launcher_core::ScannedModel {
            display_name: "Replacement".into(),
            path: root.path().join("new.gguf"),
            size_bytes: saved.models[0].size_bytes + 1,
            identity: changed_identity,
            metadata: Default::default(),
        }],
    };
    let output = reconcile_catalog(&saved, scan, Default::default());
    assert_eq!(output.config.models.len(), 2);
    assert_eq!(output.config.models[0].id, old_id);
    assert_eq!(output.config.models[0].state, ModelState::Missing);
    assert_ne!(output.config.models[1].id, old_id);
    assert_ne!(output.config.models[1].key.as_str(), "old-user-key");
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
fn newly_discovered_models_inherit_global_launch_defaults() {
    let root = TestDir::new();
    root.file("model.gguf", &tiny_gguf("Defaults"));
    let defaults = LaunchSettings {
        context_length: Some(ContextLength::new(8192).unwrap()),
        flash_attention: Some(true),
        ..LaunchSettings::default()
    };
    let saved = LauncherConfig {
        default_launch_settings: defaults.clone(),
        ..LauncherConfig::default()
    };

    let output = reconcile_catalog(&saved, scan(root.path()), Default::default());

    assert_eq!(output.config.models[0].launch_profile.settings, defaults);
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
fn concurrent_user_edit_and_catalog_reconcile_are_both_preserved() {
    use std::sync::{Arc, Barrier};
    let models = TestDir::new();
    models.file("model.gguf", &tiny_gguf("Concurrent"));
    let config = TestDir::new();
    let store = ConfigStore::new(config.path());
    let service = CatalogService::new(models.path(), store.clone());
    service.reconcile_now().unwrap();
    let scanned = scan(models.path());
    let barrier = Arc::new(Barrier::new(3));

    let edit_store = store.clone();
    let edit_barrier = barrier.clone();
    let edit = std::thread::spawn(move || {
        edit_barrier.wait();
        edit_store.update(|latest| {
            latest.models[0].key = ModelKey::parse("user-concurrent-key")?;
            latest.models[0].launch_profile.settings.context_length =
                Some(ContextLength::new(4096)?);
            Ok(())
        })
    });
    let reconcile_barrier = barrier.clone();
    let reconcile = std::thread::spawn(move || {
        reconcile_barrier.wait();
        service.reconcile_scan(scanned)
    });
    barrier.wait();
    edit.join().unwrap().unwrap();
    reconcile.join().unwrap().unwrap();

    let saved = store.load().unwrap();
    assert_eq!(saved.models[0].key.as_str(), "user-concurrent-key");
    assert_eq!(
        saved.models[0]
            .launch_profile
            .settings
            .context_length
            .unwrap()
            .get(),
        4096
    );
    assert_eq!(saved.models[0].state, ModelState::Available);
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

#[tokio::test(start_paused = true)]
async fn watch_storm_returns_at_hard_latency_even_without_quiet() {
    let models = TestDir::new();
    let config = TestDir::new();
    let service = CatalogService::new(models.path(), ConfigStore::new(config.path()));
    let (sender, mut receiver) = catalog_watch_channel_with_limits(
        Duration::from_millis(100),
        Duration::from_millis(350),
        8,
    );
    sender.emit(CatalogWatchEvent::Rescan);
    let task = tokio::spawn(async move { service.process_next(&mut receiver).await });
    tokio::task::yield_now().await;
    for _ in 0..3 {
        tokio::time::advance(Duration::from_millis(90)).await;
        sender.emit(CatalogWatchEvent::Rescan);
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(79)).await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(task.await.unwrap().unwrap().is_some());
}

#[tokio::test(start_paused = true)]
async fn bounded_watch_channel_reports_overflow_and_caps_diagnostics() {
    let models = TestDir::new();
    let config = TestDir::new();
    let service = CatalogService::new(models.path(), ConfigStore::new(config.path()));
    let (sender, mut receiver) =
        catalog_watch_channel_with_limits(Duration::from_millis(10), Duration::from_millis(100), 2);
    for index in 0..100 {
        sender.emit(CatalogWatchEvent::Error(format!("watch error {index}")));
    }
    let task = tokio::spawn(async move { service.process_next(&mut receiver).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(10)).await;
    let output = task.await.unwrap().unwrap().unwrap();
    assert!(
        output
            .diagnostics
            .iter()
            .any(|item| item.message.contains("dropped 98"))
    );
    assert!(output.diagnostics.len() <= WATCH_MAX_BATCH_DIAGNOSTICS + 1);
}

#[tokio::test]
async fn real_watcher_drives_service_and_persists_discovery() {
    let models = TestDir::new();
    let config = TestDir::new();
    let store = ConfigStore::new(config.path());
    let service = CatalogService::new(models.path(), store.clone());
    let mut watcher = CatalogWatcher::watch(models.path(), Duration::from_millis(100)).unwrap();
    // Give platform backends (notably macOS FSEvents) time to finish registering the root.
    tokio::time::sleep(Duration::from_millis(250)).await;
    models.file("watched.gguf", &tiny_gguf("Watched"));

    let output = tokio::time::timeout(Duration::from_secs(5), watcher.process_next(&service))
        .await
        .expect("filesystem notification timeout")
        .unwrap()
        .unwrap();

    assert_eq!(output.config.models[0].display_name, "Watched");
    assert_eq!(store.load().unwrap(), output.config);
}
