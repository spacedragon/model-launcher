//! Tests for the model scanner: pure key/collision logic, the secure walk, and
//! end-to-end reconciliation against a real SQLite ledger.
//!
//! Required coverage (job 4 of `docs/development-plan.md`):
//!
//! - restart / index update: identity and keys survive a fresh scan;
//! - collision order: an artifact order change never moves a key;
//! - unchanged and update: `mtime`/`size` incremental decision;
//! - delete / reappear: missing artifacts are marked `deleted` and restored;
//! - traversal: nested trees, unsupported extensions, non-regular files;
//! - symlink escape: a link out of the root is never indexed;
//! - rollback: a failing reconciliation leaves the index untouched.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::{TimeDelta, Utc};
use model_serving_domain::error::ErrorCode;
use model_serving_domain::model::ArtifactKind;
use model_serving_domain::model::Model;
use model_serving_domain::model::{Capabilities, Runtime, RuntimeKind};
use model_serving_persistence::SqliteStore;
use model_serving_persistence::repos::{
    AuditRepo, ModelRoot, ModelRootsRepo, ModelsRepo, RuntimeRepo, RuntimeWithProbe,
};
use model_serving_persistence::test_support::Fixture;

use crate::key::{base_key, model_id};
use crate::persistence::PersistenceState;

use super::*;

// --------------------------------------------------------------------------
// fixtures
// --------------------------------------------------------------------------

/// Creates a temporary model root on disk. The directory is removed when the
/// guard drops (tempfile, workspace dev-dependency).
struct Root {
    dir: tempfile::TempDir,
}

impl Root {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("create temp root"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The canonical root path, i.e. exactly what [`validate_root`] hands to
    /// [`scan_tree`]. On Windows `canonicalize` yields the extended-length
    /// (verbatim) form, which is not string-equal to the temp path, so a walk
    /// must be driven from this value and not from [`Root::path`].
    fn canonical(&self) -> PathBuf {
        std::fs::canonicalize(self.path()).expect("canonicalize fixture root")
    }

    /// Write an artifact and return its canonical path as a string.
    fn write(&self, relative: &str, bytes: &[u8]) -> String {
        let path = self.path().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, bytes).expect("write artifact");
        canonical_string(&path)
    }

    fn mkdir(&self, relative: &str) -> PathBuf {
        let path = self.path().join(relative);
        std::fs::create_dir_all(&path).expect("create dir");
        path
    }

    /// The non-canonical path of an artifact written by [`Root::write`].
    fn write_path(&self, relative: &str) -> PathBuf {
        self.path().join(relative)
    }
}

fn canonical_string(path: &Path) -> String {
    std::fs::canonicalize(path)
        .expect("canonicalize fixture path")
        .to_string_lossy()
        .into_owned()
}

/// Load a root record, so a test can assert on its bookkeeping columns.
async fn root_record(store: &model_serving_persistence::SqliteStore, id: &str) -> ModelRoot {
    ModelRootsRepo::get(store.pool(), id)
        .await
        .expect("load root")
        .expect("root exists")
}

fn fake_gguf(display_name: &str, trailer: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"GGUF");
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // tensor count
    out.extend_from_slice(&1u64.to_le_bytes()); // kv count
    let key = b"general.name";
    out.extend_from_slice(&(key.len() as u64).to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(&8u32.to_le_bytes()); // string value
    out.extend_from_slice(&(display_name.len() as u64).to_le_bytes());
    out.extend_from_slice(display_name.as_bytes());
    out.extend_from_slice(trailer);
    out
}

fn scanned(path: &str) -> ScannedFile {
    ScannedFile {
        path: path.to_owned(),
        artifact_kind: ArtifactKind::Gguf,
        size_bytes: 1,
        mtime: Utc::now(),
        display_name: None,
    }
}

/// A migrated temp-file ledger fixture (the workspace's own fixture, so the
/// tests exercise the same PRAGMA/migration path as production).
async fn store() -> Fixture {
    Fixture::new().await.expect("open fixture ledger")
}

async fn register_root(store: &SqliteStore, path: &Path, id: &str) -> ModelRoot {
    ModelRootsRepo::upsert(
        store.pool(),
        &ModelRoot {
            id: id.to_owned(),
            path: path.to_string_lossy().into_owned(),
            enabled: true,
            last_scan_at: None,
            last_scan_result: None,
        },
    )
    .await
    .expect("register model root");
    ModelRootsRepo::get(store.pool(), id)
        .await
        .expect("load model root")
        .expect("model root exists")
}

async fn models(store: &SqliteStore) -> Vec<Model> {
    ModelsRepo::list(store.pool(), true)
        .await
        .expect("list models")
}

/// Register a runtime so a model may legally reference it: `models.
/// default_runtime_id` is a foreign key into `runtimes`.
async fn register_runtime(store: &SqliteStore, id: &str) {
    RuntimeRepo::upsert(
        store.pool(),
        &RuntimeWithProbe {
            runtime: Runtime {
                id: id.to_owned(),
                kind: RuntimeKind::LlamaCpp,
                executable_path: "/opt/llama/llama-server".to_owned(),
                enabled: true,
                version_text: None,
                capabilities: Capabilities::default(),
                fixed_args: Vec::new(),
            },
            last_probe_ok: None,
            last_probed_at: None,
        },
    )
    .await
    .expect("register runtime");
}

async fn model_by_key(store: &SqliteStore, key: &str) -> Option<Model> {
    ModelsRepo::get_by_key(store.pool(), key)
        .await
        .expect("get model by key")
}

// --------------------------------------------------------------------------
// root validation
// --------------------------------------------------------------------------

#[test]
fn rejects_relative_root() {
    let error = validate_root(Path::new("models")).expect_err("relative root must be rejected");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("not absolute"), "{}", error.message);
}

#[test]
fn rejects_missing_root() {
    let temp = Root::new();
    let missing = temp.path().join("nope");
    let error = validate_root(&missing).expect_err("missing root must be rejected");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
}

#[test]
fn rejects_parent_dir_root() {
    let temp = Root::new();
    let traversing = temp.path().join("a").join("..").join("b");
    std::fs::create_dir_all(temp.path().join("a")).expect("mkdir a");
    let error = validate_root(&traversing).expect_err("`..` must be rejected");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("`..`"), "{}", error.message);
}

#[test]
fn rejects_file_root() {
    let temp = Root::new();
    let file = temp.path().join("model.gguf");
    std::fs::write(&file, b"x").expect("write file");
    let error = validate_root(&file).expect_err("a file is not a root");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(
        error.message.contains("not a directory"),
        "{}",
        error.message
    );
}

