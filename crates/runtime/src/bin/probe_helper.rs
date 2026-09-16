//! Test-support fake runtime used by `tests/probe.rs`.
//!
//! This binary is **not** part of the product. The integration tests build it
//! as `CARGO_BIN_EXE_probe_helper` and use it as a controllable child so the
//! probe pipeline (success, non-zero exit, timeout, output cap, malformed
//! output) is exercised against a real process on every platform, without
//! shell scripts or platform-specific tricks.
//!
//! Behaviour is selected by the `PROBE_HELPER_MODE` environment variable:
//!
//! - unset / anything else: behaves like a normal engine, printing a version
//!   for `--version` and help for `--help`
//! - `nonzero`: exits with code 7
//! - `sleep`: hangs until killed (for timeout tests)
//! - `sleep_help`: prints a version for `--version`, hangs on `--help`
//! - `grow`: writes ~2 MiB to stdout (for the stdout cap)
//! - `grow_err`: writes ~2 MiB to stderr (for the stderr cap)
//! - `malformed`: writes non-UTF-8 bytes to stdout
//! - `empty`: prints nothing and exits 0
//! - `descendant`: spawns a child that inherits our stdout/stderr, then exits
//!   immediately while the child keeps the pipes open (~2s). Used to prove the
//!   probe's deadline bounds pipe draining.
//! - `descendant_hold`: sleeps ~5s (the child spawned by `descendant`)

use std::io::Write as _;

/// Bytes written by the `grow` / `grow_err` modes: 256 * 8192 = 2 MiB.
const GROW_CHUNKS: usize = 256;
/// One chunk of the `grow` output.
const GROW_CHUNK: [u8; 8192] = [b'x'; 8192];

fn main() {
    let mode = std::env::var("PROBE_HELPER_MODE").unwrap_or_default();
    let argument = std::env::args().nth(1).unwrap_or_default();

    match mode.as_str() {
        "nonzero" => {
            let _ = writeln!(std::io::stderr(), "probe-helper: refusing to run");
            std::process::exit(7);
        }
        "sleep" => std::thread::sleep(std::time::Duration::from_secs(3600)),
        "sleep_help" => {
            if argument == "--help" {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            } else {
                let _ = writeln!(std::io::stdout(), "probe-helper version 1.2.3");
            }
        }
        "grow" => {
            let mut stdout = std::io::stdout().lock();
            for _ in 0..GROW_CHUNKS {
                let _ = stdout.write_all(&GROW_CHUNK);
            }
        }
        "grow_err" => {
            let mut stderr = std::io::stderr().lock();
            for _ in 0..GROW_CHUNKS {
                let _ = stderr.write_all(&GROW_CHUNK);
            }
        }
        "malformed" => {
            let _ = std::io::stdout().write_all(&[0xff, 0xfe, 0xfd]);
        }
        "descendant" => {
            // A real engine can fork a helper that inherits the probe pipes and
            // outlives the parent. Spawn one and exit so the pipes stay open.
            if let Ok(executable) = std::env::current_exe() {
                let _ = std::process::Command::new(executable)
                    .env("PROBE_HELPER_MODE", "descendant_hold")
                    .stdin(std::process::Stdio::null())
                    .spawn();
            }
        }
        "descendant_hold" => {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        "empty" => {}
        _ => match argument.as_str() {
            "--version" => {
                let _ = writeln!(std::io::stdout(), "probe-helper version 1.2.3");
            }
            "--help" => {
                let _ = writeln!(
                    std::io::stdout(),
                    "Usage: probe-helper [--version]\nSupports chat completions."
                );
            }
            _ => {
                let _ = writeln!(std::io::stderr(), "unknown argument: {argument}");
                std::process::exit(2);
            }
        },
    }
}
