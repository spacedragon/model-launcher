//! Cross-platform process-supervisor fixture tests.

use model_serving_runtime::{
    CommandSpec, ExitClassification, ManagedProcess, ProbeStatus, ShutdownOutcome,
    SupervisorConfig, SupervisorError,
};
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};

fn helper(mode: &str) -> CommandSpec {
    CommandSpec::new(PathBuf::from(env!("CARGO_BIN_EXE_supervisor_helper")))
        .with_env("SUPERVISOR_HELPER_MODE", mode)
}

fn test_config() -> SupervisorConfig {
    SupervisorConfig {
        stdout_capacity: 4096,
        stderr_capacity: 3072,
        startup_timeout: Duration::from_millis(180),
        probe_timeout: Duration::from_millis(40),
        probe_interval: Duration::from_millis(10),
        shutdown_grace: Duration::from_millis(150),
        post_kill_timeout: Duration::from_secs(3),
        pipe_eof_timeout: Duration::from_secs(3),
    }
}

#[tokio::test]
async fn continuously_drains_both_streams_into_bounded_rings() {
    let config = test_config();
    let mut process = ManagedProcess::spawn(&helper("verbose"), config).expect("spawn helper");
    let logs = process.logs();
    let deadline = Instant::now() + Duration::from_secs(10);
    while (logs.stdout_total() <= 70 * 1024 || logs.stderr_total() <= 70 * 1024)
        && Instant::now() < deadline
    {
        sleep(Duration::from_millis(20)).await;
    }
    assert!(logs.stdout_total() > 70 * 1024, "stdout pump stalled");
    assert!(logs.stderr_total() > 70 * 1024, "stderr pump stalled");
    assert_eq!(logs.stdout_tail().len(), config.stdout_capacity);
    assert_eq!(logs.stderr_tail().len(), config.stderr_capacity);

    let report = process.cancel().await.expect("cancel verbose helper");
    assert_eq!(report.classification, ExitClassification::Cancelled);
}

#[tokio::test]
async fn classifies_clean_crash_and_likely_oom() {
    for (mode, expected, code) in [
        ("clean", ExitClassification::Clean, 0),
        ("crash", ExitClassification::Crash, 3),
        ("oom", ExitClassification::LikelyGpuOom, 7),
    ] {
        let mut process =
            ManagedProcess::spawn(&helper(mode), test_config()).expect("spawn helper");
        let status = process.wait().await.expect("wait helper");
        let report = process.classify_exit(status);
        assert_eq!(report.classification, expected, "mode {mode}");
        assert_eq!(report.exit_code, Some(code));
    }
}

#[tokio::test]
async fn readiness_and_health_probes_have_deadlines() {
    let config = test_config();

    // (1) Readiness timeout: a live sleep helper that never reports Ready must
    // be torn down at the startup deadline, surfacing ReadinessTimeout. This
    // helper is fully settled by `wait_ready` before it returns.
    let mut readiness =
        ManagedProcess::spawn(&helper("sleep"), config).expect("spawn readiness helper");
    let error = readiness
        .wait_ready(|| async { Ok::<_, ()>(ProbeStatus::Pending) })
        .await
        .expect_err("pending readiness must time out");
    assert!(matches!(error, SupervisorError::ReadinessTimeout));

    // (2) Health timeout: a SEPARATE live helper, still running, so the probe
    // is genuinely executed under probe_timeout instead of short-circuiting on
    // an already-exited child. The probe outlives probe_timeout, so the
    // supervisor's deadline -- not the probe -- ends it: it waits
    // approximately probe_timeout and then returns false.
    let mut health = ManagedProcess::spawn(&helper("sleep"), config).expect("spawn health helper");
    let started = Instant::now();
    let healthy = health
        .probe_health(|| async {
            sleep(Duration::from_secs(60)).await;
            Ok::<_, ()>(ProbeStatus::Ready)
        })
        .await
        .expect("health probe");
    let elapsed = started.elapsed();
    assert!(
        !healthy,
        "a probe outlasting probe_timeout must read unhealthy"
    );
    assert!(
        elapsed >= config.probe_timeout,
        "health probe must wait at least probe_timeout (got {elapsed:?})"
    );
    assert!(
        elapsed < config.probe_timeout + Duration::from_secs(1),
        "health probe must be cut off by probe_timeout, not the probe (got {elapsed:?})"
    );

    // The second helper is still alive; cancel it now that the probe is done.
    let report = health.cancel().await.expect("cancel health helper");
    assert_eq!(report.classification, ExitClassification::Cancelled);
}

#[tokio::test]
async fn ready_result_is_rejected_if_child_exited_during_probe() {
    let mut process = ManagedProcess::spawn(&helper("clean"), test_config()).expect("spawn helper");
    let error = process
        .wait_ready(|| async {
            sleep(Duration::from_millis(100)).await;
            Ok::<_, ()>(ProbeStatus::Ready)
        })
        .await
        .expect_err("an exited child cannot be published ready");
    assert!(matches!(error, SupervisorError::ExitedBeforeReady));
}

