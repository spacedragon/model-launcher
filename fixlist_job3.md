# Codex round-1 verdict — Job 3 (persistence) — FIX LIST

Fix ALL of the following on branch job/3-persistence. For each, add/adjust a test that
proves the fix (per the "add a test" note where given). Keep crates/domain FROZEN.

## P1 — blockers

### P1-1. Repository state writes do not validate transitions against the persisted state
`repos.rs:696` (upsert), `repos.rs:783` (write_state), `repos.rs:1076` (advance):
DB CHECK fences only validate that a token is in the vocabulary. A write can still set
an ILLEGAL transition (e.g. instance `failed → ready`, operation `succeeded → running`)
because nothing compares to the CURRENT persisted state.
FIX: inside the same transaction, (a) fetch and parse the row's current state, (b) run
the frozen domain state machine (crates/domain state_machine::transition_*) on
(current, new), (c) apply a guarded `UPDATE ... WHERE <state_col> = <expected_current>`.
If the current state is missing/undecodable or the transition is illegal, return
`DomainError` with `ErrorCode::InvalidStateTransition` (for instances and operations
respectively). New/first-seen rows (no prior state) that are created with a legal
initial state must still succeed. Add a test: write a legal transition (passes), then
attempt an illegal one (e.g. ready→queued or failed→ready / operation succeeded→running)
and assert `InvalidStateTransition` and that the row is unchanged.

### P1-2. Non-state CHECK failures map to the wrong error code
`mapping.rs:148`: currently any CHECK failure is routed to `Internal`. The required
mapping: ONLY the three named state fences (instance_state_valid, operation_state_valid,
and the runtime/artifact/operation-kind fences if you named them) → `InvalidStateTransition`.
EVERY other SQLite constraint violation (invalid port, negative size, invalid boolean,
FK, UNIQUE, unnamed CHECK) → `InvalidRequest`.
FIX: classify by the constraint name when available; named state fences → InvalidStateTransition,
everything else → InvalidRequest. Add a test that violates a NON-state constraint (e.g. a
bad port value or negative size if such a CHECK exists, otherwise a UNIQUE/FK violation) and
asserts `InvalidRequest`.

### P1-3. SQL injection via interpolated IDs in list filters
`repos.rs:828` and `repos.rs:1123`: caller-controlled IDs are spliced into SQL via Rust
debug/string formatting. An ID containing `"` breaks/alters the SQL.
FIX: use `sqlx::QueryBuilder` (or plain query with `:param` bind) so EVERY filter value is a
bound parameter — no string concatenation/formatting of user input into SQL. Add a test that
filters by an ID containing a double-quote character and confirms it is treated as a literal
(no error, no match, no SQL error).

### P1-4. Database file not created with user-only permissions
`store.rs:43`: `SqliteStore::open` creates the DB file without enforcing the user-only
permissions required by docs/security-and-deployment.md.
FIX: on Unix/WSL, if the file does not exist create it securely; if it exists with mode more
permissive than 0600 (group/other bits set), tighten it to 0600 (reject is acceptable — pick
one, document it, and make it deterministic). Gate the permission enforcement with
`#[cfg(unix)]` and add a `#[cfg(unix)]` test: create the store in a temp dir, reopen, and assert
the file's mode has no group/other permission bits (use `std::os::unix::fs::MetadataExt`).
On Windows this is a no-op (keep the code compiling on Windows).

### P1-5. Corrupt/out-of-range pid & port silently become None
`repos.rs:959`: `pid`/`port` decoded via `.ok()` so an out-of-range or corrupt numeric is
silently dropped to `None`. The requirement is that undecodable/corrupt rows surface
`Internal`, never silently.
FIX: decode with `i64 -> i32/u16` via `i32::try_from`/`u16::try_from` (or the from_* helper you
use) and `.map(...).transpose()?` so a failure returns the domain `Internal` error. Keep a
legitimate SQL NULL as `None`. Add a test: write a row with an out-of-range pid/port, read it
back, assert it surfaces `Internal` (not silently None, not a panic).

## P2 — should fix

### P2-6. Runtime CRUD clobbers probe columns; preservation test is vacuous
`repos.rs:524`, `tests.rs:433`: ordinary runtime upserts overwrite `last_probe_ok`/`last_probed_at`;
the test clones the existing values and resubmits them, so the "preserved" assertion is true by
construction.
FIX: runtime CRUD must NOT touch the probe columns (they belong to the probe path, job 5). Expose
a separate `record_runtime_probe(...)` (or similar) that updates ONLY the probe columns. Add a test:
perform a CRUD update that supplies no probe data and assert the previously-recorded
last_probe_ok/last_probed_at are UNCHANGED.

### P2-7. Per-connection PRAGMA test can reuse one pooled connection
`tests.rs:223`: acquires and drops 3 connections sequentially; the pool may hand back the same
connection each time, so the test doesn't prove EACH connection gets the PRAGMA.
FIX: acquire SEVERAL connections concurrently (hold them all alive at once) and assert each one
reports the expected journal_mode/foreign_keys/busy_timeout. If the pool's max size caps this, size
the pool (or the temp store) to allow at least as many simultaneous connections as you assert.

### P2-8. WAL reopen test proves nothing (reapplies WAL)
`tests.rs:261`: uses `SqliteStore::open` which itself sets journal_mode(WAL), so it can't show WAL
persisted across a reopen.
FIX: close the original store; reopen using PLAIN SQLite connection options that do NOT request
WAL; then `PRAGMA journal_mode` and assert it is `wal` (proving the WAL mode was persisted by the
first store and survived the reopen). If your `SqliteStore` API always applies WAL, add a narrow
test-only helper (clearly marked) or open a raw sqlx connection with no WAL pragma for this assert.

## After fixing
- Run: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` (Windows).
- Then WSL: `wsl -d Ubuntu -- bash -lc 'cd /mnt/d/Workspace/model-serving && cargo test -p model-serving-persistence 2>&1 | tail -8'` (foreground, not detached).
- Commit with Conventional Commits (fix(persistence): ...), subject ≤72 chars, clean tree.
- Do NOT run codex, do NOT touch docs/jobs.md, do NOT touch crates/domain.

## Final report
- commit list (git log --oneline 1d43030..HEAD)
- for each P1/P2: one line on how it was fixed + the test that proves it
- test counts (Windows + WSL)
- any place you had to make a judgment call (document it)