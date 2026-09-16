//! Runtime `doctor`: a human- and machine-readable verdict over [`probe`].
//!
//! `doctor` is the M1 entry point that answers "can this runtime executable be
//! used at all?" (ADR-0003). It runs [`probe`] and collapses the typed
//! [`ProbeError`] taxonomy into one of four statuses with an actionable
//! message:
//!
//! - [`DoctorStatus::Ready`] — the executable ran, `--version` and `--help`
//!   were captured, and a version string was reported.
//! - [`DoctorStatus::Missing`] — the path does not exist, is not a regular
//!   file, or is not executable.
//! - [`DoctorStatus::Incompatible`] — the executable ran but is not a usable
//!   engine (malformed output, or an adapter-level incompatibility).
//! - [`DoctorStatus::Unhealthy`] — the executable is present but the probe
//!   failed (spawn error, timeout, non-zero exit, oversized output).

use std::path::{Path, PathBuf};

use model_serving_domain::model::Capabilities;

use crate::probe::{ProbeConfig, ProbeError, ProbeSnapshot, ProbeStage, probe};

/// The four `doctor` verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorStatus {
    /// The runtime probed cleanly and can be used.
    Ready,
    /// The runtime executable is absent or unusable as a file.
    Missing,
    /// The executable ran but is not a compatible engine.
    Incompatible,
    /// The executable is present but the probe failed.
    Unhealthy,
}

impl std::fmt::Display for DoctorStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready => formatter.write_str("ready"),
            Self::Missing => formatter.write_str("missing"),
            Self::Incompatible => formatter.write_str("incompatible"),
            Self::Unhealthy => formatter.write_str("unhealthy"),
        }
    }
}

/// The outcome of a `doctor` check for one executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    /// The checked executable.
    pub executable: PathBuf,
    /// The verdict.
    pub status: DoctorStatus,
    /// A stable, machine-readable reason code (provisional doctor codes).
    pub code: &'static str,
    /// Which probe invocation produced the failure, when the failure is
    /// attributable to `version` or `help`.
    pub stage: Option<ProbeStage>,
    /// An actionable, human-readable explanation.
    pub message: String,
    /// The captured version text, when the probe succeeded.
    pub version_text: Option<String>,
    /// The capabilities captured by the probe; defaults to empty on failure.
    pub capabilities: Capabilities,
}

impl DoctorReport {
    /// Whether the runtime is usable.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status == DoctorStatus::Ready
    }

    /// Build a report from a successful probe, using the probe's base
    /// capabilities as-is.
    #[must_use]
    pub fn ready(snapshot: &ProbeSnapshot) -> Self {
        Self::ready_with(snapshot, snapshot.capabilities.clone())
    }

    /// Build a ready report carrying adapter-derived `capabilities`.
    ///
    /// Adapters pass the result of their own compatibility check so the ready
    /// report reflects the adapter's view, not just the generic probe base.
    #[must_use]
    pub fn ready_with(snapshot: &ProbeSnapshot, capabilities: Capabilities) -> Self {
        Self {
            executable: snapshot.executable.clone(),
            status: DoctorStatus::Ready,
            code: "runtime_ready",
            stage: None,
            message: if snapshot.version_text.is_empty() {
                "runtime executable is usable, but reported no version text; set the executable \
                 or engine explicitly"
                    .to_owned()
            } else {
                format!("runtime executable is usable: {}", snapshot.version_text)
            },
            version_text: Some(snapshot.version_text.clone()),
            capabilities,
        }
    }

    /// Build a report from a failed probe.
    #[must_use]
    pub fn from_error(executable: &Path, error: &ProbeError) -> Self {
        let (status, code, stage, message) = describe(error);
        Self {
            executable: executable.to_path_buf(),
            status,
            code,
            stage,
            message,
            version_text: None,
            capabilities: Capabilities::default(),
        }
    }
}

/// Path-validation variants that fire before the process is spawned. Returns
/// `None` for errors that can only occur after a successful spawn.
fn describe_path_error(
    path: &impl std::fmt::Display,
    error: &ProbeError,
) -> Option<(DoctorStatus, &'static str, Option<ProbeStage>, String)> {
    Some(match error {
        ProbeError::Missing { .. } => (
            DoctorStatus::Missing,
            "runtime_missing",
            None,
            format!("runtime executable not found at {path}: install the engine or set its path"),
        ),
        ProbeError::NotFile { .. } => (
            DoctorStatus::Missing,
            "runtime_not_a_file",
            None,
            format!("runtime path is not a regular file at {path}: point at the engine binary"),
        ),
        ProbeError::NotExecutable { .. } => (
            DoctorStatus::Missing,
            "runtime_not_executable",
            None,
            format!("runtime at {path} is not executable: run `chmod +x` on the engine binary"),
        ),
        ProbeError::NotAbsolute { .. } => (
            DoctorStatus::Missing,
            "runtime_not_absolute",
            None,
            format!(
                "runtime executable path {path} is empty or not absolute: set an absolute path \
                 to the engine binary"
            ),
        ),
        _ => return None,
    })
}

