# Model Launcher MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a Windows desktop MVP that discovers GGUF models, runs a user-supplied WSL `llama-server`, exposes LM Studio-compatible management plus OpenAI-compatible inference APIs, and remains available from a low-overhead native tray.

**Architecture:** A Rust workspace separates a UI-independent application core from engine, HTTP, persistence, and Slint adapters. A serialized lifecycle actor owns the desired model and child process; both UI commands and HTTP requests use that actor. The public Axum gateway stays on a stable port and proxies to a dynamically allocated internal llama.cpp port.

**Tech Stack:** Rust 1.89, Tokio 1.x, Axum 0.8, Reqwest 0.13, Serde, Slint 1.16.1 (compatible with Rust 1.89), notify 8.2, gguf-rs-lib 0.3, Argon2, tracing, uuid, Windows `wsl.exe`.

**Reference spec:** `docs/superpowers/specs/2026-08-29-model-launcher-design.md`

---

## File structure

```text
Cargo.toml                         Workspace and shared dependency versions
rustfmt.toml                      Formatting policy
crates/model-launcher-core/
  src/lib.rs                      Public core API
  src/model.rs                    Model identity, metadata, and launch profiles
  src/capability.rs               Typed engine capabilities and launch settings
  src/engine.rs                   InferenceEngine and EngineProcess traits
  src/lifecycle.rs                Serialized lifecycle actor and state machine
  src/catalog.rs                  Recursive scan, shard grouping, identity merge
  src/config.rs                   Versioned settings and atomic persistence
  src/log.rs                      Bounded structured logs, redaction, and export
  src/error.rs                    Stable application/API error taxonomy
crates/model-launcher-wsl/
  src/lib.rs                      WSL engine adapter exports
  src/path.rs                     Windows-to-WSL path conversion
  src/probe.rs                    Version/help execution and parsing
  src/port.rs                     Internal loopback allocation and conflict retry
  src/process.rs                  Spawn, health, PID ownership, and shutdown
crates/model-launcher-api/
  src/lib.rs                      Gateway construction and server handle
  src/auth.rs                     Optional Bearer authentication
  src/models.rs                   OpenAI and LM Studio model/list schemas
  src/management.rs               List/load/unload handlers
  src/proxy.rs                    Chat/completions proxy and SSE streaming
  tests/fixtures/*.json           Pinned LM Studio-compatible JSON fixtures
  tests/contracts.rs              HTTP compatibility tests
crates/model-launcher-ui/
  build.rs                        Slint compiler setup
  ui/app.slint                    Quiet Native shell and pages
  ui/components/*.slint           Focused model row, status, modal components
  src/lib.rs                      View model and callback binding
  src/tray.rs                     SystemTrayIcon state and commands
apps/model-launcher/
  src/main.rs                     Composition root and shutdown ordering
  resources/                      Icons and Windows resources
tests/fake-llama-server/
  src/main.rs                     Controllable engine/API test double
tests/windows-wsl/                Ignored real-WSL smoke tests and README
```

## Task 1: Bootstrap the workspace and domain contracts

**Files:**
- Create: `Cargo.toml`
- Create: `rustfmt.toml`
- Create: `crates/model-launcher-core/Cargo.toml`
- Create: `crates/model-launcher-core/src/{lib,error,model,capability,engine}.rs`
- Test: inline unit tests in the modules above

- [x] **Step 1: Add a compile-failing domain test**

Define tests for model-key validation, capability-gated launch arguments, and stable error codes before implementations. Representative assertion:

```rust
assert_eq!(ModelKey::parse("Qwen/qwen3-8b-q4")?.as_str(), "Qwen/qwen3-8b-q4");
assert!(ModelKey::parse("../escape").is_err());
assert_eq!(settings.to_args(&caps), vec!["--ctx-size", "8192"]);
```

- [x] **Step 2: Verify the tests fail**

Run: `cargo test -p model-launcher-core model_key --no-fail-fast`

Expected: compilation fails because the domain types are not implemented.

- [x] **Step 3: Implement minimal typed contracts**

Implement `ModelId(Uuid)`, `ModelKey`, `ModelRecord`, `ModelState`, `LaunchProfile`, typed setting values, `EngineCapabilities`, `EngineSpec`, `InferenceEngine`, `EngineProcess`, and `AppError`. Keep async engine methods object-safe with boxed futures or `async-trait`.

