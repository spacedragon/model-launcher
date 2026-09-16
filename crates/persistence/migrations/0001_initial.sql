-- 0001_initial.sql
--
-- Initial schema for the `model-serving` control plane (docs/architecture.md
-- §3, §8, §9).
--
-- Migration policy: **append-only**. Never edit or delete a shipped file; the
-- upgrade path for existing databases is "run the new numbered files".
--
-- Conventions (docs/api.md §1, §2.2):
--   * every state / kind / class column stores the lowercase snake_case wire
--     token as TEXT and is fenced by a named CHECK constraint so a row outside
--     the state-machine vocabulary can never be stored (docs/architecture.md
--     §9; the repositories additionally validate transitions through the
--     domain state machines before writing);
--   * timestamps are RFC 3339 UTC TEXT (`created_at`, `updated_at`, `mtime`,
--     `started_at`, ...);
--   * structured payloads (`load_config`, `capabilities`, `metadata`,
--     `health`, `failure`, `device_ids`, `error`, `result`, `payload`) are
--     JSON TEXT.
--
-- NOTE: `PRAGMA journal_mode=WAL` and `PRAGMA foreign_keys=ON` are enforced at
-- connection time by `SqliteStore::open` (journal_mode cannot be switched
-- inside a transaction, so it must not live in a migration file).

-- Daemon-wide key/value settings (docs/api.md §5: GET/PATCH /admin/v1/settings).
-- `restart_required` flags values that only take effect after a restart.
CREATE TABLE settings (
    key              TEXT PRIMARY KEY,
    value            TEXT NOT NULL,
    restart_required INTEGER NOT NULL DEFAULT 0 CHECK (restart_required IN (0, 1)),
    updated_at       TEXT NOT NULL
);

-- Registered inference runtimes (docs/architecture.md §3 "Runtime").
-- `last_probe_ok` / `last_probed_at` hold the most recent probe outcome; they
-- are written by the runtime-probe job (M1 job 5) and preserved by CRUD.
CREATE TABLE runtimes (
    id             TEXT PRIMARY KEY,
    kind           TEXT NOT NULL CHECK (kind IN ('llama_cpp', 'ninfer')),
    executable_path TEXT NOT NULL,
    enabled        INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    version_text   TEXT,
    capabilities   TEXT NOT NULL DEFAULT '{}',
    fixed_args     TEXT NOT NULL DEFAULT '[]',
    last_probe_ok  INTEGER,
    last_probed_at TEXT
);

-- Controlled scan directories (docs/api.md §5: model-roots).
-- `last_scan_at` / `last_scan_result` hold the most recent scan outcome for
-- `GET /admin/v1/model-roots` (written by the scanner job, M1 job 4).
CREATE TABLE model_roots (
    id               TEXT PRIMARY KEY,
    path             TEXT NOT NULL UNIQUE,
    enabled          INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    last_scan_at     TEXT,
    last_scan_result TEXT,
    created_at       TEXT NOT NULL
);

-- Indexed model artifacts (docs/architecture.md §3 "Model").
-- `root_id` / `first_seen_at` / `last_seen_at` are scanner bookkeeping
-- reserved for the scan job (M1 job 4); CRUD upserts preserve them.
CREATE TABLE models (
    id                  TEXT PRIMARY KEY,
    key                 TEXT NOT NULL UNIQUE,
    path                TEXT NOT NULL,
    display_name        TEXT,
    artifact_kind       TEXT NOT NULL CHECK (artifact_kind IN ('gguf', 'ninfer')),
    size_bytes          INTEGER NOT NULL CHECK (size_bytes >= 0),
    mtime               TEXT NOT NULL,
    default_runtime_id  TEXT REFERENCES runtimes (id) ON DELETE SET NULL,
    default_load_config TEXT,
    metadata            TEXT,
    deleted             INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1)),
    root_id             TEXT REFERENCES model_roots (id) ON DELETE SET NULL,
    first_seen_at       TEXT NOT NULL,
    last_seen_at        TEXT NOT NULL
);
CREATE INDEX idx_models_key ON models (key);
CREATE INDEX idx_models_path ON models (path);

-- Instance records (docs/architecture.md §3 "Instance").
-- `state` is the actual lifecycle state; `desired_state` is the state an
-- in-flight operation drives the instance toward (docs/architecture.md §9:
-- "operation、instance desired state 和 audit event 同步提交").
-- `pid` / `port` are only trusted once the process supervisor confirms them.
CREATE TABLE instances (
    instance_id     TEXT PRIMARY KEY,
    model_id        TEXT NOT NULL REFERENCES models (id) ON DELETE CASCADE,
    runtime_id      TEXT NOT NULL REFERENCES runtimes (id),
    load_config     TEXT NOT NULL,
    state           TEXT NOT NULL
        CONSTRAINT instance_state_valid
        CHECK (state IN ('unloaded', 'queued', 'loading', 'ready',
                         'draining', 'unloading', 'failed', 'crashed')),
    desired_state   TEXT NOT NULL
        CONSTRAINT instance_desired_state_valid
        CHECK (desired_state IN ('unloaded', 'queued', 'loading', 'ready',
                                 'draining', 'unloading', 'failed', 'crashed')),
    pid             INTEGER,
    port            INTEGER CHECK (port IS NULL OR (port BETWEEN 1 AND 65535)),
    device_ids      TEXT NOT NULL DEFAULT '[]',
    started_at      TEXT,
    last_used_at    TEXT,
    active_requests INTEGER NOT NULL DEFAULT 0 CHECK (active_requests >= 0),
    health          TEXT,
    failure         TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX idx_instances_model ON instances (model_id);
CREATE INDEX idx_instances_runtime ON instances (runtime_id);
-- Partial index serving the docs/architecture.md §8 restart-recovery query.
CREATE INDEX idx_instances_state_nonterminal
    ON instances (state)
    WHERE state IN ('queued', 'loading', 'ready', 'draining', 'unloading');

-- Load / unload / rescan operations (docs/architecture.md §3 "Operation").
-- `error` is the structured `OperationError` JSON; `result` the structured
-- result payload JSON (docs/api.md §5).
CREATE TABLE operations (
    operation_id TEXT PRIMARY KEY,
    kind         TEXT NOT NULL CHECK (kind IN ('load', 'unload', 'rescan')),
    state        TEXT NOT NULL
        CONSTRAINT operation_state_valid
        CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    instance_id  TEXT REFERENCES instances (instance_id) ON DELETE SET NULL,
    model_id     TEXT REFERENCES models (id) ON DELETE SET NULL,
    created_at   TEXT NOT NULL,
    finished_at  TEXT,
    error        TEXT,
    result       TEXT
);
CREATE INDEX idx_operations_state ON operations (state);
CREATE INDEX idx_operations_instance ON operations (instance_id);
CREATE INDEX idx_operations_model ON operations (model_id);

-- Append-only audit / event log (docs/architecture.md §9, docs/api.md §5 SSE
-- replay). The autoincrement id is the monotonically increasing event id used
-- for `Last-Event-ID` replay.
CREATE TABLE audit_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    kind         TEXT NOT NULL,
    subject_type TEXT,
    subject_id   TEXT,
    operation_id TEXT,
    payload      TEXT NOT NULL DEFAULT '{}',
    created_at   TEXT NOT NULL
);
CREATE INDEX idx_audit_events_created ON audit_events (created_at);
CREATE INDEX idx_audit_events_operation ON audit_events (operation_id);