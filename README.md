# Model Launcher

Model Launcher is a small desktop supervisor for a user-supplied `llama-server` in WSL. It discovers GGUF models in a Windows directory, keeps one model active, and exposes a stable LM Studio management API plus OpenAI-compatible inference endpoints at `127.0.0.1:1234`.

## Prerequisites

- Rust 1.89 with Cargo (the workspace uses Rust 2024 edition).
- Windows 10/11 with WSL 2 and an installed distribution, for example `Ubuntu`.
- A working `llama-server` inside that distribution. Model Launcher does not download or update llama.cpp.
- A Windows directory containing a legally obtained GGUF model. Model Launcher does not provide models.
- Enough RAM/VRAM for the selected model and context. llama.cpp hardware/backend support remains the user's responsibility.

Check the external pieces before starting:

```powershell
wsl --list --verbose
wsl -d Ubuntu -- /usr/local/bin/llama-server --help
rustc --version
```

## Build, configure, and run

```powershell
rustup toolchain install 1.89 --component rustfmt,clippy
cargo +1.89 build -p model-launcher --release
./target/release/model-launcher.exe
```

In **Settings**, choose the WSL distribution, the absolute Linux path to `llama-server`, and the Windows model directory, then save. The application probes `--help` before enabling supported launch controls. Use **Models** to rescan, edit a stable API key if desired, and load a model. Closing the window keeps the core/API in the tray; **Quit** shuts down the owned backend.

Configuration is versioned JSON at `%LOCALAPPDATA%\ModelLauncher\config\config.json`. Corrupt or unsupported files are quarantined. Do not hand-edit it while Model Launcher is running. The application never auto-loads a model after restart.

### Authentication and LAN safety

The desktop build listens on loopback and requires a bearer token. Generate it in **Settings**; plaintext is displayed once and only Argon2 PHC hashes are persisted. Store the token in a password manager and rotate it if exposed. Examples below assume:

```powershell
$env:ML_TOKEN = "paste-the-one-time-token"
```

Never expose an unauthenticated server on a LAN. Loopback is the supported MVP binding. If a future/custom build permits non-loopback binding, use token authentication plus host firewall rules and treat every prompt, model name, and completion as network-visible sensitive data. Do not put tokens in URLs, logs, screenshots, or shell history.

## HTTP API examples

Set `BASE=http://127.0.0.1:1234`, `TOKEN` to the one-time token, `MODEL` to a key returned by the list call, and preserve `INSTANCE` from load. These examples use `curl` and `jq` so untrusted model/instance strings are encoded as JSON rather than interpolated; PowerShell users should call `curl.exe` to avoid legacy aliases or build bodies with `ConvertTo-Json`.

```bash
BASE=http://127.0.0.1:1234
TOKEN='replace-me'
MODEL='publisher/model-key'

# LM Studio-compatible catalog
curl -fsS "$BASE/api/v1/models" -H "Authorization: Bearer $TOKEN"

# Load; save model_instance_id from the response as INSTANCE
curl -fsS "$BASE/api/v1/models/load" -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg model "$MODEL" '{model:$model,context_length:4096,echo_load_config:true}')"

# Unload the exact owned instance
curl -fsS "$BASE/api/v1/models/unload" -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -d "$(jq -n --arg instance "$INSTANCE" '{instance_id:$instance}')"

# OpenAI-compatible catalog
curl -fsS "$BASE/v1/models" -H "Authorization: Bearer $TOKEN"

# Chat, non-streaming
curl -fsS "$BASE/v1/chat/completions" -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg model "$MODEL" '{model:$model,stream:false,messages:[{role:"user",content:"Say hello"}]}')"

# Chat, byte-preserving SSE streaming (-N disables curl buffering)
curl -N "$BASE/v1/chat/completions" -H "Authorization: Bearer $TOKEN" \
  -H 'Accept: text/event-stream' -H 'Content-Type: application/json' \
  -d "$(jq -n --arg model "$MODEL" '{model:$model,stream:true,messages:[{role:"user",content:"Count to three"}]}')"

# Text completion, non-streaming
curl -fsS "$BASE/v1/completions" -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg model "$MODEL" '{model:$model,stream:false,prompt:"Once upon a time"}')"

# Text completion, SSE streaming
curl -N "$BASE/v1/completions" -H "Authorization: Bearer $TOKEN" \
  -H 'Accept: text/event-stream' -H 'Content-Type: application/json' \
  -d "$(jq -n --arg model "$MODEL" '{model:$model,stream:true,prompt:"Count to three"}')"
```

An inference request can JIT-load its named model when idle. A different model requested during active generation receives `model_busy`; retry after the current lease ends or explicitly unload. Management endpoints are JSON-only and do not have streaming variants.

## Troubleshooting

- **Probe fails:** run the exact `wsl -d <Distro> -- <LlamaServer> --help` command. Verify case-sensitive Linux path, execute permission, shared-library dependencies, and that the configured distribution matches `wsl --list --quiet`.
- **Model missing/unlaunchable:** rescan and verify the GGUF is below the configured root. Drive-letter paths are mapped to `/mnt/<drive>/...`; UNC paths, traversal-like input, roots through symlinks, and paths outside the configured root are rejected. Confirm the drive is mounted inside WSL.
- **401:** create a new token in Settings and update the `Authorization: Bearer ...` header. A token cannot be recovered from its Argon2 hash.
- **Startup/health timeout:** run llama-server directly with the model path translated to WSL, inspect **Logs**, check RAM/VRAM, and ensure its internal loopback port is not stolen. The public port remains 1234.
- **Crash/backoff:** Model Launcher restarts only its recorded child PID with capped backoff. Eject cancels pending restart. It never kills processes by name, wildcard, or unverified PID; do not “fix” a conflict by terminating every `llama-server`.
- **Port 1234 busy:** identify the owner (`Get-NetTCPConnection -LocalPort 1234`) and stop it only if you own and recognize it.

## Limits and current scope

Requests are limited to 8 MiB bodies, 64 headers/32 KiB aggregate header bytes, 128 connections, and 128 in-flight requests. Upstream connect/header timeouts are 5/30 seconds. Desktop logs retain at most 2,000 records and 2 MiB of message text; the catalog caps discovery at 1,024 GGUF files/models and bounds diagnostics/metadata/tensor descriptors. Long-running inference is ultimately constrained by llama.cpp and machine resources.

The MVP supports one active model, a Windows host plus WSL engine, local GGUF discovery, and the six endpoints above. It does not install WSL/llama.cpp/models, expose arbitrary llama.cpp flags, provide TLS, support remote administration, or promise compatibility with every llama.cpp revision/GGUF architecture.

## Verification

Automated checks run on Windows, macOS, and Linux:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace --release
ruby tests/windows-wsl/validate.rb
```

Real WSL acceptance is deliberately explicit because ordinary CI cannot supply a user's distribution, executable, model, token, or GPU. The dispatch workflow runs only the noninteractive automated smoke on an ephemeral, trusted self-hosted Windows/WSL runner through the protected `windows-wsl-acceptance` environment; configure a required reviewer and allow only `main`. Configure its optional `MODEL_LAUNCHER_SMOKE_TOKEN` Actions secret (never a dispatch input); the matching Argon2 hash and model root must already be in that runner user's Model Launcher config. The harness never copies that config into artifacts. Destroy/clean the runner and its user profile after evidence upload. Interactive tray/resource observations are local-only. See [tests/windows-wsl/README.md](tests/windows-wsl/README.md). No real-Windows result is inferred from a cross-compile or from tests on macOS/Linux.