- [x] **Step 4: Run focused and workspace tests**

Run: `cargo test -p model-launcher-core && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`

Expected: all pass.

- [x] **Step 5: Commit**

```bash
git add Cargo.toml rustfmt.toml crates/model-launcher-core
git commit -m "feat: establish model launcher core contracts"
```

## Task 2: Implement versioned persistence

**Files:**
- Create: `crates/model-launcher-core/src/config.rs`
- Modify: `crates/model-launcher-core/src/lib.rs`
- Test: `crates/model-launcher-core/tests/config_store.rs`

- [x] **Step 1: Write failing persistence tests**

Cover default config, round trip, atomic replacement, retained backup, corrupt-file quarantine, model UUID/key persistence, missing model preservation, and migration from a version-0 fixture.

- [x] **Step 2: Verify failure**

Run: `cargo test -p model-launcher-core --test config_store`

Expected: fails because `ConfigStore` does not exist.

- [x] **Step 3: Implement `ConfigStore`**

Use a versioned Serde envelope. Write to a sibling temporary file, sync, rename, and retain the last valid backup. Inject the config directory into tests rather than reading real user directories.

- [x] **Step 4: Verify persistence behavior**

Run: `cargo test -p model-launcher-core --test config_store && cargo clippy -p model-launcher-core --all-targets -- -D warnings`

Expected: all pass; tests leave no files outside their temp directories.

- [x] **Step 5: Commit**

```bash
git add crates/model-launcher-core
git commit -m "feat: persist versioned launcher configuration"
```

## Task 3: Build the lifecycle actor with a fake engine

**Files:**
- Create: `crates/model-launcher-core/src/lifecycle.rs`
- Modify: `crates/model-launcher-core/src/lib.rs`
- Test: `crates/model-launcher-core/tests/lifecycle.rs`

- [x] **Step 1: Write paused-time lifecycle tests**

Use `tokio::time::pause` and a scripted fake `InferenceEngine`. Cover stopped→starting→running, load readiness, replacement, replacement rejected while busy, explicit eject cancellation, crash backoff 1/2/4/8/16/30 seconds, five-minute reset, stale-generation suppression, and shared same-model JIT load.

- [x] **Step 2: Verify failure**

Run: `cargo test -p model-launcher-core --test lifecycle`

Expected: fails because the lifecycle actor is missing.

- [x] **Step 3: Implement the actor**

Use one Tokio task receiving typed commands over `mpsc`; publish immutable snapshots through `watch`. Track desired model, generation, in-flight count, process handle, and restart attempt. Never hold a mutex across engine awaits.

- [x] **Step 4: Verify deterministic transitions**

Run: `cargo test -p model-launcher-core --test lifecycle -- --nocapture`

Expected: all transition and paused-time tests pass without wall-clock sleeps.

- [x] **Step 5: Commit**

```bash
git add crates/model-launcher-core
git commit -m "feat: supervise model lifecycle and restart backoff"
```

## Task 3A: Add bounded structured logging

**Files:**
- Create: `crates/model-launcher-core/src/log.rs`
- Modify: `crates/model-launcher-core/src/lib.rs`
- Test: `crates/model-launcher-core/tests/log_store.rs`

- [x] **Step 1: Write failing log-store tests**

Cover fixed record/byte retention limits, timestamp/source/level/generation/model fields, stdout/stderr line framing, source/level filtering, deterministic export, Authorization and Bearer-token redaction, and oversized-line truncation.

- [x] **Step 2: Verify failure**

Run: `cargo test -p model-launcher-core --test log_store`

Expected: fails because `LogStore` is absent.

- [x] **Step 3: Implement `LogStore` and engine stream ingestion**

Use a bounded `VecDeque<LogRecord>` behind a focused store API. Apply redaction before records enter storage, broadcast appended records over a bounded channel, count dropped records, and export snapshots rather than holding a lock during I/O. Accept engine stdout/stderr as byte streams and safely frame lossy UTF-8 lines with a maximum line length.

- [x] **Step 4: Verify limits and redaction**