/// Map a [`ProbeError`] to `(status, code, stage, actionable message)`.
fn describe(error: &ProbeError) -> (DoctorStatus, &'static str, Option<ProbeStage>, String) {
    let path = error.path().display();
    if let Some(report) = describe_path_error(&path, error) {
        return report;
    }
    match error {
        ProbeError::Spawn { message, .. } => (
            DoctorStatus::Unhealthy,
            "runtime_spawn_failed",
            None,
            format!(
                "failed to start runtime at {path}: {message}; check permissions and the binary"
            ),
        ),
        ProbeError::Timeout { stage, timeout, .. } => (
            DoctorStatus::Unhealthy,
            "runtime_probe_timeout",
            Some(*stage),
            format!(
                "runtime at {path} did not answer the {stage} probe within {timeout:?}: \
                 it may be hung; re-run `--{stage}` manually"
            ),
        ),
        ProbeError::NonZero {
            stage,
            status,
            detail,
            ..
        } => (
            DoctorStatus::Unhealthy,
            "runtime_probe_failed",
            Some(*stage),
            if detail.is_empty() {
                format!("runtime at {path} failed the {stage} probe ({status})")
            } else {
                format!("runtime at {path} failed the {stage} probe ({status}): {detail}")
            },
        ),
        ProbeError::OutputTooLarge {
            stage,
            stream,
            limit,
            ..
        } => (
            DoctorStatus::Unhealthy,
            "runtime_probe_output_too_large",
            Some(*stage),
            format!(
                "runtime at {path} produced more than {limit} bytes on {stream} for the \
                 {stage} probe: it may not be an inference engine"
            ),
        ),
        ProbeError::PipeRead {
            stage,
            stream,
            message,
            ..
        } => (
            DoctorStatus::Unhealthy,
            "runtime_probe_pipe_read_failed",
            Some(*stage),
            format!(
                "runtime at {path} could not be read on {stream} for the {stage} probe \
                 ({message}): re-run `--{stage}` manually"
            ),
        ),
        ProbeError::Malformed { stage, message, .. } => (
            DoctorStatus::Incompatible,
            "runtime_output_malformed",
            Some(*stage),
            format!(
                "runtime at {path} produced unreadable {stage} output ({message}): \
                 check the engine build"
            ),
        ),
        ProbeError::Incompatible { message, .. } => (
            DoctorStatus::Incompatible,
            "runtime_incompatible",
            None,
            format!("runtime at {path} is not supported: {message}"),
        ),
        // The four path-validation variants would have returned above.
        _ => unreachable!("path-validation variants return in describe_path_error"),
    }
}

/// Run `doctor` for one executable: probe it and map the result to a report.
///
/// This function never fails; an unreachable or incompatible executable is
/// represented by [`DoctorReport::status`], not an error, so a UI can render
/// the full fleet even when some runtimes are broken.
pub async fn diagnose_with<F>(executable: &Path, config: &ProbeConfig, derive: F) -> DoctorReport
where
    F: FnOnce(&ProbeSnapshot) -> Result<Capabilities, ProbeError>,
{
    match probe(executable, config).await {
        Ok(snapshot) => match derive(&snapshot) {
            Ok(capabilities) => DoctorReport::ready_with(&snapshot, capabilities),
            Err(error) => DoctorReport::from_error(executable, &error),
        },
        Err(error) => DoctorReport::from_error(executable, &error),
    }
}

