# Model Launcher MVP Design

## 1. Product goal

Model Launcher is a low-overhead desktop application for discovering, loading, serving, and ejecting local GGUF models through `llama.cpp`. It provides a stable LM Studio-compatible HTTP surface while supervising a user-supplied inference engine. The first vertical slice targets Windows with `llama-server` running inside a user-selected WSL distribution.

The application remains useful while its main window is absent: its Rust core, tray icon, API gateway, and model supervisor stay alive, while the Slint window is destroyed when closed and recreated on demand.

### MVP scope

- Windows host application with a native-compiled Slint UI.
- User-selected WSL distribution and user-supplied `llama-server` executable.
- A recursively scanned Windows model directory containing GGUF files.
- One concurrently loaded model, with architecture prepared for multiple engines and multiple simultaneous models later.
- OpenAI-compatible Models, Chat Completions, and Completions endpoints, including streaming.
- LM Studio v1-compatible model list, load, and unload endpoints.
- Tray controls for opening the window, loading recent models, ejecting the current model, and quitting.
- Configurable local or LAN binding and optional Bearer token authentication.

### Explicit non-goals

- Downloading or updating `llama.cpp` or models.
- Embeddings, Responses API, Anthropic APIs, stateful chat, MCP, or model downloads.
- Running more than one model concurrently in the MVP.
- Native Windows, macOS, or Linux engines in the MVP.
- Automatically generating a form for every `llama-server` command-line argument.

## 2. Architecture

Model Launcher is a Rust workspace organized around a long-lived application core and replaceable boundary adapters.

### 2.1 Application core

The core owns:

- `ModelCatalog`: discovers GGUF models, groups shards, reads metadata, and maintains stable identity.
- `FeatureProbe`: executes engine version/help commands and turns output into typed capabilities.
- `LifecycleManager`: serializes load/eject/switch operations and supervises the child process.
- `ApiGateway`: exposes the stable client port, authenticates requests, implements management endpoints, and proxies inference requests.
- `ConfigStore`: persists global settings, engine settings, model identity, and per-model launch profiles.
- `LogStore`: retains bounded structured application and engine logs for UI display and export.

The core must not depend on Slint types. The UI observes immutable state snapshots and issues typed commands through an application service boundary.

### 2.2 Engine boundary

The core depends on an `InferenceEngine` abstraction rather than invoking `llama.cpp` directly. Its responsibilities are:

- report identity and version;
- probe typed capabilities;
- validate an engine-specific launch configuration;
- spawn a model server and return an owned process handle;
- check readiness and health;
- request graceful shutdown and force termination of the owned process when required.

The MVP implementation is `LlamaCppWslEngine`. It invokes `wsl.exe` with an argument vector containing the selected distribution, executable path, model path, internal host/port, and supported settings. Future native llama.cpp, MLX, or other engines can implement the same boundary without changing catalog, lifecycle, API, or UI semantics.

### 2.3 Process and port model

`model-launcher.exe` owns the public HTTP listener, the tray, configuration, and supervision state. `llama-server` listens on a separately allocated internal loopback port. The public base URL therefore remains stable while models restart or switch.

The WSL adapter records the exact Linux PID for the process it launches. Shutdown targets only that PID. It must never terminate processes by executable name.

### 2.4 UI lifetime

The Slint `SystemTrayIcon` and event loop remain alive without a main window. Closing the main window destroys its component and associated view state. Selecting Open from the tray creates a fresh window and hydrates it from a current core snapshot. Closing the window shows a one-time explanation that Model Launcher continues in the tray.

## 3. Windows and WSL integration

The user selects:

- one installed WSL distribution;
- the Linux path to `llama-server` inside that distribution;
- one Windows model root, such as `D:\Models`.

Windows drive paths are converted into WSL mount paths, for example `D:\Models\Qwen\model.gguf` to `/mnt/d/Models/Qwen/model.gguf`. Conversion is implemented as typed path logic and is unit tested for drive casing, separators, spaces, Unicode, UNC paths, invalid roots, and traversal-like input. Unsupported paths fail validation before spawning.

Commands are launched with structured process arguments. No user-controlled value is concatenated into a shell command. The adapter probes with `--version` and `--help` when settings are saved and again when the cached executable identity changes.

## 4. Model discovery and identity

The catalog recursively scans the configured Windows directory for `.gguf` files. It performs an initial scan, supports explicit rescan, and watches for filesystem changes with debounce.

Recognized sharded GGUF names are grouped into one logical model. The launch path points at the first shard and llama.cpp loads the remaining shards. A failed metadata read does not hide a model: the entry falls back to filename, total size, and path while retaining a diagnostic.

For readable models, the catalog extracts architecture, parameter count, quantization, context metadata, and size. Each logical model receives:

- an internal immutable UUID;
- a generated unique API key on first discovery;
- a user-editable API key;
- a display name;
- a best-effort file identity used to reconnect moved files;
- a saved launch profile.

API keys must be unique and URL-safe. Missing files remain as `missing` records so saved settings and names are not silently discarded. Users may remove a missing record or reconnect it when the files return.

## 5. Capability probing and settings