Run: `cargo test -p model-launcher-core --test log_store && cargo clippy -p model-launcher-core --all-targets -- -D warnings`

Expected: all tests pass and no secret fixture appears in snapshots or export.

- [x] **Step 5: Commit**

```bash
git add crates/model-launcher-core
git commit -m "feat: add bounded redacted structured logs"
```

## Task 4: Discover GGUF models and preserve identity

**Files:**
- Create: `crates/model-launcher-core/src/catalog.rs`
- Modify: `crates/model-launcher-core/src/lib.rs`
- Test: `crates/model-launcher-core/tests/catalog.rs`
- Test fixtures: `crates/model-launcher-core/tests/fixtures/`

- [ ] **Step 1: Write failing catalog tests**

Create tiny generated GGUF fixtures and filename-only fixtures. Cover recursive discovery, case-insensitive extension, shard grouping, malformed metadata fallback, duplicate generated keys, user-renamed keys, moved-file reconnection, missing records, explicit removal of a missing record, and debounced reconciliation.

- [ ] **Step 2: Verify failure**

Run: `cargo test -p model-launcher-core --test catalog`

Expected: catalog API is missing.

- [ ] **Step 3: Implement scan and reconciliation**

Use `walkdir`, `gguf-rs-lib`, and `notify` 8.2. Keep one pure `scan(root) -> ScanResult` function and a separate watcher adapter. Persist only normalized records through `ConfigStore`.

- [ ] **Step 4: Verify scan behavior**

Run: `cargo test -p model-launcher-core --test catalog && cargo test -p model-launcher-core`

Expected: all pass, including malformed and moved model cases.

- [ ] **Step 5: Commit**

```bash
git add crates/model-launcher-core
git commit -m "feat: discover gguf models with stable identity"
```

## Task 5: Implement the WSL llama.cpp adapter

**Files:**
- Create: `crates/model-launcher-wsl/Cargo.toml`
- Create: `crates/model-launcher-wsl/src/{lib,path,probe,port,process}.rs`
- Test: inline unit tests and `crates/model-launcher-wsl/tests/fake_wsl.rs`

- [ ] **Step 1: Write failing path and help-parser tests**

Cover drive casing, spaces, Unicode, relative/UNC rejection, traversal-like input and invalid roots, known flag aliases, unsupported flags, version capture, and command argument rendering. Assert user-controlled values remain separate argv elements and are never interpolated into a script.

- [ ] **Step 2: Verify failure**

Run: `cargo test -p model-launcher-wsl`

Expected: crate or symbols are absent.

- [ ] **Step 3: Implement probe caching and invalidation**

Persist `ProbeSnapshot { distribution, executable_path, executable_identity, version_raw, help_raw, capabilities, probed_at }`. Obtain executable identity from a structured WSL `stat` invocation (device, inode, size, mtime). Reuse a snapshot only while distribution, path, and identity match. Saving engine settings always validates identity; a changed identity executes fresh `--version` and `--help`. Test cache hit, each invalidation input, failed reprobe preserving the prior diagnostic, and raw-output persistence.

- [ ] **Step 4: Implement a shell-safe PID protocol and process ownership**

Invoke probes directly as `wsl.exe -d <distribution> -- <executable> --version/--help`. For launch only, pass a fixed, application-owned POSIX script as the `sh -c` program: `printf 'MODEL_LAUNCHER_PID=%s\\n' "$$"; exec "$@"`. Pass a constant `$0` sentinel, then executable/model/settings solely as subsequent positional argv elements. The script text never contains user input. Parse the first control line as the exact Linux PID; `exec` preserves that PID. Stream all following stdout/stderr to `LogStore`. Stop with structured `wsl.exe -d <distribution> -- kill -TERM -- <pid>`, wait, then use `kill -KILL -- <pid>` after timeout. Reject malformed/missing PID handshakes and never fall back to process-name termination.

- [ ] **Step 5: Implement internal-port allocation and retry**

Create an injectable `InternalPortAllocator`. The production allocator asks Windows for an ephemeral loopback port by binding `127.0.0.1:0`, reads the assigned port, releases the reservation immediately before spawn, then polls readiness. If startup reports address-in-use or the port becomes occupied by a non-owned server, terminate the attempted owned PID, allocate a new port, and retry up to three times. During model replacement, require the old process to exit and its prior health endpoint to stop responding before reuse; normally allocate a fresh port. Tests simulate a stolen port, exhausted retries, old-port release delay, and verify the public gateway port never changes.

