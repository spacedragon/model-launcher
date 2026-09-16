# Runtime fixtures — provenance and version pins

Status: **not locally captured.** The development host has no `llama.cpp`
or `NInfer` installation, so `llama-server --version`, `llama-server --help`
and any HTTP response have never been captured live here.

These files are deliberately pinned fixtures used by the adapters and their
tests. They are truthful about their origin:

| File | Kind | Origin |
| --- | --- | --- |
| `llama-server-version.txt` | pinned | synthesized `llama-server` version line at the ADR-0003 candidate baseline `b5555` |
| `llama-server-help.txt` | pinned | `llama-server --help` shape for the `b5555` baseline |
| `ninfer-serve-version.txt` | pinned | pinned `ninfer-serve` identity line (`0.9.2`) |
| `ninfer-serve-help.txt` | pinned | usage text checked in with the repository |
| `health-and-models.json` | pinned | schema-versioned `/health` and `/v1/models` bodies synthesized at the pinned engine versions |
| `manifest.json` | metadata | machine-readable pins and provenance |

## Rules

1. Fixture paths/names stay stable and the manifest pins the engine versions,
   so existing consumers keep working.
2. A fixture update that changes an engine version must update
   `manifest.json` **and** ADR-0003 (see `docs/adr/ADR-0003-runtime-version-baseline.md`).
3. `health-and-models.json` stores response bodies as byte-exact JSON strings.
   Never store a re-serialized object; that loses whitespace, key order and
   escaping. Its `engine_versions` must equal the `manifest.json`
   `fixture_version` for each runtime, and its `provenance.kind` must stay
   truthful about whether the bodies are captured or synthesized.
4. When field capture becomes possible, replace the pinned files with the
   captured bytes, set `collection_status` to `captured`, and record the host
   and engine build in `manifest.json`.
