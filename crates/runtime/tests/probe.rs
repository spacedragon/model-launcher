//! Integration tests for the bounded, shell-free runtime probe and `doctor`.
//!
//! The controllable child is the `probe_helper` bin (`src/bin/probe_helper.rs`),
//! whose behaviour is selected by `PROBE_HELPER_MODE`. Cargo exposes its path
//! as `CARGO_BIN_EXE_probe_helper`, so these tests run identically on Windows
//! and Unix without shell scripts.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use model_serving_domain::model::Capabilities;
use model_serving_runtime::{
    DoctorStatus, OutputStream, ProbeConfig, ProbeError, ProbeStage, diagnose, diagnose_with, probe,
};

/// Path to the test-support fake runtime.
fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_probe_helper"))
}

/// A definitely-nonexistent path that is absolute on every platform, so the
/// probe reaches the metadata check instead of the absolute-path check.
#[cfg(windows)]
const MISSING_EXECUTABLE: &str = r"C:\definitely\not\a\real\engine-binary.exe";
#[cfg(not(windows))]
const MISSING_EXECUTABLE: &str = "/definitely/not/a/real/engine-binary";

/// A probe configuration with the helper's behaviour mode set.
fn config(mode: &str) -> ProbeConfig {
    ProbeConfig::new().with_env("PROBE_HELPER_MODE", mode)
}

#[tokio::test]
async fn probe_reports_version_help_and_capabilities() {
    let snapshot = match probe(&helper(), &config("ok")).await {
        Ok(snapshot) => snapshot,
        Err(error) => panic!("expected a successful probe, got {error:?}"),
    };

    assert!(snapshot.version_text().contains("1.2.3"));
    assert!(snapshot.help_text().contains("Usage: probe-helper"));
    assert_eq!(
        snapshot.capabilities().version.as_deref(),
        Some(snapshot.version_text())
    );
    assert!(!snapshot.capabilities().supports_chat_completions);
}

