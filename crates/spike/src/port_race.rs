//! Spike C (M0): idle-port selection race — how the daemon should hand a
//! port to a child runtime process (`llama-server` / `ninfer-serve`), which
//! cannot inherit a listening socket from us.
//!
//! **Disposable spike** — the production policy is recorded in
//! `docs/adr/ADR-0001-port-allocation.md` and implemented in M2
//! (`crates/runtime` supervisor). This file measures both candidates:
//!
//! - **A (daemon pre-allocates)**: probe `TcpListener::bind("127.0.0.1:0")`
//!   → get port P → **drop** the listener → race a "port stealer" (binds P
//!   after ~20ms) against the child (binds P) for P. The window between our
//!   probe and the child's own bind is exactly the race the M0 plan warns
//!   about ("端口先检查后被占用"). We count how often the child loses the
//!   race (eprintln only, no assertion — it is a probability, not a pass/fail).
//! - **B (child self-selects)**: the child binds `127.0.0.1:0` itself and
//!   prints `PORT=<n>` on stdout; the parent parses the ready line and
//!   verifies reachability with a real TCP connect. Asserted 100/100.
//!
//! Child / stealer commands are chosen at runtime via `std::env::consts::OS`
//! (Windows: PowerShell one-liner `TcpListener` try/catch reporting via
//! exit code; Linux: python3 if present, else `nc`, else the round is
//! skipped and reported on stderr).

use std::time::{Duration, Instant};

use tokio::io::AsyncBufReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::time::timeout;

const ROUNDS: usize = 100;
const STEALER_DELAY: Duration = Duration::from_millis(20);
/// How long the stealer KEEPS the port bound. A real competing binder is a
/// live process that holds the port (bind + listen persists), so the
/// stealer must hold — a fleeting bind-then-close would not model the race
/// the M0 plan warns about ("端口先检查后被占用").
const STEALER_HOLD: Duration = Duration::from_millis(500);
/// Simulated time from the daemon handing the port to the child until the
/// child actually binds (model loading / engine startup). `llama-server`
/// binds several seconds after spawn; 150ms keeps the round short while
/// preserving the race structure (stealer +20ms < child +150ms).
const CHILD_BIND_DELAY: Duration = Duration::from_millis(150);
/// Bounded wait for the child to print its ready line / bind.
const CHILD_TIMEOUT: Duration = Duration::from_secs(10);

/// Cross-platform helper: does `cmd args...` bind `127.0.0.1:port` after an
/// optional startup `delay`, keeping it bound for `hold`, then release it?
/// The probe reports via **stdout tokens**, not just the exit code (the
/// exit code alone cannot separate "bind rejected" from "interpreter/
/// script failure"): `BIND=ok` (0) or `BIND=conflict` (0, port was in
/// use). No token = probe error. The parent parses the token — see
/// `run_probe`.
///
/// The delay models real engine startup: `llama-server` only binds its
/// port AFTER model loading (seconds), so the daemon's probe→drop→spawn
/// window is exactly what a competing binder can win. The stealer binds at
/// +20ms; the child at +150ms (A) — see `race_preallocated_port`.
///
/// Windows uses `powershell -File <probe.ps1> <port> <delay_ms>`: inline
/// `-Command` one liners proved unreliable under CreateProcess arg quoting
/// (the same text that parses fine from a shell failed with a ParserError
/// when passed through Rust's command line, measured during the spike). The
/// .ps1 file is written once per test binary invocation into the OS temp
/// dir.
fn probe_bind_command(os: &str, port: u16, delay: Duration, hold: Duration) -> Option<Command> {
    let delay_ms = delay.as_millis() as i64;
    let hold_ms = hold.as_millis() as i64;
    match os {
        "windows" => {
            let script = probe_script_path("bind-probe.ps1")?;
            let mut cmd = Command::new("powershell");
            cmd.args([
                "-NoProfile",
                "-File",
                script.to_string_lossy().as_ref(),
                &port.to_string(),
                &delay_ms.to_string(),
                &hold_ms.to_string(),
            ]);
            Some(cmd)
        }
        _ => {
            if probe_path("python3") {
                let delay_s = delay.as_secs_f64();
                let hold_s = hold.as_secs_f64();
                // stdout token contract: `BIND=ok` / `BIND=conflict`,
                // always exit 0 — the token, not the exit code, carries
                // the bind result (any interpreter failure yields no
                // token and is classified ProbeError by `run_probe`).
                let script = format!(
                    "import socket,sys,time\ntime.sleep({delay_s})\n\n\
                     def probe():\n\
                     \x20 s=socket.socket()\n\
                     \x20 try:\n\
                     \x20 \x20 s.bind(('127.0.0.1', {port}))\n\
                     \x20 \x20 print('BIND=ok', flush=True)\n\
                     \x20 \x20 return s\n\
                     \x20 except OSError:\n\
                     \x20 \x20 print('BIND=conflict', flush=True)\n\
                     \x20 \x20 return None\n\
                     s=probe()\n\
                     time.sleep({hold_s})\n\
                     if s: s.close()\n"
                );
                let mut cmd = Command::new("python3");
                cmd.args(["-c", &script]);
                Some(cmd)
            } else {
                // No `nc -l` fallback: a *successful* `nc -l` listens
                // forever (nothing connects), so the probe always hits
                // CHILD_TIMEOUT and is misclassified as a bind failure —
                // every round would become ProbeError and the test's
                // `a_raced > 0` gate could never pass. Hosts without
                // python3 report the round as skipped instead.
                None
            }
        }
    }
}

