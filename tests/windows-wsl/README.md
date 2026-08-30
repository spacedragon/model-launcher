# Real Windows + WSL acceptance

`smoke.ps1` exercises the packaged executable against a real, user-owned WSL `llama-server` and GGUF. It is never part of ordinary CI. The manual GitHub workflow requires a trusted self-hosted runner labelled `Windows` and `WSL`; do not use an untrusted fork with model/token inputs.

Run from a clean repository in an interactive PowerShell session:

```powershell
./tests/windows-wsl/smoke.ps1 `
  -Distro Ubuntu `
  -LlamaServer /usr/local/bin/llama-server `
  -ModelRoot D:\Models\Acceptance `
  -ModelKey publisher/tiny-model `
  -SecondModelKey publisher/second-model `
  -ManualResourceChecks
```

Optional controls are `-ExePath`, `-BaseUrl`, `-Token`, `-SkipBuild`, and `-TimeoutSeconds`. If `-Token` is omitted, the harness pauses so the operator can generate and paste a one-time token. If supplied, its Argon2 hash must already exist in the user's normal launcher config; the harness copies that config into an isolated artifact data root and never edits the original. Token plaintext is not written to evidence.

The automated section checks preflight/probe, discovery, load, both model lists, non-streaming and SSE chat, unload, JIT completion, restart without auto-load, and PID-only cleanup. A supplied second model enables the explicit busy observation section. The resource mode records 50 interactive tray open/close cycles, settled working-set tolerance (32 MiB), a 30-second idle CPU sample (1% of one logical CPU), bounded logs/catalog, crash/backoff, and eject-during-backoff observations. Timing-sensitive busy, UI weak-handle release, backend crash PID selection, and overflow fixtures remain clearly labelled manual; the script never kills by name or wildcard.

Evidence is written to ignored `artifacts/windows-wsl/evidence.json` and `evidence.md`, including `PASS`, `FAIL`, `MANUAL`, and `NOT_RUN` states. A trustworthy acceptance record must attach the exact Windows version, WSL distribution/version, llama-server build/commit and `--help`, model identity/size/hash or provenance, CPU/GPU/RAM, command line, evidence files, and relevant redacted logs. Without those inputs, real-WSL/resource acceptance is **NOT RUN**, not passed.

Validate the harness and packaging contracts without Windows using:

```bash
ruby tests/windows-wsl/validate.rb
```