#[tokio::test]
async fn probe_reports_a_missing_executable() {
    let missing = PathBuf::from(MISSING_EXECUTABLE);

    match probe(&missing, &config("ok")).await {
        Err(ProbeError::Missing { path }) => assert_eq!(path, missing),
        other => panic!("expected Missing, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_rejects_a_relative_executable_path() {
    let relative = Path::new("engine-binary");
    match probe(relative, &config("ok")).await {
        Err(ProbeError::NotAbsolute { path }) => assert_eq!(path, relative),
        other => panic!("expected NotAbsolute, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_rejects_an_empty_executable_path() {
    match probe(Path::new(""), &config("ok")).await {
        Err(ProbeError::NotAbsolute { path }) => assert!(path.as_os_str().is_empty()),
        other => panic!("expected NotAbsolute, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_rejects_a_directory() {
    let Ok(directory) = tempfile::tempdir() else {
        panic!("failed to create a temporary directory");
    };

    match probe(directory.path(), &config("ok")).await {
        Err(ProbeError::NotFile { .. }) => {}
        other => panic!("expected NotFile, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_maps_a_nonzero_exit() {
    match probe(&helper(), &config("nonzero")).await {
        Err(ProbeError::NonZero {
            stage,
            exit_code,
            detail,
            ..
        }) => {
            assert_eq!(stage, ProbeStage::Version);
            assert_eq!(exit_code, Some(7));
            assert!(detail.contains("refusing to run"));
        }
        other => panic!("expected NonZero, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_kills_and_reports_a_timeout() {
    let config = config("sleep").with_timeout(Duration::from_millis(300));

    match probe(&helper(), &config).await {
        Err(ProbeError::Timeout { stage, timeout, .. }) => {
            assert_eq!(stage, ProbeStage::Version);
            assert_eq!(timeout, Duration::from_millis(300));
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_times_out_when_a_descendant_holds_the_pipes_open() {
    let config = config("descendant").with_timeout(Duration::from_millis(400));
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(10), probe(&helper(), &config)).await;
    let elapsed = started.elapsed();

    match result {
        Ok(Err(ProbeError::Timeout { stage, .. })) => {
            assert_eq!(stage, ProbeStage::Version);
        }
        other => panic!("expected a bounded Timeout, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "probe returned too slowly after a descendant held the pipes: {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(300),
        "probe returned before its deadline elapsed: {elapsed:?}"
    );
}

#[tokio::test]
async fn probe_fails_when_stdout_exceeds_the_cap() {
    let config = config("grow").with_max_stdout_bytes(4096);

    match probe(&helper(), &config).await {
        Err(ProbeError::OutputTooLarge {
            stage,
            stream,
            limit,
            ..
        }) => {
            assert_eq!(stage, ProbeStage::Version);
            assert_eq!(stream, OutputStream::Stdout);
            assert_eq!(limit, 4096);
        }
        other => panic!("expected OutputTooLarge(Stdout), got {other:?}"),
    }
}

#[tokio::test]
async fn probe_fails_when_stderr_exceeds_the_cap() {
    let config = config("grow_err").with_max_stderr_bytes(4096);

    match probe(&helper(), &config).await {
        Err(ProbeError::OutputTooLarge {
            stage,
            stream,
            limit,
            ..
        }) => {
            assert_eq!(stage, ProbeStage::Version);
            assert_eq!(stream, OutputStream::Stderr);
            assert_eq!(limit, 4096);
        }
        other => panic!("expected OutputTooLarge(Stderr), got {other:?}"),
    }
}

#[tokio::test]
async fn probe_rejects_non_utf8_output() {
    match probe(&helper(), &config("malformed")).await {
        Err(ProbeError::Malformed { stage, .. }) => assert_eq!(stage, ProbeStage::Version),
        other => panic!("expected Malformed, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn probe_rejects_a_file_without_execute_permission() {
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(directory) = tempfile::tempdir() else {
        panic!("failed to create a temporary directory");
    };
    let path = directory.path().join("engine");
    let Ok(()) = std::fs::write(&path, b"#!/bin/sh\nexit 0\n") else {
        panic!("failed to write the fake engine");
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        panic!("failed to stat the fake engine");
    };
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o644);
    let Ok(()) = std::fs::set_permissions(&path, permissions) else {
        panic!("failed to set the fake engine permissions");
    };

    match probe(&path, &config("ok")).await {
        Err(ProbeError::NotExecutable { path: probed }) => assert_eq!(probed, path),
        other => panic!("expected NotExecutable, got {other:?}"),
    }
}

#[tokio::test]
async fn doctor_reports_a_usable_runtime_as_ready() {
    let report = diagnose(&helper(), &config("ok")).await;
    assert_eq!(report.status, DoctorStatus::Ready);
    assert!(report.is_ready());
    assert_eq!(report.code, "runtime_ready");
    assert!(
        report
            .version_text
            .as_deref()
            .is_some_and(|text| text.contains("1.2.3"))
    );
}

#[tokio::test]
async fn doctor_reports_a_deleted_executable_as_missing() {
    let report = diagnose(Path::new(MISSING_EXECUTABLE), &config("ok")).await;
    assert_eq!(report.status, DoctorStatus::Missing);
    assert_eq!(report.code, "runtime_missing");
    assert!(report.message.contains("install the engine"));
}

#[tokio::test]
async fn doctor_reports_a_failing_executable_as_unhealthy() {
    let report = diagnose(&helper(), &config("nonzero")).await;
    assert_eq!(report.status, DoctorStatus::Unhealthy);
    assert_eq!(report.code, "runtime_probe_failed");
    assert!(report.message.contains("version"));
    assert_eq!(report.stage, Some(ProbeStage::Version));
}

#[tokio::test]
async fn doctor_reports_a_help_stage_failure_with_the_stage() {
    let config = config("sleep_help").with_timeout(Duration::from_millis(300));
    let report = diagnose(&helper(), &config).await;
    assert_eq!(report.status, DoctorStatus::Unhealthy);
    assert_eq!(report.code, "runtime_probe_timeout");
    assert_eq!(report.stage, Some(ProbeStage::Help));
    assert!(report.message.contains("help"));
}

#[tokio::test]
async fn diagnose_with_reports_an_incompatible_engine_as_incompatible() {
    let report = diagnose_with(&helper(), &config("ok"), |snapshot| {
        Err(ProbeError::incompatible(&snapshot.executable, "too old"))
    })
    .await;

    assert_eq!(report.status, DoctorStatus::Incompatible);
    assert_eq!(report.code, "runtime_incompatible");
    assert_eq!(report.stage, None);
    assert!(report.message.contains("too old"));
}

#[tokio::test]
async fn diagnose_with_carries_adapter_capabilities_into_a_ready_report() {
    let report = diagnose_with(&helper(), &config("ok"), |_snapshot| {
        Ok(Capabilities {
            supports_completions: true,
            ..Capabilities::default()
        })
    })
    .await;

    assert_eq!(report.status, DoctorStatus::Ready);
    assert!(report.capabilities.supports_completions);
}