fn probe_path(name: &str) -> bool {
    std::process::Command::new("bash")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write a probe/self-select .ps1 into the OS temp dir once per process and
/// return its path (windows probe scripts; unused on unix).
fn probe_script_path(name: &str) -> Option<std::path::PathBuf> {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let dir = std::env::temp_dir().join("model-serving-spike-port-race");
        let _ = std::fs::create_dir_all(&dir);
        let bind_probe = r#"
param([int]$Port, [int]$DelayMs = 0, [int]$HoldMs = 0)
if ($DelayMs -gt 0) { Start-Sleep -Milliseconds $DelayMs }
try { $l = New-Object Net.Sockets.TcpListener(([System.Net.IPAddress]::Parse('127.0.0.1')),$Port); $l.Start(); Write-Output 'BIND=ok'; if ($HoldMs -gt 0) { Start-Sleep -Milliseconds $HoldMs }; $l.Stop(); exit 0 }
catch { Write-Output 'BIND=conflict'; exit 0 }
"#;
        let self_select = r#"
try {
    $l = New-Object Net.Sockets.TcpListener(([System.Net.IPAddress]::Parse('127.0.0.1')),0)
    $l.Start()
    Write-Output ("PORT=" + [int]$l.Server.LocalEndPoint.Port)
    Start-Sleep -Seconds 5
    $l.Stop()
    exit 0
} catch { exit 1 }
"#;
        let _ = std::fs::write(dir.join("bind-probe.ps1"), bind_probe);
        let _ = std::fs::write(dir.join("self-select.ps1"), self_select);
    });
    Some(
        std::env::temp_dir()
            .join("model-serving-spike-port-race")
            .join(name),
    )
}

/// Tri-state probe result. The states matter: a child probe that *errors*
/// (spawn failure, interpreter missing, timeout, script failure) must NOT
/// be counted as a reproduced race even if the stealer happened to bind
/// the port — only a token-reported bind *conflict* plus a stealer bind is
/// a `LostRace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    /// The probe printed `BIND=ok`: the port was bound successfully.
    Bound,
    /// The probe printed `BIND=conflict`: the bind was explicitly rejected
    /// (port in use). This token is emitted by the probe script itself,
    /// so interpreter/script failures cannot masquerade as it.
    BindFailed,
    /// No token could be read (spawn error, timeout, interpreter
    /// failure) — the round yields no race observation.
    ProbeError,
}

