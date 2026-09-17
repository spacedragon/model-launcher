//! Real-runtime smoke test runner for llama.cpp (`docs/jobs.md` Job 8).
//!
//! Executes consecutive load-readiness-identity-inference-unload cycles
//! against a real `llama-server` binary and a real `.gguf` model.
//!
//! # Supported Engine Version
//!
//! - **Minimum supported build**: `llama.cpp` `b5555` (ADR-0003 candidate baseline).
//! - Verified to support loopback binding (`--host 127.0.0.1`), `--port`,
//!   `--alias`, `GET /health` (`{"status":"ok"}`), `GET /v1/models`, and
//!   `POST /v1/chat/completions`.
//!
//! # Usage
//!
//! ```bash
//! # Using CLI flags:
//! cargo run --package model-serving-runtime-llamacpp --bin llama_smoke_runner -- \
//!   --executable /path/to/llama-server \
//!   --model /path/to/model.gguf \
//!   --cycles 20
//!
//! # Or using environment variables:
//! LLAMACPP_SMOKE_EXECUTABLE=/path/to/llama-server \
//! LLAMACPP_SMOKE_MODEL=/path/to/model.gguf \
//! LLAMACPP_SMOKE_CYCLES=20 \
//! cargo run --package model-serving-runtime-llamacpp --bin llama_smoke_runner
//! ```

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use model_serving_domain::model::{ArtifactKind, LoadConfig, Model, Runtime, RuntimeKind};
use model_serving_runtime::{DoctorReport, ProbeConfig, SupervisorConfig};
use model_serving_runtime_llamacpp::{
    LaunchContext, LifecycleConfig, LlamaCppAdapter, MIN_SUPPORTED_BUILD,
};

fn print_help() {
    println!(
        r#"llama_smoke_runner — 20-cycle real-runtime smoke runner for llama.cpp

Supported Version:
  llama.cpp build b{MIN_SUPPORTED_BUILD}+ (ADR-0003 baseline)

Usage:
  llama_smoke_runner [OPTIONS]

Options:
  -e, --executable <PATH>   Path to llama-server executable
                            (or set LLAMACPP_SMOKE_EXECUTABLE)
  -m, --model <PATH>        Path to .gguf model file
                            (or set LLAMACPP_SMOKE_MODEL)
  -c, --cycles <N>          Number of load-infer-unload cycles (default: 20)
                            (or set LLAMACPP_SMOKE_CYCLES)
      --ctx-size <N>        Context size in tokens (default: 2048)
      --port <PORT>         Fixed loopback port (default: auto-allocated per cycle)
  -h, --help                Show this help message

Each cycle executes:
  1. Launch child in isolated process group via ManagedProcess
  2. Poll GET /health until {{"status":"ok"}} (startup readiness)
  3. Verify GET /v1/models exposes expected model identity
  4. Verify POST /v1/chat/completions minimal generation (max_tokens: 1)
  5. Clean ADR-0002 unload (TERM -> grace -> KILL)
  6. Confirm zero orphan processes remaining
"#
    );
}

struct RunnerConfig {
    executable: PathBuf,
    model_path: PathBuf,
    cycles: usize,
    ctx_size: u32,
    port: Option<u16>,
}

