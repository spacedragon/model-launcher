# Real Windows + WSL acceptance

`smoke.ps1` exercises the packaged executable against a real, user-owned WSL `llama-server` and GGUF. It is never part of ordinary CI. The manual GitHub workflow requires a trusted self-hosted runner labelled `Windows` and `WSL`; do not use an untrusted fork with model/token inputs.

Run from a clean repository in an interactive PowerShell session:

```powershell
./tests/windows-wsl/smoke.ps1 `
  -Distro Ubuntu `
  -LlamaServer /usr/local/bin/llama-server `
  -LlamaCommit b1234 `
  -ModelRoot D:\Models\Acceptance `
  -ModelKey publisher/tiny-model `
  -ModelProvenance "publisher/repository revision; license" `
  -SecondModelKey publisher/second-model `
  -ManualResourceChecks
```

Optional controls are `-ExePath`, `-BaseUrl`, `-Token`, `-ModelSha256`, `-SkipBuild`, and `-TimeoutSeconds`. If `-ModelSha256` is omitted, the first shard is hashed locally (which can take time for a large model). If `-Token` is omitted in an interactive session, the harness pauses so the operator can generate and paste a one-time token. If supplied, its Argon2 hash must already exist in the user's normal launcher config; the harness copies that config into an isolated artifact data root and never edits the original. Token plaintext is masked and never written to evidence.

The GitHub workflow is automated only: it passes `-NonInteractive`, never enables resource checks, and cannot call `Read-Host`. Configure the optional Actions secret `MODEL_LAUNCHER_SMOKE_TOKEN` on the trusted self-hosted runner's repository/environment. For a successful authenticated run the runner's existing Model Launcher config must contain that token's Argon2 hash; if either is absent, the job fails explicitly instead of waiting for input. Workflow dispatch inputs are ordinary metadata and must never contain a token.

The automated section checks preflight/probe, discovery, load, both model lists, non-streaming and SSE chat, unload, JIT completion, restart without auto-load, and PID-only cleanup. Resource checks are operator-local only. Every prompt uses the strict `Read-PassFail` gate: only the exact answer `PASS` succeeds; empty/FAIL/notes record `FAIL`, evidence is still saved, and the process exits nonzero. It covers 50 window cycles, a separate weak-handle observation from debug/instrumentation output, settled working-set tolerance (32 MiB), post-cycle model-list/chat survival, a 30-second idle CPU sample (1% of one logical CPU), bounded logs/catalog, crash/backoff, and eject-during-backoff. The script never kills by name or wildcard.

Evidence is written to ignored `artifacts/windows-wsl/evidence.json` and `evidence.md`. Automated checks use `PASS`/`FAIL`/`NOT_RUN`; manual evidence is strictly `PASS` or `FAIL`. Metadata captures timestamps, Windows/build, PowerShell, WSL version/kernel/distribution, llama version/commit/help hash/executable hash, first-shard path/size/hash and provenance, CPU/GPU/RAM, app version/commit/executable hash, bind/config, and a sanitized command. Attach relevant redacted logs as well. Without these inputs, real-WSL/resource acceptance is **NOT RUN**, not passed.

Validate the harness and packaging contracts without Windows using:

```bash
ruby tests/windows-wsl/validate.rb
```