/// Run a probe command and classify it via the **stdout token contract**
/// (`BIND=ok` / `BIND=conflict`), not the exit code: a nonzero exit can
/// be an interpreter/script/runtime failure, not a bind rejection, and
/// counting that as `BindFailed` would fabricate `LostRace` rounds when
/// the stealer happens to bind. No token ⇒ `ProbeError`.
/// `kill_on_drop` ensures a timeout/drop kills the child — without it a
/// binder that succeeded (port was free) would listen forever after the
/// 10s window and leak one process per round.
async fn run_probe(cmd: &mut Command) -> ProbeOutcome {
    cmd.stdout(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    match timeout(CHILD_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if std::env::var_os("RACE_DEBUG").is_some() {
                eprintln!(
                    "[probe] rc={:?} stdout={} stderr={}",
                    out.status.code(),
                    stdout.trim(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            if stdout.lines().any(|l| l.trim() == "BIND=ok") {
                ProbeOutcome::Bound
            } else if stdout.lines().any(|l| l.trim() == "BIND=conflict") {
                ProbeOutcome::BindFailed
            } else {
                ProbeOutcome::ProbeError
            }
        }
        _ => ProbeOutcome::ProbeError,
    }
}

/// **A**: daemon-side pre-allocated port. Returns `None` when no binder
/// exists on this host (race-A harness needs `python3` on unix or
/// `powershell` on Windows — the `nc -l` fallback is not usable because a
/// successful listener never exits); otherwise a `RaceARound`
/// classification (lost to the stealer / child ok / unrelated probe error).
async fn race_preallocated_port(round: usize) -> Option<RaceARound> {
    // 1. Daemon "probes" by binding :0, then releases the port.
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(_) => return None,
    };
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);

    // 2. Stealer: sleeps ~20ms, then binds P and HOLDS it (models a live
    //    competing process that snipes the port in the daemon's
    //    probe→child window).
    let Some(mut stealer_cmd) =
        probe_bind_command(std::env::consts::OS, port, STEALER_DELAY, STEALER_HOLD)
    else {
        eprintln!("[race A] round {round}: skipped — no python3/powershell binder on this host");
        return None;
    };
    let stealer_task = tokio::spawn(async move { run_probe(&mut stealer_cmd).await });

    // 3. Child: binds P only after its (simulated engine) startup delay.
    let mut child_cmd =
        probe_bind_command(std::env::consts::OS, port, CHILD_BIND_DELAY, Duration::ZERO)
            .expect("just checked");
    let child = run_probe(&mut child_cmd).await;
    let stealer = stealer_task.await.expect("spawned on current-thread");

    // Classify: a *reproduced race* requires the child's bind to have
    // been explicitly REJECTED while the stealer actually bound the port.
    // A child probe *error* (spawn/interpreter/timeout) is unrelated to
    // the race and must not be reported as a stealer win.
    match (child, stealer) {
        (ProbeOutcome::Bound, _) => Some(RaceARound::ChildOk),
        (ProbeOutcome::BindFailed, ProbeOutcome::Bound) => Some(RaceARound::LostRace),
        _ => {
            eprintln!(
                "[race A] round {round}: no race observation (child={child:?}, stealer={stealer:?})"
            );
            Some(RaceARound::ProbeError)
        }
    }
}

/// Classification of one raced round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RaceARound {
    /// Stealer bound P first; the child's bind of the daemon-probed port
    /// failed — the race reproduced.
    LostRace,
    /// Child bound P successfully this round.
    ChildOk,
    /// Child probe failed for reasons unrelated to the stealer (spawn /
    /// interpreter / timeout) — not counted toward the race statistics.
    ProbeError,
}

/// **B**: child self-selects `:0`, prints `PORT=<n>`, parent connects.
/// Returns true if parse + TCP connect both succeeded.
async fn child_self_selects_port(round: usize) -> Option<bool> {
    let mut cmd = self_select_command()?;
    cmd.stdout(std::process::Stdio::piped());
    // The self-select child holds a listener for ~5s; kill on drop so a
    // test timeout cannot leak a live listener between rounds.
    cmd.kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // The binder exists (we just built its command), so a spawn
            // failure is a *failed round*, not an environment skip —
            // counting it as skipped would mask the failure behind the
            // test's `ROUNDS - skipped` arithmetic.
            eprintln!("[race B] round {round}: spawn failed: {e}");
            return Some(false);
        }
    };

    // 4. Parent: read stdout line-by-line until `PORT=<n}` (or timeout).
    // One deadline for the *whole* wait: a child that emits log lines
    // continuously would otherwise restart a fresh `CHILD_TIMEOUT` on
    // every `read_line` and run this loop forever.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut reader = tokio::io::BufReader::new(&mut stdout);
    let mut line = String::new();
    let ready_deadline = Instant::now() + CHILD_TIMEOUT;
    let port = 'read: loop {
        let remaining = ready_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break 'read None;
        }
        match timeout(remaining, reader.read_line(&mut line)).await {
            Ok(Ok(0)) => break 'read None, // EOF before ready line
            Ok(Ok(_)) => {
                if let Some(rest) = line.trim().strip_prefix("PORT=")
                    && let Ok(p) = rest.trim().parse::<u16>()
                {
                    break 'read Some(p);
                }
                line.clear();
            }
            // IO error: the child died mid-read — treat like "no ready line".
            Ok(Err(_)) => break 'read None,
            Err(_) => break 'read None, // overall deadline elapsed
        }
    };
    let port = match port {
        Some(p) => p,
        None => {
            eprintln!("[race B] round {round}: no PORT=<n> ready line within {CHILD_TIMEOUT:?}");
            let _ = child.kill().await;
            return Some(false);
        }
    };

    // 5. Parent connects to the reported port (reachability check).
    let reachable = match timeout(CHILD_TIMEOUT, TcpStream::connect(("127.0.0.1", port))).await {
        Ok(Ok(_stream)) => true,
        other => {
            eprintln!("[race B] round {round}: connect to {port} failed: {other:?}");
            false
        }
    };
    let _ = child.kill().await;
    let _ = child.wait().await;
    Some(reachable)
}