#[tokio::test]
async fn shutdown_settles_process_and_both_pipe_eofs() {
    let mut process = ManagedProcess::spawn(&helper("sleep"), test_config()).expect("spawn helper");
    let outcome = timeout(Duration::from_secs(5), process.shutdown())
        .await
        .expect("shutdown outer deadline")
        .expect("shutdown");
    #[cfg(unix)]
    assert_eq!(outcome, ShutdownOutcome::Graceful);
    #[cfg(windows)]
    assert!(matches!(
        outcome,
        ShutdownOutcome::Graceful | ShutdownOutcome::Escalated
    ));
    assert!(process.try_wait().expect("status").is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn term_ignoring_child_escalates_to_group_kill() {
    let mut process =
        ManagedProcess::spawn(&helper("ignores_term"), test_config()).expect("spawn helper");
    sleep(Duration::from_millis(100)).await;
    let outcome = process.shutdown().await.expect("shutdown");
    assert_eq!(outcome, ShutdownOutcome::Escalated);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn process_group_kill_removes_descendant_and_reaches_eof() {
    let mut process =
        ManagedProcess::spawn(&helper("grandchild"), test_config()).expect("spawn helper");
    let logs = process.logs();
    let deadline = Instant::now() + Duration::from_secs(3);
    while logs.stdout_total() == 0 && Instant::now() < deadline {
        let _ = process.try_wait().expect("poll child");
        sleep(Duration::from_millis(20)).await;
    }
    assert!(logs.stdout_total() > 0, "grandchild fixture did not start");
    process.shutdown().await.expect("group shutdown");
    assert!(live_pids_in_group(process.pid()).is_empty());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn natural_leader_exit_kills_snapshotted_descendant() {
    let mut process = ManagedProcess::spawn(&helper("leader_exit_grandchild"), test_config())
        .expect("spawn helper");
    let status = timeout(Duration::from_secs(5), process.wait())
        .await
        .expect("wait outer deadline")
        .expect("descendant-aware wait");
    assert!(status.success());
    assert!(live_pids_in_group(process.pid()).is_empty());
}

#[tokio::test]
async fn drop_reaps_the_direct_child_pid_cross_platform() {
    let process = ManagedProcess::spawn(&helper("sleep"), test_config()).expect("spawn helper");
    let pid = process.pid();

    // Confirm the child is observably alive before dropping it. An enumeration
    // failure (`None`) skips the test rather than pretending the child is
    // absent; a child that is never seen alive is a real failure.
    let mut seen_alive = false;
    let pre_deadline = Instant::now() + Duration::from_secs(2);
    while !seen_alive && Instant::now() < pre_deadline {
        match pid_present(pid) {
            Some(true) => seen_alive = true,
            Some(false) => {}
            None => return,
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(
        seen_alive,
        "precondition: child pid {pid} must be observable before drop"
    );

    // Dropping the supervisor must reap (kill + wait) the direct child.
    drop(process);

    // Poll up to 5s for the direct PID to disappear. An enumeration failure
    // (`None`) skips instead of pretending the PID is absent.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match pid_present(pid) {
            Some(false) | None => return,
            Some(true) => {
                assert!(
                    Instant::now() < deadline,
                    "child pid {pid} still present 5s after drop"
                );
                sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Cross-platform liveness check for a direct child PID.
///
/// Returns `Some(true)` if the PID is confirmed alive, `Some(false)` if it is
/// confirmed gone, and `None` when process enumeration failed on this
/// platform. Callers MUST treat `None` as "cannot tell" and skip rather than
/// pretend the PID is absent.
#[cfg(target_os = "linux")]
fn pid_present(pid: u32) -> Option<bool> {
    // `/proc/<pid>` exists for any registered process, including an unreaped
    // zombie; only a clean `NotFound` means the entry is truly gone.
    match std::fs::metadata(format!("/proc/{pid}")) {
        Ok(_) => Some(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        // Any other I/O error means /proc could not be enumerated.
        Err(_) => None,
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
#[allow(unsafe_code)]
fn pid_present(pid: u32) -> Option<bool> {
    // SAFETY: `kill(pid, 0)` sends no signal; it only probes whether the
    // process exists and whether we may signal it.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return Some(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        // ESRCH: no such process -> confirmed gone.
        Some(libc::ESRCH) => Some(false),
        // EPERM: it exists but is not ours -> still present.
        Some(libc::EPERM) => Some(true),
        // Anything else: enumeration failed -> skip.
        _ => None,
    }
}

#[cfg(windows)]
fn pid_present(pid: u32) -> Option<bool> {
    // `tasklist /NH /FO CSV` prints one `"<image>","<pid>",...` row per
    // process; match the exact PID column. A spawn failure or non-zero exit
    // means we cannot enumerate: report `None` so the caller skips instead of
    // pretending the PID is absent.
    let output = std::process::Command::new("tasklist")
        .arg("/NH")
        .arg("/FO")
        .arg("CSV")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let wanted = pid.to_string();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<&str> = line
            .split(',')
            .map(|field| field.trim_matches('"').trim())
            .collect();
        if fields.len() >= 2 && fields[1] == wanted {
            return Some(true);
        }
    }
    Some(false)
}

#[cfg(not(any(unix, windows)))]
fn pid_present(_pid: u32) -> Option<bool> {
    // No process-enumeration strategy for this platform: always skip.
    None
}

#[cfg(target_os = "linux")]
fn live_pids_in_group(pgid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false;
            };
            let Some(end) = stat.rfind(')') else {
                return false;
            };
            let fields: Vec<&str> = stat[end + 1..].split_whitespace().collect();
            fields.first().copied() != Some("Z")
                && fields.get(2).and_then(|value| value.parse().ok()) == Some(pgid)
        })
        .collect()
}
