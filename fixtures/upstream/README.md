# Upstream SSE fixtures

Exact wire bytes produced by the fake upstream in
`crates/spike/src/fake_upstream.rs` (Spike A, M0), captured per sender
script line. Each `.jsonl` file holds one JSON object per line — no
comments, no headers.

## Field semantics

- `line` — the exact bytes one upstream sender task put on the wire for a
  normal `data: <evt>` event (CRLF framing included:
  `data: <evt>\r\n\r\n`).
- `raw` — a script line with the `RAW:` prefix stripped; the sender
  appends `\r\n\r\n`, so the field already contains the full wire bytes.
  Models non-SSE text passed through verbatim (an upstream may emit
  anything; the gateway is byte-transparent).
- `raw_hex` — hex of the exact wire bytes: the `RAWHEX:` payload
  hex-decoded, with the sender-appended `\r\n\r\n` event framing
  (`0d0a0d0a`) included. These records exercise arbitrary bytes that no
  text codec survives (NUL/control bytes, truncated UTF-8).

## Files

- `sse-healthy.jsonl` — scenario `healthy`: five well-formed events.
- `sse-malformed.jsonl` — scenario `malformed`: well-formed events mixed
  with non-SSE text and raw byte sequences.

Fixture updates must accompany any change to `fake_upstream.rs`'s
scenarios (see ADR-0003 fixture convention).