#[test]
fn accepts_directory_root_and_canonicalizes() {
    let temp = Root::new();
    temp.mkdir("nested");
    let canonical = validate_root(&temp.path().join("nested")).expect("valid root");
    assert!(canonical.is_absolute());
    assert_eq!(
        canonical.to_string_lossy(),
        canonical_string(&temp.path().join("nested"))
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_root() {
    let temp = Root::new();
    let real = temp.mkdir("real");
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("create dir symlink");
    let error = validate_root(&link).expect_err("a symlinked root must be rejected");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("symlink"), "{}", error.message);
}

// --------------------------------------------------------------------------
// key + collision determinism
// --------------------------------------------------------------------------

#[test]
fn base_key_is_a_lowercase_token() {
    assert_eq!(base_key("/models/Qwen2.5-7B GGUF.gguf"), "qwen2-5-7b-gguf");
    assert_eq!(base_key("/models/a__b.gguf"), "a-b");
    assert_eq!(base_key("/models/---.gguf"), "model");
    assert_eq!(base_key("/models/MODEL.ninfer"), "model");
}

#[test]
fn model_id_is_stable_and_path_specific() {
    let first = model_id("/models/a.gguf");
    assert_eq!(first, model_id("/models/a.gguf"));
    assert_ne!(first, model_id("/models/b.gguf"));
    assert_eq!(first.len(), 36);
    assert_eq!(&first[14..15], "5", "RFC 4122 version 5: {first}");
    assert!(matches!(&first[19..20], "8" | "9" | "a" | "b"), "{first}");
}

#[test]
fn sha256_matches_known_vectors() {
    // FIPS 180-4 test vectors, to prove the local digest is a real SHA-256.
    assert_eq!(
        hex(&crate::key::sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&crate::key::sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&crate::key::sha256(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[test]
fn unique_base_keys_are_used_as_is() {
    let candidates = vec!["alpha".to_owned(), "beta".to_owned()];
    let resolved = key::build_index(&candidates, &key::Reserved::new());
    assert_eq!(resolved, candidates);
}

#[test]
fn collision_order_does_not_change_keys() {
    // Two files in different directories but with the same stem.
    let left = "/models/a/shared.gguf".to_owned();
    let right = "/models/b/shared.gguf".to_owned();

    // Traversal order A: left, right. Traversal order B: right, left.
    let forward = key::keys_for_paths(&[left.clone(), right.clone()]);
    let backward = key::keys_for_paths(&[right.clone(), left.clone()]);

    // Keyed by path the two traversals agree: the lexicographically smallest
    // path always takes the lowest free suffix.
    assert_eq!(forward, backward);
    assert_eq!(forward[&left], "shared-1");
    assert_eq!(forward[&right], "shared-2");
}

#[test]
fn keys_for_paths_is_order_independent() {
    let paths = vec![
        "/models/z/shared.gguf".to_owned(),
        "/models/a/shared.gguf".to_owned(),
        "/models/m/shared.gguf".to_owned(),
        "/models/solo.gguf".to_owned(),
    ];
    let mut shuffled = paths.clone();
    shuffled.reverse();
    shuffled.swap(0, 2);
    assert_eq!(key::keys_for_paths(&paths), key::keys_for_paths(&shuffled));

    let keys = key::keys_for_paths(&paths);
    assert_eq!(keys["/models/solo.gguf"], "solo");
    assert_eq!(keys["/models/a/shared.gguf"], "shared-1");
    assert_eq!(keys["/models/m/shared.gguf"], "shared-2");
    assert_eq!(keys["/models/z/shared.gguf"], "shared-3");
}

#[test]
fn reserved_key_forces_a_suffix() {
    // The index already holds `shared` for an untouched row.
    let reserved = key::Reserved::from(["shared".to_owned()]);
    let resolved = key::build_index(&["shared".to_owned()], &reserved);
    assert!(!reserved.contains(&resolved[0]));
    assert_eq!(resolved[0], "shared-1");
}

#[test]
fn collision_suffixes_never_collide_with_reserved_keys() {
    // The index already holds `shared` and `shared-1` for untouched rows, so
    // the two colliding candidates must rank past the whole family: the
    // existing `shared-1` is skipped and the candidates take `shared-2` and
    // `shared-3` in path order.
    let reserved = key::Reserved::from(["shared-1".to_owned(), "shared".to_owned()]);
    let candidates = vec![
        base_key("/models/a/shared.gguf"),
        base_key("/models/b/shared.gguf"),
    ];
    let resolved = key::build_index(&candidates, &reserved);
    assert_eq!(resolved, vec!["shared-2", "shared-3"]);
    for key in &resolved {
        assert!(
            !reserved.contains(key),
            "`{key}` collides with a reserved key"
        );
    }
}

#[test]
fn artifact_kind_of_matches_extensions_case_insensitively() {
    assert_eq!(
        artifact_kind_of(Path::new("/m/a.GGUF")),
        Some(ArtifactKind::Gguf)
    );
    assert_eq!(
        artifact_kind_of(Path::new("/m/a.Ninfer")),
        Some(ArtifactKind::Ninfer)
    );
    assert_eq!(artifact_kind_of(Path::new("/m/a.bin")), None);
    assert_eq!(artifact_kind_of(Path::new("/m/a")), None);
}

// --------------------------------------------------------------------------
// traversal
// --------------------------------------------------------------------------

#[test]
fn walk_finds_nested_artifacts_and_ignores_other_files() {
    let root = Root::new();
    root.write("top.gguf", b"gguf");
    root.write("nested/deep/model.ninfer", b"ninfer");
    root.write("nested/readme.txt", b"text");
    root.write("weights.bin", b"bin");

    let scanned = scan_tree(&root.canonical(), ScanOptions::default()).expect("scan");
    let names: Vec<String> = scanned
        .files
        .iter()
        .map(|file| {
            Path::new(&file.path)
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec!["model.ninfer", "top.gguf"]);
    assert_eq!(scanned.skipped, 0);
    assert_eq!(scanned.discarded, 0);
}

#[test]
fn walk_result_is_sorted_and_stable() {
    let root = Root::new();
    root.write("z.gguf", b"z");
    root.write("a.gguf", b"a");
    root.write("m.gguf", b"m");
    let scanned = scan_tree(&root.canonical(), ScanOptions::default()).expect("scan");
    let paths: Vec<&String> = scanned.files.iter().map(|file| &file.path).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "walk output must be in lexicographic order");
}

#[test]
fn walk_records_gguf_display_name_and_reads_only_after_magic() {
    let root = Root::new();
    root.write("named.gguf", &fake_gguf("Tiny Model", &[0u8; 32]));
    root.write("junk.gguf", b"not a gguf at all");
    let scanned = scan_tree(&root.canonical(), ScanOptions::default()).expect("scan");
    let named = scanned
        .files
        .iter()
        .find(|file| file.path.ends_with("named.gguf"))
        .expect("named.gguf indexed");
    assert_eq!(named.display_name.as_deref(), Some("Tiny Model"));
    let junk = scanned
        .files
        .iter()
        .find(|file| file.path.ends_with("junk.gguf"))
        .expect("junk.gguf still indexed");
    assert_eq!(junk.display_name.as_deref(), Some("junk"));
}

#[test]
fn walk_skips_empty_and_root_only_trees() {
    let root = Root::new();
    let scanned = scan_tree(&root.canonical(), ScanOptions::default()).expect("scan");
    assert!(scanned.files.is_empty());
}

#[test]
fn walk_respects_max_depth() {
    let root = Root::new();
    root.write("one.gguf", b"1");
    root.write("a/two.gguf", b"2");
    root.write("a/b/three.gguf", b"3");

    // A depth budget omits a subtree, so the walk fails closed rather than
    // silently returning a partial view of the tree.
    let error = scan_tree(
        &root.canonical(),
        ScanOptions {
            max_depth: 0,
            ..ScanOptions::default()
        },
    )
    .expect_err("a depth budget must abort the walk");
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");

    let error = scan_tree(
        &root.canonical(),
        ScanOptions {
            max_depth: 1,
            ..ScanOptions::default()
        },
    )
    .expect_err("a depth budget must abort the walk");
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");

    // A budget that is not exceeded still walks the whole tree.
    let full = scan_tree(
        &root.canonical(),
        ScanOptions {
            max_depth: 2,
            ..ScanOptions::default()
        },
    )
    .expect("a sufficient budget scans normally");
    assert_eq!(full.files.len(), 3);
    assert_eq!(full.skipped, 0);
}

#[test]
fn walk_reports_canonical_paths_inside_the_root() {
    let root = Root::new();
    root.write("a/b.gguf", b"x");
    let scanned = scan_tree(&root.canonical(), ScanOptions::default()).expect("scan");
    let canonical_root = canonical_string(root.path());
    for file in &scanned.files {
        assert!(
            file.path.starts_with(&canonical_root),
            "{} escaped {}",
            file.path,
            canonical_root
        );
        assert_eq!(file.path, canonical_string(Path::new(&file.path)));
    }
}

#[cfg(unix)]
#[test]
fn walk_never_follows_a_symlink_out_of_the_root() {
    let outside = Root::new();
    let secret = outside.write("secret.gguf", b"do not index me");

    let root = Root::new();
    root.write("real.gguf", b"ok");
    std::os::unix::fs::symlink(Path::new(&secret), root.path().join("escaped.gguf"))
        .expect("file symlink");
    std::os::unix::fs::symlink(outside.path(), root.path().join("escaped-dir"))
        .expect("dir symlink");

    let scanned = scan_tree(&root.canonical(), ScanOptions::default()).expect("scan");
    let indexed: Vec<&String> = scanned.files.iter().map(|file| &file.path).collect();
    assert_eq!(indexed.len(), 1, "only the real artifact: {indexed:?}");
    assert!(indexed[0].ends_with("real.gguf"));
    assert!(
        !scanned
            .files
            .iter()
            .any(|file| file.path.contains("escaped") || file.path == secret.as_str()),
        "a symlink target must never be indexed"
    );
    assert!(scanned.skipped >= 2, "both links are counted as skipped");
}

#[cfg(unix)]
#[test]
fn walk_does_not_loop_on_a_symlinked_directory() {
    let root = Root::new();
    root.write("a/one.gguf", b"1");
    // `a/self -> ..` would make an unsafeguarded walk recurse forever.
    std::os::unix::fs::symlink(root.path(), root.path().join("a").join("self"))
        .expect("self symlink");
    let scanned = scan_tree(&root.canonical(), ScanOptions::default()).expect("scan");
    assert_eq!(scanned.files.len(), 1);
}

/// Windows counterpart of [`walk_never_follows_a_symlink_out_of_the_root`].
///
/// Symlink creation needs developer mode or elevation on Windows, so the test
/// tries both link kinds and skips (rather than fails) when the host refuses:
/// the point is that a reparse point inside the root is never traversed, and a
/// host that cannot create one cannot exercise it. A directory junction is
/// attempted too, because a junction is a reparse point that
/// `Metadata::file_type().is_symlink()` does **not** report — it is only
/// caught by the `FILE_ATTRIBUTE_REPARSE_POINT` check in
/// [`crate::walk::is_link_like`].
#[cfg(windows)]
#[test]
fn windows_reparse_points_are_never_followed() {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    use std::process::Command;

    let outside = Root::new();
    let secret = outside.write("secret.gguf", b"do not index me");

    let root = Root::new();
    root.write("real.gguf", b"ok");

    let mut links_created = 0usize;
    if symlink_file(Path::new(&secret), root.path().join("escaped.gguf")).is_ok() {
        links_created += 1;
    }
    if symlink_dir(outside.path(), root.path().join("escaped-dir")).is_ok() {
        links_created += 1;
    }
    // A junction needs no special privilege on most Windows hosts.
    let junction = root.path().join("junction-dir");
    let made_junction = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(outside.path())
        .output()
        .is_ok_and(|out| out.status.success());
    if made_junction {
        links_created += 1;
    }
    if links_created == 0 {
        return; // no reparse point could be created on this host
    }

    let scanned = scan_tree(&root.canonical(), ScanOptions::default()).expect("scan");
    let indexed: Vec<&String> = scanned.files.iter().map(|file| &file.path).collect();
    assert_eq!(indexed.len(), 1, "only the real artifact: {indexed:?}");
    assert!(indexed[0].ends_with("real.gguf"));
    for file in &scanned.files {
        assert!(
            !file.path.contains("escaped") && !file.path.contains("junction"),
            "a reparse point was followed: {}",
            file.path
        );
    }
    assert!(
        scanned.skipped >= links_created as u64,
        "every link is counted as skipped (made {links_created}, skipped {})",
        scanned.skipped
    );
}

#[cfg(unix)]
#[test]
fn walk_skips_fifos() {
    let root = Root::new();
    root.write("real.gguf", b"x");
    let fifo = root.path().join("pipe.gguf");
    // `mkfifo` is an ordinary process, so the argument is an `OsStr` path —
    // `Command::arg` accepts a `Path` directly. A `CString` would be both wrong
    // here (`CString` is not `AsRef<OsStr>`, which does not compile) and lossy
    // (`to_string_lossy` mangles non-UTF-8 paths). No libc call is involved.
    let status = std::process::Command::new("mkfifo").arg(&fifo).status();
    match status {
        Ok(status) if status.success() => {}
        // No `mkfifo` on this host: the non-regular-file branch is still
        // covered by the directory-symlink tests.
        _ => return,
    }
    let scanned = scan_tree(&root.canonical(), ScanOptions::default()).expect("scan");
    assert_eq!(scanned.files.len(), 1);
    assert!(scanned.skipped >= 1);
}

// --------------------------------------------------------------------------
// reconciliation: restart, unchanged/update, delete/reappear, admin fields
// --------------------------------------------------------------------------

#[tokio::test]
async fn scan_indexes_artifacts_and_records_root_bookkeeping() {
    let store = store().await;
    let root = Root::new();
    root.write("first.gguf", &fake_gguf("First Model", &[0u8; 16]));
    root.write("second.ninfer", b"ninfer");
    let record = register_root(&store.store, root.path(), "root-1").await;

    let summary = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan");
    assert_eq!(summary.files_seen, 2);
    assert_eq!(summary.added, 2);
    assert_eq!(summary.updated, 0);
    assert_eq!(summary.rejected, 0);

    let indexed = models(&store.store).await;
    assert_eq!(indexed.len(), 2);
    let first = model_by_key(&store.store, "first")
        .await
        .expect("first row");
    assert_eq!(first.artifact_kind, ArtifactKind::Gguf);
    assert_eq!(first.display_name.as_deref(), Some("First Model"));
    assert_eq!(
        first.size_bytes,
        fake_gguf("First Model", &[0u8; 16]).len() as u64
    );
    assert!(!first.deleted);

    // Root bookkeeping is written in the same transaction.
    let reloaded = ModelRootsRepo::get(store.store.pool(), "root-1")
        .await
        .expect("load root")
        .expect("root exists");
    assert!(reloaded.last_scan_at.is_some());
    assert_eq!(
        reloaded
            .last_scan_result
            .as_ref()
            .map(|value| value["added"] == 2),
        Some(true)
    );
    assert_eq!(
        reloaded.path, record.path,
        "the scan never edits the root path"
    );
}

#[tokio::test]
async fn restart_keeps_id_and_key_stable() {
    let store = store().await;
    let root = Root::new();
    root.write("stable.gguf", b"content");
    register_root(&store.store, root.path(), "root-1").await;

    let first = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 1");
    let row = model_by_key(&store.store, "stable").await.expect("row");
    let id_before = row.id.clone();

    // Simulate a daemon restart: a brand-new service over the same ledger.
    let second = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 2");
    assert_eq!(second.added, 0, "restart must not re-add");
    assert_eq!(second.unchanged, 1);
    assert_eq!(second.updated, 0);
    assert_eq!(first.root_id, second.root_id);

    let after = model_by_key(&store.store, "stable").await.expect("row");
    assert_eq!(after.id, id_before, "the row identity is stable");
    assert_eq!(after.key, "stable");
    assert_eq!(
        models(&store.store).await.len(),
        1,
        "no duplicate row after restart"
    );
}

#[tokio::test]
async fn unchanged_file_is_not_rewritten_and_admin_fields_survive() {
    let store = store().await;
    let root = Root::new();
    root.write("admin.gguf", b"content");
    register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan");

    // An administrator edits the row through the CRUD path. The runtime must
    // exist first: `default_runtime_id` is a foreign key into `runtimes`.
    register_runtime(&store.store, "runtime-1").await;
    let mut edit = model_by_key(&store.store, "admin").await.expect("row");
    edit.display_name = Some("Admin Renamed".to_owned());
    edit.metadata = Some(serde_json::json!({ "owner": "ops" }));
    edit.default_runtime_id = Some("runtime-1".to_owned());
    ModelsRepo::upsert(store.store.pool(), &edit)
        .await
        .expect("admin upsert");

    let summary = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 2");
    assert_eq!(summary.unchanged, 1);
    assert_eq!(summary.updated, 0);

    let after = model_by_key(&store.store, "admin").await.expect("row");
    assert_eq!(after.display_name.as_deref(), Some("Admin Renamed"));
    assert_eq!(after.default_runtime_id.as_deref(), Some("runtime-1"));
    assert_eq!(after.metadata, Some(serde_json::json!({ "owner": "ops" })));
}

#[tokio::test]
async fn changed_size_is_reported_as_updated() {
    let store = store().await;
    let root = Root::new();
    let path = root.write("grow.gguf", b"small");
    register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 1");

    std::fs::write(&path, b"a much larger payload").expect("grow artifact");
    touch(Path::new(&path), 1_000);

    let summary = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 2");
    assert_eq!(summary.updated, 1);
    assert_eq!(summary.unchanged, 0);
    assert_eq!(summary.added, 0);
    let after = model_by_key(&store.store, "grow").await.expect("row");
    assert_eq!(after.size_bytes, b"a much larger payload".len() as u64);
}

#[tokio::test]
async fn touching_mtime_only_is_reported_as_updated() {
    let store = store().await;
    let root = Root::new();
    let path = root.write("touched.gguf", b"same size");
    register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 1");
    let before = model_by_key(&store.store, "touched")
        .await
        .expect("row")
        .mtime;

    touch(Path::new(&path), 1_000);
    let summary = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 2");
    assert_eq!(summary.updated, 1);
    let after = model_by_key(&store.store, "touched").await.expect("row");
    assert!(
        after.mtime > before,
        "mtime advanced: {} > {before}",
        after.mtime
    );
    assert_eq!(after.size_bytes, b"same size".len() as u64);
}

#[tokio::test]
async fn deleted_artifact_is_marked_and_reappearance_restores_the_same_row() {
    let store = store().await;
    let root = Root::new();
    let path = root.write("gone.gguf", b"payload");
    register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 1");
    let original = model_by_key(&store.store, "gone").await.expect("row");

    std::fs::remove_file(&path).expect("delete artifact");
    let summary = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 2");
    assert_eq!(summary.removed, 1);
    assert_eq!(summary.files_seen, 0);

    let marked = model_by_key(&store.store, "gone").await.expect("row kept");
    assert!(marked.deleted, "a missing artifact is marked, not dropped");
    assert_eq!(marked.id, original.id);

    root.write("gone.gguf", b"payload again");
    let summary = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 3");
    assert_eq!(summary.restored, 1);
    assert_eq!(
        summary.added, 0,
        "reappearance must not create a second row"
    );

    let restored = model_by_key(&store.store, "gone").await.expect("row");
    assert!(!restored.deleted);
    assert_eq!(restored.id, original.id, "same row, same id");
    assert_eq!(restored.key, original.key);
}

#[tokio::test]
async fn scan_emits_audit_events_for_each_change_class() {
    let store = store().await;
    let root = Root::new();
    let path = root.write("audited.gguf", b"one");
    register_root(&store.store, root.path(), "root-1").await;

    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 1");
    std::fs::remove_file(&path).expect("delete");
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 2");
    root.write("audited.gguf", b"two");
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 3");

    let events = AuditRepo::list(store.store.pool(), None)
        .await
        .expect("list audit");
    let kinds: Vec<String> = events
        .iter()
        .map(|event| model_serving_persistence::wire_token(&event.kind))
        .collect();
    assert!(
        kinds.iter().any(|kind| kind == "model_upserted"),
        "{kinds:?}"
    );
    assert!(
        kinds.iter().any(|kind| kind == "model_deleted"),
        "{kinds:?}"
    );
    assert!(
        kinds.iter().any(|kind| kind == "model_restored"),
        "{kinds:?}"
    );
}

#[tokio::test]
async fn collision_resolution_is_stable_across_scans_and_collisions_share_a_base() {
    let store = store().await;
    let root = Root::new();
    root.write("a/shared.gguf", b"left");
    root.write("b/shared.gguf", b"right");
    register_root(&store.store, root.path(), "root-1").await;

    let first = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 1");
    assert_eq!(first.added, 2);
    let rows = models(&store.store).await;
    let mut keys: Vec<String> = rows.iter().map(|row| row.key.clone()).collect();
    keys.sort();
    assert_eq!(keys, vec!["shared-1", "shared-2"]);
    let ids: HashMap<String, String> = rows
        .iter()
        .map(|row| (row.path.clone(), row.id.clone()))
        .collect();

    let second = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 2");
    assert_eq!(second.unchanged, 2, "a collision must not churn");
    assert_eq!(second.added, 0);
    assert_eq!(second.updated, 0);
    for row in models(&store.store).await {
        assert_eq!(row.id, ids[&row.path], "collision identity is stable");
    }
    assert_eq!(models(&store.store).await.len(), 2, "no duplicate keys");

    // The collision keys are also confirmed to be order-independent by the
    // pure key module (see `collision_order_does_not_change_keys`).
}

#[tokio::test]
async fn adding_a_new_file_cannot_steal_a_reserved_key() {
    let store = store().await;
    let root = Root::new();
    root.write("a/shared.gguf", b"left");
    register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 1");
    let existing = model_by_key(&store.store, "shared").await.expect("row");

    root.write("b/shared.gguf", b"right");
    let summary = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 2");
    assert_eq!(summary.added, 1);

    let untouched = model_by_key(&store.store, "shared")
        .await
        .expect("row still there");
    assert_eq!(untouched.id, existing.id);
    let newcomer = model_by_key(&store.store, "shared-1")
        .await
        .expect("suffixed row");
    assert_ne!(newcomer.id, existing.id);
    assert_eq!(models(&store.store).await.len(), 2);
}

#[tokio::test]
async fn scan_only_touches_the_scanned_root() {
    let store = store().await;
    let root_a = Root::new();
    let root_b = Root::new();
    root_a.write("a-only.gguf", b"a");
    root_b.write("b-only.gguf", b"b");
    register_root(&store.store, root_a.path(), "root-a").await;
    register_root(&store.store, root_b.path(), "root-b").await;

    ScanService::new(&store.store)
        .scan_all_roots()
        .await
        .expect("scan all");
    assert_eq!(models(&store.store).await.len(), 2);

    std::fs::remove_file(root_b.path().join("b-only.gguf")).expect("delete b artifact");
    let summary = ScanService::new(&store.store)
        .scan_root("root-a")
        .await
        .expect("scan a");
    assert_eq!(summary.removed, 0, "root-a must not touch root-b rows");
    let untouched = model_by_key(&store.store, "b-only").await.expect("b row");
    assert!(!untouched.deleted);

    let summary = ScanService::new(&store.store)
        .scan_root("root-b")
        .await
        .expect("scan b");
    assert_eq!(summary.removed, 1);
    assert!(
        model_by_key(&store.store, "b-only")
            .await
            .expect("b row")
            .deleted
    );
}

#[tokio::test]
async fn disabled_root_is_rejected() {
    let store = store().await;
    let root = Root::new();
    let mut record = register_root(&store.store, root.path(), "root-1").await;
    record.enabled = false;
    ModelRootsRepo::upsert(store.store.pool(), &record)
        .await
        .expect("disable root");

    let error = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect_err("disabled root");
    assert_eq!(error.code, ErrorCode::InvalidRequest);

    assert!(
        ScanService::new(&store.store)
            .scan_all_roots()
            .await
            .expect("scan all")
            .is_empty()
    );
}

#[tokio::test]
async fn unknown_root_id_is_not_found() {
    let store = store().await;
    let error = ScanService::new(&store.store)
        .scan_root("missing-root")
        .await
        .expect_err("unknown root");
    assert_eq!(error.code, ErrorCode::ModelNotFound);
}

#[tokio::test]
async fn missing_root_directory_is_rejected_before_any_write() {
    let store = store().await;
    let root = Root::new();
    register_root(&store.store, root.path(), "root-1").await;
    std::fs::remove_dir_all(root.path()).expect("remove root");

    let error = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect_err("missing root");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(models(&store.store).await.is_empty());
    let record = ModelRootsRepo::get(store.store.pool(), "root-1")
        .await
        .expect("load root")
        .expect("root");
    assert!(
        record.last_scan_at.is_none(),
        "no bookkeeping write on failure"
    );
}

// --------------------------------------------------------------------------
// rollback
// --------------------------------------------------------------------------

#[tokio::test]
async fn rollback_on_root_write_failure_leaves_the_index_untouched() {
    let store = store().await;
    let root = Root::new();
    root.write("first.gguf", b"one");
    root.write("second.gguf", b"two");
    let record = register_root(&store.store, root.path(), "root-1").await;

    // Drop the root row behind the service's back: validation and the model
    // writes still run, but the root bookkeeping UPDATE affects 0 rows and the
    // transaction must roll the model upserts back.
    ModelRootsRepo::delete(store.store.pool(), &record.id)
        .await
        .expect("delete root");
    // Re-register a *stale* record to drive the scan (the row no longer exists,
    // so the bookkeeping write fails).
    let error = scan_with_stale_root(&store.store, &record)
        .await
        .expect_err("rollback");

    // The vanished root row breaks the `models.root_id` foreign key, so the
    // scan fails on the referential write and the storage layer classifies it
    // as a rejected request.
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(
        models(&store.store).await.is_empty(),
        "a failed reconciliation must not leave partial model rows"
    );
    let events = AuditRepo::count(store.store.pool())
        .await
        .expect("audit count");
    assert_eq!(events, 0, "audit events roll back with the transaction");
}

/// Drive a scan with a root record the ledger no longer contains.
async fn scan_with_stale_root(
    store: &SqliteStore,
    stale: &ModelRoot,
) -> error::Result<ScanSummary> {
    let canonical = validate_root(Path::new(&stale.path))?;
    let scanned = walk::scan_tree(&canonical, ScanOptions::default())?;
    let occupied = BTreeSet::new();
    let bookkeeping = bookkeeping_for(
        &scanned.files,
        &HashMap::new(),
        &occupied,
        scanned.discarded,
    );
    persistence::ScanWrite { store }
        .record_scan(stale, &canonical, &bookkeeping, scanned.skipped)
        .await
}

#[tokio::test]
async fn failed_scan_keeps_previous_index_rows() {
    let store = store().await;
    let root = Root::new();
    root.write("kept.gguf", b"value");
    let record = register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 1");
    let before = model_by_key(&store.store, "kept").await.expect("row");
    let scans_before = ModelRootsRepo::get(store.store.pool(), "root-1")
        .await
        .expect("load root")
        .expect("root")
        .last_scan_at;

    ModelRootsRepo::delete(store.store.pool(), &record.id)
        .await
        .expect("delete root");
    let error = scan_with_stale_root(&store.store, &record)
        .await
        .expect_err("scan must fail");
    assert_eq!(error.code, ErrorCode::Internal);

    let after = model_by_key(&store.store, "kept")
        .await
        .expect("row survived");
    assert_eq!(after.id, before.id);
    assert_eq!(after.size_bytes, before.size_bytes);
    let _ = scans_before;
}

// --------------------------------------------------------------------------
// pure bookkeeping assembly
// --------------------------------------------------------------------------

#[test]
fn bookkeeping_preserves_identity_for_a_known_path() {
    let path = "/models/known.gguf".to_owned();
    let files = vec![scanned(&path)];
    let mut states = HashMap::new();
    states.insert(
        "admin-key".to_owned(),
        PersistenceState {
            id: "row-id".to_owned(),
            path: path.clone(),
            size_bytes: 7,
            mtime: Utc::now(),
            deleted: false,
        },
    );

    let occupied: BTreeSet<String> = states.keys().cloned().collect();
    let bookkeeping = bookkeeping_for(&files, &states, &occupied, 3);
    assert_eq!(bookkeeping.rejected, 3);
    assert_eq!(bookkeeping.entries.len(), 1);
    assert_eq!(bookkeeping.entries[0].id, "row-id");
    assert_eq!(
        bookkeeping.entries[0].key, "admin-key",
        "an admin-overridden key is never regenerated"
    );
    assert!(bookkeeping.entries[0].existing.is_some());
    assert!(bookkeeping.reserved.contains("admin-key"));
}

#[test]
fn bookkeeping_marks_an_existing_path_as_unchanged_only_when_stat_matches() {
    let mtime = Utc::now();
    let existing = Existing {
        size_bytes: 10,
        mtime,
        deleted: false,
    };
    assert!(is_unchanged(&existing, 10, mtime));
    assert!(!is_unchanged(&existing, 11, mtime));
    assert!(!is_unchanged(&existing, 10, mtime + TimeDelta::seconds(1)));
    assert!(!is_unchanged(
        &Existing {
            deleted: true,
            ..existing
        },
        10,
        mtime
    ));
}

#[test]
fn base_key_groups_exposes_collision_candidates() {
    let files = vec![
        scanned("/models/a/shared.gguf"),
        scanned("/models/b/shared.gguf"),
        scanned("/models/unique.gguf"),
    ];
    let groups = base_key_groups(&files);
    assert_eq!(groups["shared"].len(), 2);
    assert_eq!(groups["unique"].len(), 1);
}

// --------------------------------------------------------------------------
// compile-time-ish guards
// --------------------------------------------------------------------------

#[test]
fn scan_summary_round_trips_through_json() {
    let summary = ScanSummary {
        root_id: "root-1".to_owned(),
        root_path: "/models".to_owned(),
        scanned_at: Utc::now(),
        duration_ms: 12,
        files_seen: 3,
        added: 1,
        updated: 1,
        unchanged: 1,
        restored: 0,
        removed: 2,
        skipped: 4,
        rejected: 5,
    };
    let json = serde_json::to_string(&summary).expect("serialize");
    let parsed: ScanSummary = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, summary);
}

#[test]
fn root_with_bookkeeping_only_sets_scan_fields() {
    let root = ModelRoot {
        id: "root-1".to_owned(),
        path: "/models".to_owned(),
        enabled: true,
        last_scan_at: None,
        last_scan_result: None,
    };
    let summary = ScanSummary {
        root_id: "root-1".to_owned(),
        root_path: "/models".to_owned(),
        scanned_at: Utc::now(),
        duration_ms: 0,
        files_seen: 0,
        added: 0,
        updated: 0,
        unchanged: 0,
        restored: 0,
        removed: 0,
        skipped: 0,
        rejected: 0,
    };
    let updated = root_with_bookkeeping(&root, Utc::now(), &summary);
    assert_eq!(updated.path, root.path);
    assert_eq!(updated.enabled, root.enabled);
    assert!(updated.last_scan_at.is_some());
    assert!(updated.last_scan_result.is_some());
}

/// A test-side `touch`: rewrites the mtime without touching content, using no
/// unsafe code and no extra crate. `millis` moves the timestamp later from
/// "now", so the scan's `mtime` comparison is guaranteed to notice.
fn touch(path: &Path, millis: u64) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for touch");
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_millis(millis))
        .expect("set mtime");
}

// --------------------------------------------------------------------------
// review round 1 regressions
// --------------------------------------------------------------------------

/// P1-A: an inspection failure must abort the scan **before** any database
/// write, so a partial view of the tree can never mark previously indexed
/// artifacts deleted.
///
/// Driven through [`ScanOptions::fail_on_path_containing`] rather than real
/// permission bits, because an elevated Windows account ignores them.
#[tokio::test]
async fn incomplete_scan_aborts_and_leaves_index_and_bookkeeping_untouched() {
    let store = store().await;
    let root = Root::new();
    root.write("keep.gguf", b"one");
    root.write("nested/deep.gguf", b"two");
    register_root(&store.store, root.path(), "root-1").await;

    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("baseline scan");
    let before = models(&store.store).await;
    assert_eq!(before.len(), 2, "both artifacts indexed");
    let root_before = ModelRootsRepo::get(store.store.pool(), "root-1")
        .await
        .expect("load root")
        .expect("root");
    assert!(
        root_before.last_scan_at.is_some(),
        "bookkeeping was written"
    );

    // The walk now fails when it inspects the `nested` directory. Without the
    // fail-closed fix this was reported as `skipped`, and the sweep would have
    // marked `nested/deep.gguf` deleted even though nothing changed on disk.
    let failing = ScanService::with_options(
        &store.store,
        ScanOptions {
            fail_on_path_containing: Some("nested"),
            ..ScanOptions::default()
        },
    );
    let error = failing
        .scan_root("root-1")
        .await
        .expect_err("an incomplete scan must fail");
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");

    let after = models(&store.store).await;
    assert_eq!(after, before, "no model row changed");
    assert!(
        after.iter().all(|model| !model.deleted),
        "nothing was marked deleted: {after:?}"
    );
    let root_after = ModelRootsRepo::get(store.store.pool(), "root-1")
        .await
        .expect("load root")
        .expect("root");
    assert_eq!(
        root_after.last_scan_at, root_before.last_scan_at,
        "the root bookkeeping is untouched"
    );
    assert_eq!(root_after.last_scan_result, root_before.last_scan_result);
}

/// P1-A (walk level): the failure is a real `Internal` error, not a counter.
#[test]
fn walk_reports_an_inspection_failure_as_internal() {
    let root = Root::new();
    root.write("ok.gguf", b"ok");
    root.mkdir("victim");
    root.write("victim/inside.gguf", b"inside");

    let error = scan_tree(
        &root.canonical(),
        ScanOptions {
            fail_on_path_containing: Some("victim"),
            ..ScanOptions::default()
        },
    )
    .expect_err("injection must abort the walk");
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");
}

/// P1-B: a soft-deleted row owned by **another** root still occupies its
/// `UNIQUE` key. A new artifact of this root must not adopt that key (which
/// would re-home the unrelated row and inherit its `id` / admin fields).
#[tokio::test]
async fn cross_root_soft_deleted_key_is_not_hijacked() {
    let store = store().await;
    let root_a = Root::new();
    let root_b = Root::new();
    register_root(&store.store, root_a.path(), "root-a").await;
    register_root(&store.store, root_b.path(), "root-b").await;

    // Root A indexes `shared.gguf`, then an administrator edits the row.
    let a_path = root_a.write("shared.gguf", b"from root a");
    ScanService::new(&store.store)
        .scan_root("root-a")
        .await
        .expect("scan a");
    register_runtime(&store.store, "runtime-a").await;
    let mut admin = model_by_key(&store.store, "shared").await.expect("row");
    admin.display_name = Some("Admin Owned".to_owned());
    admin.metadata = Some(serde_json::json!({ "owner": "a" }));
    admin.default_runtime_id = Some("runtime-a".to_owned());
    ModelsRepo::upsert(store.store.pool(), &admin)
        .await
        .expect("admin upsert");
    let admin_id = admin.id.clone();

    // The artifact disappears and the scan soft-deletes the row. The key stays
    // occupied in `models`.
    std::fs::remove_file(&a_path).expect("remove a artifact");
    ScanService::new(&store.store)
        .scan_root("root-a")
        .await
        .expect("scan a again");
    let deleted = model_by_key(&store.store, "shared")
        .await
        .expect("row kept");
    assert!(deleted.deleted);

    // Root B now gains a *different* artifact with the same base key.
    root_b.write("shared.gguf", b"from root b");
    let summary = ScanService::new(&store.store)
        .scan_root("root-b")
        .await
        .expect("scan b");
    assert_eq!(summary.added, 1, "the new artifact is added, not merged");

    // Root A's row is untouched: same id, still deleted, admin fields intact.
    let a_row = model_by_key(&store.store, "shared").await.expect("row");
    assert_eq!(a_row.id, admin_id, "root A keeps its identity");
    assert_eq!(a_row.display_name.as_deref(), Some("Admin Owned"));
    assert_eq!(a_row.metadata, Some(serde_json::json!({ "owner": "a" })));
    assert_eq!(a_row.default_runtime_id.as_deref(), Some("runtime-a"));

    // Root B's artifact owns a distinct key and a distinct row.
    let b_row = model_by_key(&store.store, "shared-1")
        .await
        .expect("suffixed");
    assert_ne!(b_row.id, admin_id, "separate rows");
    assert_eq!(
        b_row.path,
        canonical_string(Path::new(&root_b.write_path("shared.gguf")))
    );
    assert_eq!(models(&store.store).await.len(), 2, "two rows, no merge");
}

/// P2: the summary persisted into `model_roots.last_scan_result` must equal the
/// summary returned to the caller — including `duration_ms`, which used to be
/// stored as `0` and only filled in after the transaction committed.
#[tokio::test]
async fn stored_scan_summary_matches_the_returned_summary() {
    let store = store().await;
    let root = Root::new();
    root.write("one.gguf", b"one");
    root.write("two.gguf", b"two");
    register_root(&store.store, root.path(), "root-1").await;

    let returned = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan");

    let record = ModelRootsRepo::get(store.store.pool(), "root-1")
        .await
        .expect("load root")
        .expect("root");
    let stored = record.last_scan_result.expect("scan result stored");
    let stored: ScanSummary = serde_json::from_value(stored).expect("stored summary deserializes");

    assert_eq!(
        stored, returned,
        "the stored summary must be the returned summary"
    );
    assert_eq!(stored.scanned_at, returned.scanned_at);
}

/// P2: the incremental path (unchanged files, no adds/updates) also persists the
/// summary it returns.
#[tokio::test]
async fn stored_scan_summary_matches_on_an_unchanged_rescan() {
    let store = store().await;
    let root = Root::new();
    root.write("stable.gguf", b"content");
    register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 1");

    let returned = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan 2");
    assert_eq!(returned.unchanged, 1);
    assert_eq!(returned.added, 0);

    let record = ModelRootsRepo::get(store.store.pool(), "root-1")
        .await
        .expect("load root")
        .expect("root");
    let stored: ScanSummary =
        serde_json::from_value(record.last_scan_result.expect("stored")).expect("deserializes");
    assert_eq!(stored, returned);
}

// --------------------------------------------------------------------------
// review round 2 regressions
// --------------------------------------------------------------------------

/// P1: `models.root_id` is nullable (`REFERENCES model_roots (id) ON DELETE SET
/// NULL`) and `ModelsRepo::upsert` never writes it, so an administrator-created
/// row legitimately carries `NULL`. Decoding that column as `String` used to
/// fail the entire scan with `Internal`.
#[tokio::test]
async fn unowned_null_root_id_row_does_not_break_the_scan() {
    let store = store().await;
    let root = Root::new();
    root.write("indexed.gguf", b"scanned");
    register_root(&store.store, root.path(), "root-1").await;

    // A standalone artifact added through the repository, with no scan root.
    let unowned = Model {
        id: "unowned-model".to_owned(),
        key: "manual-entry".to_owned(),
        // A path that does not exist on this host: the row is a standalone
        // artifact that no scan root will ever traverse.
        path: "/nonexistent/manual-entry.gguf".to_owned(),
        display_name: Some("Manually Added".to_owned()),
        artifact_kind: ArtifactKind::Gguf,
        size_bytes: 42,
        mtime: Utc::now(),
        default_runtime_id: None,
        default_load_config: None,
        metadata: Some(serde_json::json!({ "source": "admin" })),
        deleted: false,
    };
    ModelsRepo::upsert(store.store.pool(), &unowned)
        .await
        .expect("insert unowned row");
    // Confirm the premise: the column really is `NULL`, not a placeholder.
    let stored_root: Option<String> =
        sqlx::query_scalar("SELECT root_id FROM models WHERE key = 'manual-entry'")
            .fetch_one(store.store.pool())
            .await
            .expect("read root_id");
    assert_eq!(stored_root, None, "the row is unowned");

    // The scan of the configured root must succeed rather than fail `Internal`.
    let summary = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("a NULL root_id must not fail the scan");
    assert_eq!(summary.added, 1);
    assert_eq!(
        summary.removed, 0,
        "the unowned row is not this root's to sweep"
    );

    // The unowned row is preserved untouched, and separated from the scan.
    let after = model_by_key(&store.store, "manual-entry")
        .await
        .expect("unowned row kept");
    assert_eq!(after.id, unowned.id);
    assert_eq!(after.display_name.as_deref(), Some("Manually Added"));
    assert_eq!(
        after.metadata,
        Some(serde_json::json!({ "source": "admin" }))
    );
    assert!(!after.deleted);
    assert_eq!(models(&store.store).await.len(), 2, "two separate rows");
}

/// P1 (eligibility): an unowned row that is *live* and sits at the recorded path
/// is adopted by the scan — same `id`, no duplicate — while an unowned row that
/// is already soft-deleted is never resurrected, because nothing records which
/// root deleted it.
#[tokio::test]
async fn unowned_rows_are_adopted_when_live_but_never_resurrected() {
    let store = store().await;
    let root = Root::new();
    let path = root.write("adopt.gguf", b"content");
    register_root(&store.store, root.path(), "root-1").await;

    // A live unowned row whose path matches what the scan will find.
    let adopted = Model {
        id: "adoptable".to_owned(),
        key: "adopt".to_owned(),
        path: path.clone(),
        display_name: Some("Pre-existing".to_owned()),
        artifact_kind: ArtifactKind::Gguf,
        size_bytes: 7,
        mtime: Utc::now(),
        default_runtime_id: None,
        default_load_config: None,
        metadata: None,
        deleted: false,
    };
    ModelsRepo::upsert(store.store.pool(), &adopted)
        .await
        .expect("insert adoptable row");

    // A soft-deleted unowned row whose path matches a file that *is* present on
    // disk, so the walk will find it and match this row by path. That is
    // exactly what makes the test load-bearing: if a deleted row with no owner
    // were treated as this root's, the update path would run, set
    // `deleted = 0` and stamp `root_id = root-1`, silently resurrecting an
    // artifact another root had deleted. Because the row is unowned and
    // deleted, the scan must instead leave it alone and index the file as a
    // genuinely new artifact.
    let orphan_path = root.write("orphan.gguf", b"orphan contents");
    let mut orphan = adopted.clone();
    orphan.id = "orphan".to_owned();
    orphan.key = "orphan".to_owned();
    orphan.path = orphan_path.clone();
    orphan.deleted = true;
    ModelsRepo::upsert(store.store.pool(), &orphan)
        .await
        .expect("insert orphan row");

    let summary = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan");
    // `adopt.gguf` matches the live unowned row by path (updated in place, not
    // duplicated). `orphan.gguf` is present on disk but its only matching row is
    // unowned and already deleted, so it is indexed as a brand-new artifact
    // rather than resurrecting that row.
    assert_eq!(summary.added, 1, "the orphan file is a new artifact");
    assert_eq!(summary.updated, 1, "only the live unowned row is adopted");
    assert_eq!(summary.removed, 0);
    assert_eq!(summary.files_seen, 2);

    let adopted_after = model_by_key(&store.store, "adopt")
        .await
        .expect("adopted row");
    assert_eq!(adopted_after.id, "adoptable", "identity preserved");
    assert!(!adopted_after.deleted);
    // The live unowned row is adopted by this scan, which records its ownership
    // so later scans maintain it like any other indexed artifact.
    assert_eq!(adopted_after.path, path);
    let adopted_owner: Option<String> =
        sqlx::query_scalar("SELECT root_id FROM models WHERE id = 'adoptable'")
            .fetch_one(store.store.pool())
            .await
            .expect("read owner");
    assert_eq!(
        adopted_owner.as_deref(),
        Some("root-1"),
        "adopted by the scan"
    );

    // The unowned, soft-deleted orphan stays deleted and stays unowned: this
    // root did not own it, so it is not this root's to maintain or restore.
    let orphan_after = model_by_key(&store.store, "orphan")
        .await
        .expect("orphan row kept");
    assert!(
        orphan_after.deleted,
        "never resurrected by an unrelated scan"
    );
    assert_eq!(orphan_after.id, "orphan", "still the original row");
    let orphan_owner: Option<String> =
        sqlx::query_scalar("SELECT root_id FROM models WHERE id = 'orphan'")
            .fetch_one(store.store.pool())
            .await
            .expect("read owner");
    assert_eq!(orphan_owner, None, "still unowned");

    // A third row holds the file that was actually indexed. Its key is a fresh
    // one, because the orphan row already occupies the base key `orphan`.
    let fresh = model_by_key(&store.store, "orphan-1")
        .await
        .expect("the file was indexed as a new artifact");
    assert!(!fresh.deleted);
    assert_eq!(fresh.path, orphan_path);
    assert_ne!(
        fresh.id, "orphan",
        "a distinct identity from the deleted row"
    );
    assert_eq!(models(&store.store).await.len(), 3);
}

/// P1 (acceptance gap): a file mtime with sub-millisecond precision must not
/// churn the artifact as `Updated` on every rescan, because `models.mtime` is
/// persisted with millisecond precision only.
#[test]
fn sub_millisecond_mtime_normalizes_to_persistence_precision() {
    // Direct unit test, so the behaviour is covered on every platform even
    // where the filesystem cannot preserve sub-millisecond stamps.
    let raw = DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("timestamp");
    let normalized = normalize_mtime(raw);
    assert_eq!(normalized.timestamp_millis(), raw.timestamp_millis());
    assert_eq!(
        normalized.timestamp_subsec_nanos() % 1_000_000,
        0,
        "ms only"
    );
    // Normalisation is idempotent, which is what makes the comparison stable.
    assert_eq!(normalize_mtime(normalized), normalized);
    // And it agrees with the serialized form the database stores.
    assert_eq!(
        model_serving_persistence::ts_string(normalized),
        model_serving_persistence::ts_string(raw)
    );
}

/// P1 (acceptance gap, end to end): write a file whose mtime has real
/// sub-millisecond precision, scan twice, and require the second scan to report
/// it unchanged.
#[tokio::test]
async fn sub_millisecond_mtime_does_not_churn_across_scans() {
    let store = store().await;
    let root = Root::new();
    let path = PathBuf::from(root.write("precise.ninfer", b"precise contents"));
    register_root(&store.store, root.path(), "root-1").await;

    // A whole-second instant plus a sub-millisecond remainder. Windows stores
    // 100 ns ticks and Linux stores nanoseconds, so the remainder survives on
    // both; if a filesystem rounds it away the assertion below is still valid
    // (it just cannot demonstrate the churn it prevents).
    let precise = std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::from_secs(1_700_000_000)
        + std::time::Duration::from_micros(1_234_567);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open artifact");
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(precise)
            .set_accessed(precise),
    )
    .expect("set precise mtime");
    drop(file);

    let first = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("first scan");
    assert_eq!(first.added, 1);

    let second = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("second scan");
    assert_eq!(
        second.updated, 0,
        "a sub-millisecond mtime must not churn as Updated"
    );
    assert_eq!(second.unchanged, 1, "the rescan reports it unchanged");

    // A third scan, to be sure the state converges rather than oscillating.
    let third = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("third scan");
    assert_eq!(third.updated, 0);
    assert_eq!(third.unchanged, 1);
}

/// P2: an indexed artifact whose key an administrator renamed away from its base
/// key must not participate in key allocation at all — its row keeps its own
/// key, and the freed base key is available to a genuinely new artifact.
#[tokio::test]
async fn admin_renamed_row_does_not_consume_a_suffix_for_a_new_same_base_artifact() {
    let store = store().await;
    let root = Root::new();
    root.write("shared.gguf", b"original");
    register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("baseline scan");

    // The administrator renames the row's key, freeing the base key `shared`.
    // A rename is an in-place UPDATE (the row keeps its `id`), not an upsert:
    // `ModelsRepo::upsert` conflicts on `key` and would try to insert a second
    // row with the same `id`.
    let renamed = model_by_key(&store.store, "shared").await.expect("row");
    let renamed_id = renamed.id.clone();
    let renamed_path = renamed.path.clone();
    sqlx::query("UPDATE models SET key = 'custom-name' WHERE id = ?1")
        .bind(&renamed_id)
        .execute(store.store.pool())
        .await
        .expect("admin rename");

    // A *different* new artifact appears with the same base key.
    root.write("nested/shared.gguf", b"newcomer");
    let summary = ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("scan after rename");
    assert_eq!(summary.added, 1);

    // The newcomer takes the free base key itself, not `shared-1`.
    let newcomer = model_by_key(&store.store, "shared")
        .await
        .expect("newcomer took the free base key");
    assert_ne!(newcomer.path, renamed_path, "a distinct artifact");
    assert!(
        newcomer.path.contains("nested"),
        "the newcomer is the nested file: {}",
        newcomer.path
    );
    assert!(
        model_by_key(&store.store, "shared-1").await.is_none(),
        "no suffix was burned"
    );

    // The admin-renamed row is untouched: same id, same key.
    let still = model_by_key(&store.store, "custom-name")
        .await
        .expect("admin key kept");
    assert_eq!(still.id, renamed_id, "the admin row keeps its identity");
    assert!(!still.deleted);
    assert_eq!(models(&store.store).await.len(), 2);
}

/// P1 (budget): a depth budget omits a subtree from the walk, which is
/// incomplete traversal rather than a safe skip. Counting the omitted subtree
/// and continuing would let the authoritative sweep delete the artifacts under
/// it. The scan must fail closed and leave both the index and the root
/// bookkeeping byte-identical.
#[tokio::test]
async fn depth_budget_aborts_before_any_write_and_preserves_the_index() {
    let store = store().await;
    let root = Root::new();
    root.write("shallow.gguf", b"shallow");
    root.write("nested/deep.gguf", b"deep");
    root.write("nested/deeper/deepest.gguf", b"deepest");
    register_root(&store.store, root.path(), "root-1").await;

    // Baseline with a permissive budget: all three artifacts are indexed.
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("baseline scan");
    let before = models(&store.store).await;
    assert_eq!(before.len(), 3);
    let root_before = root_record(&store.store, "root-1").await;

    // A tighter depth now cannot reach `nested/deeper/deepest.gguf`. Without
    // the fail-closed fix this was a `skipped` count and the sweep marked that
    // artifact deleted even though it still exists on disk.
    let tight = ScanService::with_options(
        &store.store,
        ScanOptions {
            max_depth: 1,
            ..ScanOptions::default()
        },
    );
    let error = tight
        .scan_root("root-1")
        .await
        .expect_err("an incomplete scan must fail");
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");

    let after = models(&store.store).await;
    assert_eq!(after, before, "no model row changed");
    assert!(
        after.iter().all(|model| !model.deleted),
        "nothing was marked deleted: {after:?}"
    );
    let root_after = root_record(&store.store, "root-1").await;
    assert_eq!(
        root_after.last_scan_at, root_before.last_scan_at,
        "root bookkeeping is untouched"
    );
    assert_eq!(root_after.last_scan_result, root_before.last_scan_result);
}

/// P1 (budget): the same for the directory budget, which bounds how many
/// directories the walk will descend into.
#[tokio::test]
async fn directory_budget_aborts_before_any_write_and_preserves_the_index() {
    let store = store().await;
    let root = Root::new();
    root.write("a/one.gguf", b"one");
    root.write("b/two.gguf", b"two");
    root.write("c/three.gguf", b"three");
    register_root(&store.store, root.path(), "root-1").await;

    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("baseline scan");
    let before = models(&store.store).await;
    assert_eq!(before.len(), 3);
    let root_before = root_record(&store.store, "root-1").await;

    // The root plus three subdirectories: a budget of 2 cannot visit them all.
    let tight = ScanService::with_options(
        &store.store,
        ScanOptions {
            max_directories: 2,
            ..ScanOptions::default()
        },
    );
    let error = tight
        .scan_root("root-1")
        .await
        .expect_err("an incomplete scan must fail");
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");

    let after = models(&store.store).await;
    assert_eq!(after, before, "no model row changed");
    assert!(after.iter().all(|model| !model.deleted));
    let root_after = root_record(&store.store, "root-1").await;
    assert_eq!(root_after.last_scan_at, root_before.last_scan_at);
    assert_eq!(root_after.last_scan_result, root_before.last_scan_result);
}

/// P1 (budget): the walk itself fails closed, with an `Internal` code, rather
/// than reporting the omitted subtree as skipped.
#[test]
fn walk_aborts_when_a_budget_is_exceeded() {
    let root = Root::new();
    root.write("nested/deep.gguf", b"deep");

    let depth_error = scan_tree(
        &root.canonical(),
        ScanOptions {
            max_depth: 0,
            ..ScanOptions::default()
        },
    )
    .expect_err("an unreachable subtree must abort the walk");
    assert_eq!(depth_error.code, ErrorCode::Internal, "{depth_error:?}");

    root.mkdir("also");
    let dir_error = scan_tree(
        &root.canonical(),
        ScanOptions {
            max_directories: 1,
            ..ScanOptions::default()
        },
    )
    .expect_err("an unvisited directory must abort the walk");
    assert_eq!(dir_error.code, ErrorCode::Internal, "{dir_error:?}");
}

/// P1 (stale snapshot): a scan whose loaded root predates a newer committed scan
/// must fail at the compare-and-swap and roll back, instead of overwriting the
/// newer scan's bookkeeping.
///
/// The interleaving is constructed deterministically — no scheduling luck: the
/// stale snapshot is built first, a newer scan commits, and only then is the
/// stale `record_scan` invoked.
///
/// This isolates the CAS specifically. The stale bookkeeping describes exactly
/// the same artifacts the newer scan indexed, so every per-row step is a
/// legitimate no-op that *would succeed*: the identity preflight passes, the
/// updates match their rows, and nothing conflicts on insert. Only the root
/// `last_scan_at` comparison can detect that this scan is stale — which is what
/// makes the test load-bearing for the CAS rather than for the other guards.
#[tokio::test]
async fn stale_scan_fails_the_root_cas_and_rolls_back() {
    let store = store().await;
    let root = Root::new();
    root.write("kept.gguf", b"kept");
    root.write("extra.gguf", b"extra");
    register_root(&store.store, root.path(), "root-1").await;

    // Commit a baseline scan so the index is populated and the artifacts are
    // unchanged from here on: later scans of this tree are pure no-ops.
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("baseline scan");

    // Build a snapshot from the *baseline* root record and states. Because the
    // files never change again, this snapshot stays valid at the row level.
    let canonical = root.canonical();
    let stale_root = root_record(&store.store, "root-1").await;
    let (states, occupied) = crate::persistence::load_states(&store.store, "root-1")
        .await
        .expect("load states");
    let scanned = scan_tree(&canonical, ScanOptions::default()).expect("walk");
    let stale = crate::bookkeeping_for(&scanned.files, &states, &occupied, scanned.discarded);

    // A newer scan of the same root commits first and advances `last_scan_at`.
    // Its rows are identical, so the stale snapshot below remains referentially
    // consistent — only the root's timestamp has moved on.
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("newer scan");
    // Timestamps are stored at millisecond precision, so two fast scans can land
    // in the same millisecond. Advance the stored value explicitly rather than
    // hoping the clock moved.
    let advanced = stale_root.last_scan_at.expect("baseline stamped") + chrono::Duration::days(1);
    sqlx::query("UPDATE model_roots SET last_scan_at = ?2 WHERE id = ?1")
        .bind("root-1")
        .bind(model_serving_persistence::ts_string(advanced))
        .execute(store.store.pool())
        .await
        .expect("advance the newer scan timestamp");
    let after_newer = models(&store.store).await;
    let root_newer = root_record(&store.store, "root-1").await;
    assert_ne!(
        root_newer.last_scan_at, stale_root.last_scan_at,
        "the newer scan advanced the timestamp, so the snapshot is stale"
    );
    assert_eq!(root_newer.last_scan_at, Some(advanced));

    // The stale scan runs with the pre-newer root record. Its rows still match,
    // so only the CAS stands between it and overwriting newer bookkeeping.
    let error = crate::persistence::ScanWrite {
        store: &store.store,
    }
    .record_scan(&stale_root, &canonical, &stale, scanned.skipped)
    .await
    .expect_err("a stale scan must fail the CAS");
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");
    assert!(
        error
            .message
            .contains("changed while the scan was in flight"),
        "the failure is the CAS, not another guard: {error:?}"
    );

    // Nothing the stale scan did survives — including its sweep.
    assert_eq!(models(&store.store).await, after_newer, "index unchanged");
    let root_after = root_record(&store.store, "root-1").await;
    assert_eq!(
        root_after.last_scan_at, root_newer.last_scan_at,
        "the newer scan's bookkeeping stands"
    );
    assert_eq!(root_after.last_scan_result, root_newer.last_scan_result);
}

/// P1 (stale snapshot): a concurrent administrator edit that re-keys a row this
/// scan matched by path must not be misread as `unchanged` and then swept. The
/// preflight identity check aborts and rolls back.
#[tokio::test]
async fn concurrent_admin_rekey_aborts_and_rolls_back_the_sweep() {
    let store = store().await;
    let root = Root::new();
    root.write("shared.gguf", b"shared");
    register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("baseline scan");

    // Snapshot the scan's work list while the row still holds `shared`.
    let canonical = root.canonical();
    let loaded_root = root_record(&store.store, "root-1").await;
    let (states, occupied) = crate::persistence::load_states(&store.store, "root-1")
        .await
        .expect("load states");
    let scanned = scan_tree(&canonical, ScanOptions::default()).expect("walk");
    let bookkeeping = crate::bookkeeping_for(&scanned.files, &states, &occupied, scanned.discarded);

    // An administrator re-keys the row between the snapshot and the write.
    sqlx::query("UPDATE models SET key = 'admin-renamed' WHERE key = 'shared'")
        .execute(store.store.pool())
        .await
        .expect("admin re-key");
    let before: Vec<Model> = models(&store.store).await;
    assert_eq!(before.len(), 1, "exactly one row, now re-keyed");

    // The stale scan must fail the preflight identity check rather than treat a
    // zero-row UPDATE as `unchanged` and then sweep the re-keyed row away.
    let error = crate::persistence::ScanWrite {
        store: &store.store,
    }
    .record_scan(&loaded_root, &canonical, &bookkeeping, scanned.skipped)
    .await
    .expect_err("a stale scan must fail the identity preflight");
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");

    let after: Vec<Model> = models(&store.store).await;
    assert_eq!(after, before, "the re-keyed row survives untouched");
    let still = model_by_key(&store.store, "admin-renamed")
        .await
        .expect("admin key kept");
    assert!(!still.deleted, "the sweep must not delete it");
    assert!(
        model_by_key(&store.store, "shared").await.is_none(),
        "the stale key was not recreated"
    );
}

/// P1 (stale snapshot): the CAS also covers the root's `path` and `enabled`
/// columns, not just `last_scan_at`. A scan whose loaded root was re-pointed at
/// a different directory must not write bookkeeping for the old one.
#[tokio::test]
async fn stale_scan_fails_when_the_root_path_changed() {
    let store = store().await;
    let root = Root::new();
    root.write("a.gguf", b"a");
    register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("baseline scan");

    let canonical = root.canonical();
    let stale_root = root_record(&store.store, "root-1").await;
    let (states, occupied) = crate::persistence::load_states(&store.store, "root-1")
        .await
        .expect("load states");
    let scanned = scan_tree(&canonical, ScanOptions::default()).expect("walk");
    let bookkeeping = crate::bookkeeping_for(&scanned.files, &states, &occupied, scanned.discarded);

    // The administrator re-points the root elsewhere, then the stale scan tries
    // to commit against the directory it loaded.
    sqlx::query("UPDATE model_roots SET path = path || '-moved' WHERE id = ?1")
        .bind("root-1")
        .execute(store.store.pool())
        .await
        .expect("re-point root");
    let root_now = root_record(&store.store, "root-1").await;
    let before = models(&store.store).await;

    let error = crate::persistence::ScanWrite {
        store: &store.store,
    }
    .record_scan(&stale_root, &canonical, &bookkeeping, scanned.skipped)
    .await
    .expect_err("a re-pointed root must fail the CAS");
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");
    assert!(
        error
            .message
            .contains("changed while the scan was in flight"),
        "the failure is the CAS: {error:?}"
    );
    assert_eq!(models(&store.store).await, before, "index unchanged");
    assert_eq!(
        root_record(&store.store, "root-1").await.last_scan_at,
        root_now.last_scan_at,
        "the re-pointed root's bookkeeping is untouched"
    );
}

/// P1 (stale snapshot): disabling a root invalidates an in-flight scan, so its
/// bookkeeping is never written for a disabled root.
#[tokio::test]
async fn stale_scan_fails_when_the_root_was_disabled() {
    let store = store().await;
    let root = Root::new();
    root.write("a.gguf", b"a");
    register_root(&store.store, root.path(), "root-1").await;
    ScanService::new(&store.store)
        .scan_root("root-1")
        .await
        .expect("baseline scan");

    let canonical = root.canonical();
    let stale_root = root_record(&store.store, "root-1").await;
    let (states, occupied) = crate::persistence::load_states(&store.store, "root-1")
        .await
        .expect("load states");
    let scanned = scan_tree(&canonical, ScanOptions::default()).expect("walk");
    let bookkeeping = crate::bookkeeping_for(&scanned.files, &states, &occupied, scanned.discarded);

    sqlx::query("UPDATE model_roots SET enabled = 0 WHERE id = ?1")
        .bind("root-1")
        .execute(store.store.pool())
        .await
        .expect("disable root");
    let root_now = root_record(&store.store, "root-1").await;
    assert!(!root_now.enabled);
    let before = models(&store.store).await;

    let error = crate::persistence::ScanWrite {
        store: &store.store,
    }
    .record_scan(&stale_root, &canonical, &bookkeeping, scanned.skipped)
    .await
    .expect_err("a disabled root must fail the CAS");
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");
    assert_eq!(models(&store.store).await, before, "index unchanged");
    assert_eq!(
        root_record(&store.store, "root-1").await.last_scan_at,
        root_now.last_scan_at,
        "the disabled root's bookkeeping is untouched"
    );
}

/// P1 (stale snapshot): the preflight must not fire on a legitimate no-op. An
/// unchanged rescan performs a zero-row bookkeeping UPDATE by design, and that
/// must still be accepted (see `stored_scan_summary_matches_on_an_unchanged_rescan`
/// for the end-to-end form; this pins the property directly).
#[tokio::test]
async fn legitimate_unchanged_rescan_still_succeeds() {
    let store = store().await;
    let root = Root::new();
    root.write("stable.gguf", b"content");
    register_root(&store.store, root.path(), "root-1").await;

    let loaded_root = root_record(&store.store, "root-1").await;
    let canonical = root.canonical();
    let (states, occupied) = crate::persistence::load_states(&store.store, "root-1")
        .await
        .expect("load states");
    let scanned = scan_tree(&canonical, ScanOptions::default()).expect("walk");
    let bookkeeping = crate::bookkeeping_for(&scanned.files, &states, &occupied, scanned.discarded);

    // First write through the loaded root: adds the row and stamps the root.
    let summary = crate::persistence::ScanWrite {
        store: &store.store,
    }
    .record_scan(&loaded_root, &canonical, &bookkeeping, scanned.skipped)
    .await
    .expect("first scan");
    assert_eq!(summary.added, 1);

    // Now an unchanged rescan from a fresh snapshot: the bookkeeping UPDATE
    // affects 0 rows because nothing about the row needs changing, and the CAS
    // must still match.
    let reloaded = root_record(&store.store, "root-1").await;
    let (states, occupied) = crate::persistence::load_states(&store.store, "root-1")
        .await
        .expect("load states");
    let bookkeeping = crate::bookkeeping_for(&scanned.files, &states, &occupied, scanned.discarded);
    let summary = crate::persistence::ScanWrite {
        store: &store.store,
    }
    .record_scan(&reloaded, &canonical, &bookkeeping, scanned.skipped)
    .await
    .expect("an unchanged rescan is not a stale scan");
    assert_eq!(summary.unchanged, 1);
    assert_eq!(summary.updated, 0);
    assert_eq!(summary.removed, 0);
}