Model Launcher maintains a curated registry of useful settings rather than attempting to understand every possible llama.cpp flag. Initial settings are:

- context length;
- GPU layers;
- CPU threads;
- batch size;
- parallel slots;
- flash attention;
- KV cache type;
- internal host and port, which remain application-managed.

Each setting definition includes accepted help tokens, value type, validation, UI metadata, command-line rendering, and any known version constraints. A setting is exposed and rendered only when the current `--help` output confirms support. Unsupported values already present in a saved profile are retained but omitted from the command and reported in the UI.

Probe results include raw version output, raw help output, parsed capabilities, executable identity, and timestamp. A probe failure disables model loading and displays the original diagnostic.

## 6. Lifecycle state machine

All UI, tray, JIT, and management API operations enter one serialized lifecycle manager. The principal states are:

- `Stopped`
- `Starting`
- `Running`
- `Stopping`
- `Backoff`
- `FailedValidation`

Loading model B while model A is running stops A, waits for its port and process to exit, then starts B. Load succeeds only after the engine health endpoint is ready. If B fails to start, the application remains stopped and reports a structured error; it does not restore A.

Neither UI nor management-API replacement may interrupt active inference. When one or more inference requests are in flight, a request to load a different model returns `model_busy`; the UI disables Load actions for other models and explains why. Eject remains an explicit destructive operation: it cancels active upstream requests, stops the model, and causes affected clients to receive a terminated-stream or service-unavailable error.

An unexpected engine exit restarts the same desired model with exponential delays of 1, 2, 4, 8, 16, then 30 seconds, with 30 seconds as the cap. Five minutes of healthy operation resets the failure count. Explicit eject, model replacement, invalid configuration, or application shutdown clears the desired model and cancels backoff.

Graceful shutdown has a bounded timeout and then force-terminates only the owned WSL PID. Lifecycle transitions and cancellation are generation-tagged so stale health checks or restart timers cannot revive a replaced model.

## 7. API design

The default listener is `127.0.0.1:1234` without authentication. Users may select another address, including LAN addresses, and independently enable Bearer token authentication. Non-loopback binding without authentication produces a prominent warning but remains allowed.

### 7.1 OpenAI-compatible endpoints

- `GET /v1/models` lists all discovered models, including unloaded models.
- `POST /v1/chat/completions` proxies supported request fields and response headers to llama.cpp and preserves SSE streaming.
- `POST /v1/completions` behaves equivalently for text completions.

The gateway does not reinterpret inference payloads beyond routing and necessary model identity handling. It applies bounded request size, connection count, startup timeout, and safe header forwarding.

### 7.2 LM Studio-compatible management endpoints

- `GET /api/v1/models` returns LM Studio v1-style discovered model metadata and state.
- `POST /api/v1/models/load` accepts the LM Studio v1 load shape for the subset supported by the probed engine. It automatically ejects the current model, starts the requested model, waits for health, and returns load metadata.
- `POST /api/v1/models/unload` accepts `instance_id` and ejects the matching loaded model.

Unsupported optional LM Studio load fields produce a validation error rather than being silently accepted.

Compatibility is pinned to the public LM Studio v1 REST documentation captured on 2026-08-29. The repository keeps sanitized JSON fixtures for each supported request, success response, and error response. Contract tests compare semantic JSON values rather than object key order. The supported baseline fields are:

- list entries: `type`, `publisher`, `key`, `display_name`, `architecture`, `quantization`, `size_bytes`, and `params_string`;
- load request: `model`, `context_length`, `eval_batch_size`, `flash_attention`, `num_experts`, `offload_kv_cache_to_gpu`, and `echo_load_config` where the engine reports support;
- load response: `type`, `model_instance_id`, `load_time_seconds`, `status`, and optional `load_config`;
- unload request/response: `instance_id`.

Fields that Model Launcher cannot derive are returned as JSON `null` only where the LM Studio schema permits null; otherwise the field is omitted. Future LM Studio additions do not enter the MVP accidentally and require an explicit compatibility update.

### 7.3 JIT model loading

Inference requests use their `model` field as follows:

- no model is running and the key exists: load it, then proxy the request;
- the requested model is already running: proxy immediately;
- another model is running and idle: switch, then proxy;
- another model is running with one or more in-flight inference requests: return `409 model_busy`;
- the model key is unknown: return `404 model_not_found`.

During an explicit start or restart, requests return `503 model_starting` with `Retry-After`. Load failure returns `503 model_load_failed`. Concurrent JIT requests for the same model share the same in-progress load result rather than starting duplicate processes.

### 7.4 Authentication

Tokens are randomly generated and displayed only once. Only an Argon2 hash is persisted. Authorization values are redacted from logs. Authentication failures use a uniform response and do not disclose configuration details.

## 8. User interface

The accepted direction is `Quiet Native`: a light Windows 11-inspired presentation using Segoe UI, restrained blue accents, soft surfaces, and low-contrast separators. The main window uses a compact 38-pixel title bar and a 48-pixel horizontal navigation row. It does not reserve a sidebar for the product name or navigation.

### 8.1 Models