fn parse_args() -> Result<RunnerConfig, String> {
    let args: Vec<String> = std::env::args().collect();
    let mut executable: Option<String> = std::env::var("LLAMACPP_SMOKE_EXECUTABLE").ok();
    let mut model_path: Option<String> = std::env::var("LLAMACPP_SMOKE_MODEL").ok();
    let mut cycles: usize = std::env::var("LLAMACPP_SMOKE_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let mut ctx_size: u32 = 2048;
    let mut port: Option<u16> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-e" | "--executable" => {
                i += 1;
                if i >= args.len() {
                    return Err("missing value for --executable".to_owned());
                }
                executable = Some(args[i].clone());
            }
            "-m" | "--model" => {
                i += 1;
                if i >= args.len() {
                    return Err("missing value for --model".to_owned());
                }
                model_path = Some(args[i].clone());
            }
            "-c" | "--cycles" => {
                i += 1;
                if i >= args.len() {
                    return Err("missing value for --cycles".to_owned());
                }
                cycles = args[i]
                    .parse()
                    .map_err(|e| format!("invalid --cycles value: {e}"))?;
            }
            "--ctx-size" => {
                i += 1;
                if i >= args.len() {
                    return Err("missing value for --ctx-size".to_owned());
                }
                ctx_size = args[i]
                    .parse()
                    .map_err(|e| format!("invalid --ctx-size value: {e}"))?;
            }
            "--port" => {
                i += 1;
                if i >= args.len() {
                    return Err("missing value for --port".to_owned());
                }
                port = Some(
                    args[i]
                        .parse()
                        .map_err(|e| format!("invalid --port value: {e}"))?,
                );
            }
            other => return Err(format!("unknown option: {other}")),
        }
        i += 1;
    }

    let executable = executable.ok_or_else(|| {
        "missing executable; specify via --executable <PATH> or LLAMACPP_SMOKE_EXECUTABLE"
            .to_owned()
    })?;
    let model_path = model_path.ok_or_else(|| {
        "missing model; specify via --model <PATH> or LLAMACPP_SMOKE_MODEL".to_owned()
    })?;

    Ok(RunnerConfig {
        executable: PathBuf::from(executable),
        model_path: PathBuf::from(model_path),
        cycles,
        ctx_size,
        port,
    })
}

fn allocate_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind to free loopback port");
    listener.local_addr().expect("query local address").port()
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse_tasklist_csv_contains_pid(output: &str, pid: u32) -> Result<bool, String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Err("tasklist returned empty output".to_owned());
    }
    let wanted = pid.to_string();
    let mut found_any_valid_line = false;
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line
            .split(',')
            .map(|field| field.trim_matches('"').trim())
            .collect();
        if fields.len() < 2 {
            return Err(format!(
                "tasklist output is indeterminate (unparseable line: {line:?})"
            ));
        }
        if fields[1].parse::<u32>().is_err() {
            return Err(format!(
                "tasklist output is indeterminate (non-numeric PID field {:?} in line: {line:?})",
                fields[1]
            ));
        }
        found_any_valid_line = true;
        if fields[1] == wanted {
            return Ok(true);
        }
    }
    if !found_any_valid_line {
        return Err("tasklist output is indeterminate (no valid process records found)".to_owned());
    }
    Ok(false)
}

