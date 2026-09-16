//! Test-support fake engine used by `tests/supervisor.rs`.
//!
//! This binary is **not** part of the product. The integration tests build it
//! as `CARGO_BIN_EXE_supervisor_helper` and use it as a controllable child so
//! the `ManagedProcess` supervisor is exercised against a real process on
//! every platform, without shell scripts or platform-specific tricks.
//!
//! Behaviour is selected by the `SUPERVISOR_HELPER_MODE` environment variable:
//!
//! - `verbose`: writes ~256 bytes on **both** stdout and stderr every ~10 ms,
//!   forever (>64 KiB per stream in ~2.5 s) — proves the background drain
//!   keeps a chatty child from deadlocking on a full pipe.
//! - `clean`: prints a line to stdout, exits 0.
//! - `crash`: prints a line to stderr, exits 3.
//! - `oom`: prints a conservative CUDA OOM signature, exits 7.
//! - `sleep`: sleeps forever (drop-fallback / plain-TERM subjects).
//! - `ignores_term` (unix): `SIG_IGN` for SIGTERM, then sleeps forever —
//!   only the group SIGKILL escalation can end it.
//! - `grandchild` (unix): spawns `sleep 100`, which inherits this process's
//!   pipes **and stays in the same process group**, then sleeps forever —
//!   the supervisor can only reach pipe EOF by killing the whole group.
//! - `leader_exit_grandchild` (unix): spawns the same grandchild, waits long
//!   enough for an ownership snapshot, then exits while the descendant lives.
//! - anything else (or a unix-only mode on Windows): stderr message, exit 5.

use std::io::Write;
use std::time::Duration;

/// Bytes written per tick on each stream.
const TICK_BYTES: usize = 256;

fn main() {
    let mode = std::env::var("SUPERVISOR_HELPER_MODE").unwrap_or_default();

    match mode.as_str() {
        "verbose" => {
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            let mut sequence: u64 = 0;
            loop {
                let stdout_line = format!("s:{sequence}:{}", "x".repeat(TICK_BYTES));
                let stderr_line = format!("e:{sequence}:{}", "y".repeat(TICK_BYTES));
                let _ = writeln!(out, "{stdout_line}");
                let _ = writeln!(err, "{stderr_line}");
                sequence += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "clean" => {
            let _ = writeln!(std::io::stdout(), "clean: ready");
            std::process::exit(0);
        }
        "crash" => {
            let _ = writeln!(std::io::stderr(), "crash: simulated failure");
            std::process::exit(3);
        }
        "oom" => {
            let _ = writeln!(std::io::stderr(), "CUDA out of memory while loading model");
            std::process::exit(7);
        }
        "sleep" => sleep_forever(),
        "ignores_term" => {
            #[cfg(unix)]
            {
                // SAFETY: `SIG_IGN` is a valid POSIX disposition for SIGTERM
                // and this helper performs no other signal handling.
                // SIG_IGN survives across the group TERM, so only KILL ends
                // the process — the escalation subject.
                #[allow(unsafe_code)]
                unsafe {
                    libc::signal(libc::SIGTERM, libc::SIG_IGN);
                }
            }
            #[cfg(not(unix))]
            unsupported(&mode);
            let _ = writeln!(std::io::stdout(), "ignores_term: armed");
            sleep_forever();
        }
        "grandchild" => {
            #[cfg(unix)]
            {
                // The grandchild inherits our (piped) stdout/stderr and, by
                // default, our process group — exactly the ADR-0002 scenario
                // where a surviving descendant would hold the pipes open.
                match std::process::Command::new("sleep").arg("100").spawn() {
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("supervisor_helper: cannot spawn grandchild: {error}");
                        std::process::exit(5);
                    }
                }
                let _ = writeln!(std::io::stdout(), "grandchild: spawned");
            }
            #[cfg(not(unix))]
            unsupported(&mode);
            sleep_forever();
        }
        "leader_exit_grandchild" => {
            #[cfg(unix)]
            {
                match std::process::Command::new("sleep").arg("100").spawn() {
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("supervisor_helper: cannot spawn grandchild: {error}");
                        std::process::exit(5);
                    }
                }
                let _ = writeln!(std::io::stdout(), "leader_exit_grandchild: spawned");
                std::thread::sleep(Duration::from_millis(300));
                std::process::exit(0);
            }
            #[cfg(not(unix))]
            unsupported(&mode);
        }
        other => {
            eprintln!("supervisor_helper: unknown mode {other}");
            std::process::exit(5);
        }
    }
}

/// Never return; the process lives until the supervisor terminates it.
fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// (non-unix) Unix-only mode requested: report and exit non-zero so a test
/// that accidentally used it fails loudly instead of hanging.
#[cfg(not(unix))]
fn unsupported(mode: &str) {
    eprintln!("supervisor_helper: mode {mode} is unix-only");
    std::process::exit(5);
}