The Models page uses the full content width. It shows current engine status, public base URL, model search, rescan, compact model rows, metadata, and persistent Load/Eject actions. Narrow windows progressively hide secondary metadata while preserving the model name, state, and action.

Loading opens a native modal containing the editable model key and only settings supported by the active capability probe. Per-model values default from global settings when the model is first discovered.

### 8.2 Server, Logs, and Settings

- Server shows listener address, port, authentication state, warnings, and copy actions for base URL and newly generated token.
- Logs combines structured application and llama.cpp output with source/level filtering, bounded retention, copy, and export.
- Settings contains the model directory, WSL distribution, llama-server path, and global launch defaults. Saving engine settings performs validation and capability probing.

### 8.3 Tray

The native tray menu displays status and the active model, and provides:

- Open Model Launcher;
- Eject current model when applicable;
- a bounded Recent Models submenu for quick loading;
- Quit.

Tray state updates reactively from core snapshots. Quit stops accepting HTTP traffic, cancels restart work, stops the managed engine, flushes configuration, and exits.

## 9. Persistence and observability

Versioned JSON configuration is stored in the Windows user application-data directory. Writes use a temporary file and atomic replacement. A previous valid file is retained for recovery. Corrupt or unsupported configuration is quarantined with a visible diagnostic instead of overwritten.

Generated authentication tokens are removed from the live UI state immediately after copying. The system clipboard is cleared after 60 seconds only if it still contains that token; the expiry task retains only a SHA-256 digest, so a later user copy is preserved. Rust-owned token buffers are zeroed on a best-effort basis, but Slint `SharedString` and platform clipboard implementations do not guarantee zeroization, so this is not a claim that every in-memory copy is erased.

Structured logs include timestamp, source, level, lifecycle generation, model UUID, and message. The in-memory log store is bounded. Export excludes secrets and can include the cached version/help probe for diagnosis.

The application restores configuration and catalog state on restart but never automatically loads the previously running model.

## 10. Failure handling

- Invalid WSL distribution or executable: validation error with probe output; loading disabled.
- Unsupported Windows model path: model marked unlaunchable with a path diagnostic.
- Port conflict: choose a new internal port; fail clearly if the public port is occupied.
- Model metadata failure: retain a degraded catalog entry.
- Health timeout: stop the owned process, preserve its log tail, and return `model_load_failed`.
- Engine crash: enter cancellable exponential backoff.
- Configuration write failure: preserve in-memory state, report the error, and do not claim the change was persisted.
- API client disconnect: cancel upstream inference where safe and decrement the in-flight count exactly once.

## 11. Security and resource controls

- No shell interpolation of paths or settings.
- Strict validation of ports, numeric ranges, model keys, bind addresses, and engine capabilities.
- Request body, header, connection, and log retention limits.
- Authorization redaction and one-time token display.
- No arbitrary extra command-line field in the MVP.
- Only owned process identifiers may be terminated.
- Window recreation reads snapshots rather than retaining hidden UI state.

## 12. Testing and MVP acceptance

### Automated tests

- Unit tests for help parsing, capability rendering, Windows-to-WSL path conversion, shard grouping, identity persistence, lifecycle transitions, cancellation, and backoff.
- API contract tests for model list/load/unload, OpenAI request forwarding, error bodies, authentication, and byte-correct SSE streaming.
- Lifecycle integration tests with a fake engine covering readiness, crash, timeout, replacement, cancellation, and shared JIT loading.
- Persistence tests for atomic writes, migrations, corruption recovery, and missing/moved models.
- Slint view-model tests for action availability and responsive information priority where practical.

### Windows integration acceptance

Using a real WSL distribution, a user-provided llama-server, and a small GGUF model:

1. Configure and successfully probe WSL and llama.cpp.
2. Discover the model from a Windows directory and edit its API key.
3. Load from the UI, receive a healthy state, and complete streamed chat inference through port 1234.
4. Eject through the LM Studio-compatible endpoint.
5. Trigger JIT loading through an OpenAI request.
6. Confirm a different-model request returns `model_busy` during active generation.
7. Kill llama-server and observe capped exponential restart.
8. Eject during backoff and confirm it never restarts.
9. Close and reopen the main window from the tray without stopping the API or model.
10. Restart Model Launcher and confirm settings return while no model auto-loads.

### Resource acceptance

- Destroying the main window removes the Slint window/component and leaves only the core and tray UI.
- Fifty open/close cycles do not show sustained memory growth.
- Idle CPU usage remains negligible outside filesystem events, health checks, and restart activity.
- Logs and catalog caches remain bounded.

## 13. Delivery sequence

The implementation should proceed as a vertical slice:

1. Establish the Rust workspace, typed domain model, persistence, and fake engine.
2. Implement the lifecycle manager and test all transitions.
3. Implement model discovery, GGUF metadata, and stable identity.
4. Implement WSL probing, path conversion, spawn, health, and owned-process shutdown.
5. Implement the stable API gateway, management compatibility, proxying, authentication, and JIT loading.
6. Implement the Slint Quiet Native UI and native tray lifecycle.
7. Add Windows packaging, real WSL integration tests, resource checks, and release documentation.