#[cfg(windows)]
fn is_process_alive(pid: u32) -> Result<bool, String> {
    let output = std::process::Command::new("tasklist")
        .arg("/NH")
        .arg("/FO")
        .arg("CSV")
        .output()
        .map_err(|err| format!("failed to spawn tasklist: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "tasklist exited with non-zero status ({:?}): {}",
            output.status.code(),
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_tasklist_csv_contains_pid(&stdout, pid)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn is_process_alive(pid: u32) -> Result<bool, String> {
    #[allow(unsafe_code)]
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
    Ok(alive)
}

#[cfg(not(any(unix, windows)))]
fn is_process_alive(pid: u32) -> Result<bool, String> {
    let _ = pid;
    Err("unsupported platform for orphan check".to_owned())
}

fn validate_environment(config: &RunnerConfig) -> Result<(), String> {
    if !config.executable.is_absolute() {
        return Err(format!(
            "executable path {} must be absolute",
            config.executable.display()
        ));
    }
    if !config.executable.exists() {
        return Err(format!(
            "executable not found at {}",
            config.executable.display()
        ));
    }
    if !config.model_path.exists() {
        return Err(format!(
            "model file not found at {}",
            config.model_path.display()
        ));
    }
    Ok(())
}

async fn diagnose_runtime(executable: &std::path::Path) -> Result<DoctorReport, String> {
    let probe_config = ProbeConfig::new();
    let report = LlamaCppAdapter::diagnose(executable, &probe_config).await;
    if !report.is_ready() {
        return Err(format!(
            "llama-server diagnostic check failed (status: {} [{}]: {})",
            report.status, report.code, report.message
        ));
    }
    Ok(report)
}

async fn run_single_cycle(
    cycle: usize,
    total: usize,
    port: u16,
    model: &Model,
    runtime: &Runtime,
    load_config: &LoadConfig,
    lifecycle_config: &LifecycleConfig,
) -> Result<(), String> {
    let cycle_start = Instant::now();
    println!("[Cycle {cycle}/{total}] Launching on port {port}...");

    let ctx = LaunchContext {
        model,
        config: load_config,
        runtime,
        port,
    };

    let mut lifecycle = LlamaCppAdapter::launch(&ctx, lifecycle_config.clone())
        .map_err(|err| format!("launch error: {err}"))?;

    let pid = lifecycle.pid();
    print!("  -> PID {pid}, waiting for readiness (/health)... ");

    let ready_start = Instant::now();
    if let Err(err) = lifecycle.wait_ready().await {
        println!("FAILED ({:.1?})", ready_start.elapsed());
        let stderr_tail = lifecycle.stderr_tail();
        let stderr = String::from_utf8_lossy(&stderr_tail);
        eprintln!("--- Stderr Tail ---\n{stderr}\n-------------------");
        return Err(format!("readiness failed: {err}"));
    }
    println!("READY ({:.1?})", ready_start.elapsed());

    print!("  -> Verifying model identity (/v1/models)... ");
    if let Err(err) = lifecycle.verify_identity().await {
        println!("FAILED");
        return Err(format!("identity verification failed: {err}"));
    }
    println!("VERIFIED (key: {})", model.key);

    print!("  -> Verifying inference (/v1/chat/completions)... ");
    let infer_start = Instant::now();
    if let Err(err) = lifecycle.verify_inference().await {
        println!("FAILED ({:.1?})", infer_start.elapsed());
        return Err(format!("inference verification failed: {err}"));
    }
    println!("OK ({:.1?})", infer_start.elapsed());

    print!("  -> Unloading child process (TERM -> grace -> KILL)... ");
    let unload_start = Instant::now();
    if let Err(err) = lifecycle.unload().await {
        println!("FAILED ({:.1?})", unload_start.elapsed());
        return Err(format!("unload failed: {err}"));
    }
    println!("UNLOADED ({:.1?})", unload_start.elapsed());

    // Confirm child process cleanup (no-orphan verification)
    tokio::time::sleep(Duration::from_millis(150)).await;
    let alive =
        is_process_alive(pid).map_err(|err| format!("orphan check failed for PID {pid}: {err}"))?;
    if alive {
        return Err(format!(
            "process {pid} is still alive after unload! (orphan detected)"
        ));
    }
    println!("  -> Orphan check: PID {pid} successfully terminated.");
    println!(
        "  -> Cycle {cycle} complete in {:.2?}.\n",
        cycle_start.elapsed()
    );
    Ok(())
}

fn create_smoke_context(
    config: &RunnerConfig,
    doctor_report: &DoctorReport,
) -> (Model, Runtime, LoadConfig, LifecycleConfig) {
    let model_key = config
        .model_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("smoke-model")
        .to_owned();

    let model = Model {
        id: "smoke-model-id".to_owned(),
        key: model_key,
        path: config.model_path.to_string_lossy().into_owned(),
        artifact_kind: ArtifactKind::Gguf,
        size_bytes: 0,
        mtime: chrono::Utc::now(),
        display_name: None,
        default_runtime_id: None,
        default_load_config: None,
        metadata: None,
        deleted: false,
    };

    let runtime = Runtime {
        id: "smoke-runtime-id".to_owned(),
        kind: RuntimeKind::LlamaCpp,
        executable_path: config.executable.to_string_lossy().into_owned(),
        enabled: true,
        version_text: doctor_report.version_text.clone(),
        capabilities: doctor_report.capabilities.clone(),
        fixed_args: vec![],
    };

    let load_config = LoadConfig {
        context_length: config.ctx_size,
        max_concurrency: Some(1),
        eval_batch_size: None,
        flash_attention: None,
        offload_kv_cache_to_gpu: None,
        n_gpu_layers: None,
        engine_config: None,
    };

    let lifecycle_config = LifecycleConfig {
        supervisor: SupervisorConfig {
            startup_timeout: Duration::from_secs(120),
            probe_timeout: Duration::from_secs(3),
            probe_interval: Duration::from_millis(500),
            shutdown_grace: Duration::from_secs(5),
            post_kill_timeout: Duration::from_secs(5),
            pipe_eof_timeout: Duration::from_secs(5),
            ..SupervisorConfig::default()
        },
        http_timeout: Duration::from_secs(30),
        http_connect_timeout: Duration::from_secs(5),
    };

    (model, runtime, load_config, lifecycle_config)
}

async fn execute_smoke_cycles(
    config: &RunnerConfig,
    model: &Model,
    runtime: &Runtime,
    load_config: &LoadConfig,
    lifecycle_config: &LifecycleConfig,
) -> Result<(), String> {
    let start_all = Instant::now();
    for cycle in 1..=config.cycles {
        let port = config.port.unwrap_or_else(allocate_free_port);
        run_single_cycle(
            cycle,
            config.cycles,
            port,
            model,
            runtime,
            load_config,
            lifecycle_config,
        )
        .await?;
    }
    let elapsed_total = start_all.elapsed();
    println!("================================================================");
    println!(
        " [PASS] Successfully completed {}/{} cycles in {:.2?}.",
        config.cycles, config.cycles, elapsed_total
    );
    println!(" Zero orphan processes detected across all cycles.");
    println!("================================================================");
    Ok(())
}

#[tokio::main]
async fn main() {
    let config = match parse_args() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("Error: {err}");
            eprintln!("Run with --help for usage details.");
            std::process::exit(1);
        }
    };

    println!("================================================================");
    println!(" llama.cpp Real-Runtime Smoke Test Runner (Job 8)");
    println!(" Supported Version: llama.cpp b{MIN_SUPPORTED_BUILD}+ (ADR-0003 baseline)");
    println!(" Executable:       {}", config.executable.display());
    println!(" Model:            {}", config.model_path.display());
    println!(" Target Cycles:    {}", config.cycles);
    println!(" Context Length:   {} tokens", config.ctx_size);
    println!("================================================================");

    if let Err(err) = validate_environment(&config) {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }

    let doctor_report = match diagnose_runtime(&config.executable).await {
        Ok(report) => {
            let version = report.version_text.as_deref().unwrap_or("unknown");
            println!("  -> Runtime preflight: OK (version: {version})");
            report
        }
        Err(err) => {
            eprintln!("Error: {err}");
            std::process::exit(1);
        }
    };

    let (model, runtime, load_config, lifecycle_config) =
        create_smoke_context(&config, &doctor_report);

    if let Err(err) =
        execute_smoke_cycles(&config, &model, &runtime, &load_config, &lifecycle_config).await
    {
        eprintln!("[FAIL] {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tasklist_finds_matching_pid() {
        let sample = r#""smss.exe","408","Services","0","1,234 K"
"llama-server.exe","12345","Console","1","50,000 K"
"explorer.exe","5678","Console","1","80,000 K""#;
        assert_eq!(parse_tasklist_csv_contains_pid(sample, 12345), Ok(true));
        assert_eq!(parse_tasklist_csv_contains_pid(sample, 99999), Ok(false));
    }

    #[test]
    fn parse_tasklist_rejects_empty_output() {
        assert!(parse_tasklist_csv_contains_pid("", 12345).is_err());
        assert!(parse_tasklist_csv_contains_pid("   \r\n  ", 12345).is_err());
    }

    #[test]
    fn parse_tasklist_rejects_indeterminate_non_csv_or_info_message() {
        let info = "INFO: No tasks are running which match the specified criteria.";
        assert!(parse_tasklist_csv_contains_pid(info, 12345).is_err());
    }

    #[test]
    fn parse_tasklist_rejects_non_numeric_pid() {
        let bad = r#""Image Name","PID","Session Name""#;
        assert!(parse_tasklist_csv_contains_pid(bad, 12345).is_err());
    }
}