- [ ] **Step 6: Verify without requiring WSL**

Run: `cargo test -p model-launcher-wsl && cargo clippy -p model-launcher-wsl --all-targets -- -D warnings`

Expected: all fake-runner tests pass on macOS/Linux CI too.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/model-launcher-wsl
git commit -m "feat: add llama cpp WSL engine adapter"
```

## Task 6: Implement the stable API gateway and compatibility contracts

**Files:**
- Create: `crates/model-launcher-api/Cargo.toml`
- Create: `crates/model-launcher-api/src/{lib,auth,models,management,proxy}.rs`
- Create: `crates/model-launcher-api/tests/contracts.rs`
- Create: `crates/model-launcher-api/tests/fixtures/{models,load,unload}/*.json`
- Create: `tests/fake-llama-server/Cargo.toml`
- Create: `tests/fake-llama-server/src/main.rs`

- [ ] **Step 1: Add pinned JSON fixture tests**

Encode the 2026-08-29 LM Studio v1 baseline from the spec. Test list/load/unload success and errors, omitted-versus-null behavior, optional load config echo, authentication, and stable application error codes.

- [ ] **Step 2: Add failing proxy tests**

Start the fake upstream on an ephemeral port. Verify request body forwarding, safe headers, model-field routing, byte-identical SSE chunks, client disconnect accounting, body/header/connection limits, startup timeout, JIT load, same-model shared load, `model_busy`, `model_starting`, and unknown model errors. Eject during active SSE must cancel upstream work, terminate the client stream with the documented error behavior, and decrement the in-flight count exactly once.

- [ ] **Step 3: Verify failure**

Run: `cargo test -p model-launcher-api --test contracts`

Expected: fails because routes are unimplemented.

- [ ] **Step 4: Implement Axum routes and proxy**

Use typed management DTOs and streaming Reqwest bodies. Put authentication and request limits in middleware. Treat model selection as a lifecycle command, not direct engine access. Hash generated tokens with Argon2; expose plaintext only from the creation result.

- [ ] **Step 5: Verify contracts and streaming**

Run: `cargo test -p model-launcher-api --all-targets && cargo clippy -p model-launcher-api --all-targets -- -D warnings`

Expected: all fixture, auth, routing, and SSE tests pass.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/model-launcher-api tests/fake-llama-server
git commit -m "feat: expose compatible model and inference APIs"
```

## Task 7: Compose the headless application service

**Files:**
- Create: `apps/model-launcher/Cargo.toml`
- Create: `apps/model-launcher/src/{main,service}.rs`
- Test: `apps/model-launcher/tests/headless.rs`

- [ ] **Step 1: Write a failing end-to-end headless test**

Compose temp configuration, catalog fixture, fake engine, lifecycle, and gateway. Verify scan→load→stream→eject, restart without auto-load, and shutdown ordering.

- [ ] **Step 2: Verify failure**

Run: `cargo test -p model-launcher --test headless`

Expected: binary/service crate is absent.

- [ ] **Step 3: Implement composition and shutdown**

Build a service handle independent of UI. Shutdown stops accepting HTTP, cancels backoff, ejects the engine, persists config, and joins background tasks with timeouts.

- [ ] **Step 4: Verify the vertical slice**

Run: `cargo test -p model-launcher --test headless && cargo test --workspace`

Expected: the complete non-visual MVP flow passes.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml apps/model-launcher
git commit -m "feat: compose headless model launcher service"
```

## Task 8: Build the Quiet Native Slint UI and tray lifecycle

**Files:**
- Create: `crates/model-launcher-ui/Cargo.toml`
- Create: `crates/model-launcher-ui/build.rs`
- Create: `crates/model-launcher-ui/ui/app.slint`
- Create: `crates/model-launcher-ui/ui/components/{model-row,status-pill,load-dialog}.slint`
- Create: `crates/model-launcher-ui/src/{lib,tray}.rs`
- Modify: `apps/model-launcher/Cargo.toml`
- Modify: `apps/model-launcher/src/main.rs`
- Test: `crates/model-launcher-ui/tests/view_model.rs`

- [ ] **Step 1: Write failing view-model tests**

Test snapshot-to-row mapping, responsive metadata priority, busy-state action disabling, supported-setting visibility, one-time close notice, one-time plaintext token reveal, save-settings-triggered capability reprobe, recent model ordering, bounded log snapshot/filter/export commands, and tray command mapping without opening a real window.

- [ ] **Step 2: Verify failure**

Run: `cargo test -p model-launcher-ui --test view_model`

Expected: UI crate is absent.

- [ ] **Step 3: Implement the accepted layout**

Use Slint 1.16.1 with system-tray enabled. Implement the 38px title region, 48px horizontal navigation, full-width compact model list, Quiet Native palette, capability-driven load dialog, Server/Logs/Settings pages, and tray menu. The Logs page consumes bounded snapshots/subscriptions from `LogStore`, filters by source/level, and invokes redacted export. Keep core state in Rust; Slint receives display models and emits commands.

- [ ] **Step 4: Implement destroy/recreate behavior**

Keep only the tray component and an application handle while closed. Drop `MainWindow` after close, and instantiate/hydrate a new one on Open. Add an instrumentation-only weak reference assertion used by the test harness.

- [ ] **Step 5: Verify UI logic and compile**

Run: `cargo test -p model-launcher-ui && cargo build -p model-launcher`

Expected: view-model tests pass and the desktop binary builds on the development host; platform-specific code is cfg-gated.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/model-launcher-ui apps/model-launcher
git commit -m "feat: add quiet native desktop and tray UI"
```

## Task 9: Add Windows packaging and real-WSL acceptance harness

**Files:**
- Create: `apps/model-launcher/build.rs`
- Create: `apps/model-launcher/resources/app.rc`
- Create: `tests/windows-wsl/README.md`
- Create: `tests/windows-wsl/smoke.ps1`
- Create: `.github/workflows/ci.yml`
- Create: `.github/workflows/windows.yml`
- Create: `README.md`
- Modify: `.gitignore`

- [ ] **Step 1: Add CI and smoke-test checks**

CI runs format, clippy, unit, contract, and headless tests on Windows, macOS, and Linux; the real WSL/model smoke test remains explicit because it requires a local model and executable.

- [ ] **Step 2: Add Windows metadata and documentation**

Embed application name/version/icon, document prerequisites and setup, explain LAN/token safety, provide curl examples for every supported endpoint, and document the ignored WSL smoke test inputs.

- [ ] **Step 3: Run the full automated verification**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --all-targets && cargo build --workspace --release`

Expected: all commands pass.

- [ ] **Step 4: Run resource and lifecycle checks**

On Windows, run the smoke harness with a small GGUF and user-supplied llama-server. Open/close the window 50 times, confirm the window weak handle is released and working-set growth stays within the documented tolerance, verify idle CPU is negligible, fill logs/catalog beyond their configured limits and confirm they stay bounded, kill llama-server to observe capped backoff, and eject during backoff.

- [ ] **Step 5: Commit**

```bash
git add .github .gitignore README.md apps/model-launcher tests/windows-wsl
git commit -m "build: package and verify the Windows WSL MVP"
```

## Task 10: Final verification against the specification

**Files:**
- Modify only files required by defects found during verification
- Create: `docs/superpowers/verification/2026-08-29-model-launcher-mvp.md`

- [ ] **Step 1: Trace every MVP requirement to evidence**

Create a requirement matrix linking each design requirement to tests, commands, or a documented Windows manual check. Mark any unavailable real-WSL evidence explicitly rather than claiming it passed.

- [ ] **Step 2: Run final automated checks from a clean build**

Run: `cargo clean && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --all-targets && cargo build -p model-launcher --release`

Expected: all commands pass from clean state.

- [ ] **Step 3: Review the implementation**

Use `superpowers:requesting-code-review` against the design and this plan. Fix all blocking findings and rerun targeted plus full verification.

- [ ] **Step 4: Commit verification evidence**

```bash
git add docs/superpowers/verification
git commit -m "docs: record Model Launcher MVP verification"
```