/// Run the generic `doctor` for one executable: probe it and map the result to
/// a report using the probe's own base capabilities.
///
/// Adapters should prefer [`diagnose_with`] so that an engine which probes
/// cleanly but is too old or is the wrong product is reported as
/// [`DoctorStatus::Incompatible`] rather than [`DoctorStatus::Ready`].
///
/// This function never fails; an unreachable or incompatible executable is
/// represented by [`DoctorReport::status`], not an error.
pub async fn diagnose(executable: &Path, config: &ProbeConfig) -> DoctorReport {
    diagnose_with(executable, config, |snapshot| {
        Ok(snapshot.capabilities.clone())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::{DoctorReport, DoctorStatus};
    use crate::probe::{OutputStream, ProbeError, ProbeStage};
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn missing_maps_to_missing() {
        let error = ProbeError::Missing {
            path: Path::new("/no/such/engine").to_path_buf(),
        };
        let report = DoctorReport::from_error(Path::new("/no/such/engine"), &error);
        assert_eq!(report.status, DoctorStatus::Missing);
        assert_eq!(report.code, "runtime_missing");
        assert!(report.message.contains("/no/such/engine"));
        assert!(!report.is_ready());
    }

    #[test]
    fn not_absolute_maps_to_missing() {
        let error = ProbeError::NotAbsolute {
            path: Path::new("engine-binary").to_path_buf(),
        };
        let report = DoctorReport::from_error(Path::new("engine-binary"), &error);
        assert_eq!(report.status, DoctorStatus::Missing);
        assert_eq!(report.code, "runtime_not_absolute");
        assert!(report.message.contains("not absolute"));
        assert!(report.message.contains("engine-binary"));
        assert!(!report.is_ready());
    }

    #[test]
    fn timeout_maps_to_unhealthy() {
        let error = ProbeError::Timeout {
            path: Path::new("/opt/engine").to_path_buf(),
            stage: ProbeStage::Help,
            timeout: Duration::from_secs(5),
        };
        let report = DoctorReport::from_error(Path::new("/opt/engine"), &error);
        assert_eq!(report.status, DoctorStatus::Unhealthy);
        assert_eq!(report.code, "runtime_probe_timeout");
        assert_eq!(report.stage, Some(ProbeStage::Help));
        assert!(report.message.contains("help"));
    }

    #[test]
    fn nonzero_maps_to_unhealthy_with_stage() {
        let error = ProbeError::NonZero {
            path: Path::new("/opt/engine").to_path_buf(),
            stage: ProbeStage::Version,
            exit_code: Some(7),
            status: "exit code 7".to_owned(),
            detail: "boom".to_owned(),
        };
        let report = DoctorReport::from_error(Path::new("/opt/engine"), &error);
        assert_eq!(report.status, DoctorStatus::Unhealthy);
        assert!(report.message.contains("version"));
        assert!(report.message.contains("boom"));
    }

    #[test]
    fn oversized_maps_to_unhealthy() {
        let error = ProbeError::OutputTooLarge {
            path: Path::new("/opt/engine").to_path_buf(),
            stage: ProbeStage::Version,
            stream: OutputStream::Stdout,
            limit: 1024,
        };
        let report = DoctorReport::from_error(Path::new("/opt/engine"), &error);
        assert_eq!(report.status, DoctorStatus::Unhealthy);
        assert_eq!(report.stage, Some(ProbeStage::Version));
        assert!(report.message.contains("stdout"));
        assert!(report.message.contains("version"));
    }

    #[test]
    fn pipe_read_failure_maps_to_unhealthy_with_stage() {
        let error = ProbeError::PipeRead {
            path: Path::new("/opt/engine").to_path_buf(),
            stage: ProbeStage::Help,
            stream: OutputStream::Stderr,
            message: "broken pipe".to_owned(),
        };
        let report = DoctorReport::from_error(Path::new("/opt/engine"), &error);
        assert_eq!(report.status, DoctorStatus::Unhealthy);
        assert_eq!(report.code, "runtime_probe_pipe_read_failed");
        assert_eq!(report.stage, Some(ProbeStage::Help));
        assert!(report.message.contains("stderr"));
    }

    #[test]
    fn malformed_maps_to_incompatible() {
        let error = ProbeError::Malformed {
            path: Path::new("/opt/engine").to_path_buf(),
            stage: ProbeStage::Version,
            message: "stdout: invalid utf-8".to_owned(),
        };
        let report = DoctorReport::from_error(Path::new("/opt/engine"), &error);
        assert_eq!(report.status, DoctorStatus::Incompatible);
        assert_eq!(report.code, "runtime_output_malformed");
        assert_eq!(report.stage, Some(ProbeStage::Version));
    }

    #[test]
    fn incompatible_maps_to_incompatible() {
        let error = ProbeError::incompatible("/opt/engine", "version b1 below minimum b5555");
        let report = DoctorReport::from_error(Path::new("/opt/engine"), &error);
        assert_eq!(report.status, DoctorStatus::Incompatible);
        assert_eq!(report.code, "runtime_incompatible");
        assert!(report.message.contains("b5555"));
    }
}