/// Build the B-child command for this OS. `None` when no binder capable of
/// emitting the `PORT=<n>` ready line exists (e.g. only `nc` is available
/// on Linux).
fn self_select_command() -> Option<Command> {
    match std::env::consts::OS {
        "windows" => {
            // Keep the listener alive ~5s so the parent can connect; the
            // parent kills the child after verification. `powershell -File`
            // (not inline `-Command`, see `probe_bind_command` note).
            let script = probe_script_path("self-select.ps1")?;
            let mut cmd = Command::new("powershell");
            cmd.args(["-NoProfile", "-File", script.to_string_lossy().as_ref()]);
            Some(cmd)
        }
        _ => {
            if probe_path("python3") {
                let script = "import socket,time\ns=socket.socket()\n\
                    s.bind(('127.0.0.1',0))\n\
                    s.listen(1)\n\
                    print('PORT='+str(s.getsockname()[1]),flush=True)\n\
                    time.sleep(5);s.close()\n";
                let mut cmd = Command::new("python3");
                cmd.arg("-c").arg(script);
                Some(cmd)
            } else {
                None
            }
        }
    }
}

/// Run the whole spike. Prints the A/B statistics on stderr.
pub async fn run() {
    let mut a_lost = 0usize;
    let mut a_ok = 0usize;
    let mut a_probe_err = 0usize;
    let mut a_skipped = 0usize;
    for round in 0..ROUNDS {
        match race_preallocated_port(round).await {
            Some(RaceARound::LostRace) => a_lost += 1,
            Some(RaceARound::ChildOk) => a_ok += 1,
            Some(RaceARound::ProbeError) => a_probe_err += 1,
            None => a_skipped += 1,
        }
    }
    let a_raced = a_lost + a_ok;
    eprintln!(
        "[race A] N={ROUNDS}: child bind failed {} / raced {} (skipped {a_skipped}, probe errors {a_probe_err}) — \
         daemon pre-allocating a port is racy: the stealer won in \
         {}% of reproduced rounds",
        a_lost,
        a_raced,
        a_lost
            .checked_mul(100)
            .and_then(|n| n.checked_div(a_raced))
            .unwrap_or(0),
    );

    let mut b_ok = 0usize;
    let mut b_skipped = 0usize;
    for round in 0..ROUNDS {
        match child_self_selects_port(round).await {
            Some(true) => b_ok += 1,
            Some(false) => {}
            None => b_skipped += 1,
        }
    }
    let b_raced = ROUNDS - b_skipped;
    eprintln!(
        "[race B] N={ROUNDS}: child self-select :0 + ready line + TCP connect succeeded {b_ok} / {b_raced} (skipped {b_skipped})"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (A) Measure the pre-allocation race. No assertion on the failure
    /// rate (it is a probability — the ADR records the measured value);
    /// we only assert the harness actually ran some rounds.
    #[tokio::test]
    #[ignore = "100 rounds x 2 child spawns; run with `cargo test -p model-serving-spike -- --ignored`, findings in ADR-0001"]
    async fn race_a_preallocated_port_failure_rate() {
        let mut a_lost = 0usize;
        let mut a_ok = 0usize;
        let mut a_probe_err = 0usize;
        let mut a_skipped = 0usize;
        for round in 0..ROUNDS {
            match race_preallocated_port(round).await {
                Some(RaceARound::LostRace) => a_lost += 1,
                Some(RaceARound::ChildOk) => a_ok += 1,
                Some(RaceARound::ProbeError) => a_probe_err += 1,
                None => a_skipped += 1,
            }
        }
        let a_raced = a_lost + a_ok;
        eprintln!(
            "Spike C / A: child failed to bind daemon-probed port in {a_lost}/{a_raced} reproduced rounds (skipped {a_skipped}, probe errors {a_probe_err})"
        );
        assert!(
            a_raced > 0,
            "harness must run at least one raced round (python3/powershell available)"
        );
    }

    /// (B) Child self-selects :0, prints `PORT=<n>`, parent connects.
    /// Asserted: 100/100.
    #[tokio::test]
    async fn race_b_child_self_selects_port() {
        let mut ok = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;
        for round in 0..ROUNDS {
            match child_self_selects_port(round).await {
                Some(true) => ok += 1,
                Some(false) => failed += 1,
                None => skipped += 1,
            }
        }
        eprintln!(
            "Spike C / B: child self-selected port + ready line + connect succeeded {ok}/{ROUNDS} (skipped {skipped}, failed {failed})"
        );
        if skipped == ROUNDS {
            // No cross-platform binder available on this host at all —
            // report, do not fail the Windows-only dev run.
            eprintln!(
                "[race B] skipped ALL rounds: python3/powershell unavailable — verify on CI/WSL"
            );
            return;
        }
        assert_eq!(
            failed, 0,
            "child self-selected port must be reachable in every round"
        );
        assert_eq!(
            ok + failed,
            ROUNDS - skipped,
            "a transient spawn failure (counted as skipped) would break the 100/100 guarantee — every non-skipped round must have succeeded"
        );
    }
}
