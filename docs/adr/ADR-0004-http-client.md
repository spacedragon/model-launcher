# ADR-0004: HTTP client for upstream streaming (reqwest stream)

- Status: accepted
- Date: 2026-09-15
- Spike: S1 / Spike A (`crates/spike/src/forwarder.rs`, `crates/spike/src/fake_upstream.rs`)
- Tests: `cargo test -p model-serving-spike` (4/4 green on Windows + Git Bash)

## Context

The daemon must forward long-lived streaming HTTP responses (SSE from
`llama-server` / `ninfer-serve`) to browser clients, byte-for-byte, with this
hard requirement (validated by Spike A):

> **Client disconnect ⇒ upstream request/task cancelled ⇒ upstream TCP
> connection released ⇒ no leaked half-open streams or upstream slots.**

Candidate clients:

1. **reqwest** with the `stream` feature (`bytes_stream()`) — async,
   built on hyper 1.x, same stack as axum 0.8.
2. **hyper client** directly — one less crate, but reqwest is a thin wrapper
   over the identical hyper client; hand-rolling connection pooling, header
   handling, and error mapping buys nothing here.
3. **ureq/other** — sync-first; poor fit for a streaming server-side client.

Key mechanism, identical in reqwest and hyper: the response body is a `Stream`
poll-driven by our server (axum). Dropping the stream drops the response,
which drops the underlying connection/task, closing the upstream socket.
Nothing special (no `abort()` handles) is needed — cancellation is structural
via RAII/drop.

Spike A measured exactly this with a hyper-based fake upstream that counts
concurrent in-flight SSE sender tasks (`Stats::active`): after the test client
abandoned the stream, `active` returned to 0 within ~100 ms and the upstream
event counter froze (proving cancellation, not just idle).

## Decision

Use **reqwest 0.12 (`default-features = false`, features `stream` + `json`)**
as the upstream HTTP client, shared as one `reqwest::Client` per runtime
connection (connection pooling via `pool_idle_timeout`).

Rationale:

- Same hyper 1.x core as axum 0.8 → consistent error/IO semantics, no TLS
  stack duplication (local upstreams are plain HTTP/1.1; `default-features
  = false` drops TLS entirely).
- `Client::bytes_stream()` gives a `Stream<Item = Result<Bytes>>` that maps
  directly onto `axum::body::Body::from_stream`; a client disconnect drops
  the body, which drops the reqwest stream and closes the upstream socket.
  Verified by `disconnect_releases_upstream_no_leak`.
- One shared `Client` gives us idle connection reuse for the many short
  `/health` probes the daemon will make against runtime processes.

Configuration rules (enforced in `Forwarder::new`):

- **No global request timeout** — streaming responses are unbounded; killing
  a healthy 10-minute generation with a timer would corrupt streams.
  `connect_timeout` (5 s) bounds only the connect phase.
- **`.no_proxy()`** — the upstream is always a local child process on
  loopback; inherited `HTTP(S)_PROXY` environment settings must never route
  inference traffic through a proxy (verified by a codex review scenario
  where a system proxy broke loopback upstreams).
- Upstream status codes are passed through; an upstream *connect* failure
  surfaces as an explicit `502 {error: "upstream_unreachable"}` — never a
  hang.
- **Byte-transparent forwarding**: the forwarder does not parse/validate SSE
  (malformed upstream bytes pass through verbatim; see spike
  `malformed_upstream_data_no_hang`). SSE semantics are the client's concern;
  a validating layer can be added later without changing this client choice.

## Consequences

- `reqwest` becomes a workspace dependency for the server crate (not just the
  spike); keep `default-features = false` to stay TLS-free.
- Cancellation correctness depends on **not buffering** the whole response
  and **not holding** the `reqwest::Response` alive beyond the body stream
  (the response is consumed by `bytes_stream()`; do not cache it).
- Upstream 断流 (server closes mid-stream) surfaces as *truncation* of the
  forwarded body, not an error status (HTTP status is already sent). Clients
  must tolerate truncated SSE; the daemon may later attach a terminal
  `runtime_event` for diagnostic purposes.
- HTTP/1.1 only for now (local upstreams on loopback); revisit HTTP/2 only
  if an upstream requires it.