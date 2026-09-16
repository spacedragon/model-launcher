//! The repositories: `models`, `model_roots`, `runtimes`, `instances`,
//! `operations`, `audit` events and `settings`.
//!
//! Every function takes an explicit `sqlx::Executor` — pass
//! `store.pool()` for single-statement work or `&mut *tx` from
//! [`crate::SqliteStore::transaction`] for a multi-table consistency write
//! (`docs/architecture.md` §9).
//!
//! Row <-> domain mapping and validation live here, not in the frozen
//! `model-serving-domain` crate: rows decode into domain types (`Model`,
//! `Runtime`, `Instance`, `Operation`) and any decode failure surfaces as the
//! domain `Internal` error code. State columns are additionally fenced by the
//! schema's named `CHECK` constraints, so a token outside the state-machine
//! vocabulary can never be stored even by out-of-band SQL.
//!
//! `ModelRoot` and `AuditKind` / `AuditEvent` are persistence-level types:
//! the domain crate (frozen for this job) has no home for them; model roots
//! and the audit log are storage concerns of the control plane.

use chrono::{DateTime, Utc};
use model_serving_domain::error::Result;
use model_serving_domain::model::{
    Capabilities, Instance, InstanceFailure, InstanceHealth, InstanceState, LoadConfig, Model,
    Operation, OperationError, OperationKind, OperationState, Runtime, RuntimeKind,
};
use serde::{Deserialize, Serialize};
use sqlx::Sqlite;

use crate::mapping::{
    as_i64, from_i64, from_json, now, parse_ts, parse_wire, storage_error, to_json, ts_string,
    wire_token,
};

/// A controlled scan directory (`docs/api.md` §5 model-roots).
///
/// Persistence-level type — the frozen domain crate has no model-root type.
/// `last_scan_at` / `last_scan_result` hold the most recent scanner outcome
/// (written by the scan job, M1 job 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRoot {
    /// Stable root id.
    pub id: String,
    /// Absolute directory path scanned for artifacts.
    pub path: String,
    /// Whether the scanner includes this root.
    pub enabled: bool,
    /// When the last scan of this root ran (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scan_at: Option<DateTime<Utc>>,
    /// Structured outcome of the last scan (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scan_result: Option<serde_json::Value>,
}

/// A stored `Runtime` plus its most recent probe outcome.
///
/// Persistence-level view: the frozen `Runtime` type carries no probe
/// fields; `last_probe_ok` / `last_probed_at` are written by the
/// runtime-probe job (M1 job 5) and preserved by CRUD.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeWithProbe {
    /// The registered runtime.
    pub runtime: Runtime,
    /// Whether the most recent probe succeeded (optional until first probe).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_ok: Option<bool>,
    /// When the most recent probe ran (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probed_at: Option<DateTime<Utc>>,
}

/// An append-only audit / event-log entry (`docs/architecture.md` §9,
/// `docs/api.md` §5 SSE replay). The autoincrement `id` doubles as the
/// monotonically increasing event id for `Last-Event-ID` replay.
///
/// Persistence-level type: `AuditKind` is a storage vocabulary (the domain
/// crate is frozen for this job).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// A model row was inserted or updated.
    ModelUpserted,
    /// A model row was soft-deleted.
    ModelDeleted,
    /// A soft-deleted model row was restored by a re-appearing artifact.
    ModelRestored,
    /// A model root was inserted or updated.
    ModelRootUpserted,
    /// A model root was removed.
    ModelRootDeleted,
    /// A runtime was inserted or updated.
    RuntimeUpserted,
    /// A runtime was removed.
    RuntimeDeleted,
    /// A setting value was written.
    SettingWritten,
    /// An instance's desired state was driven by an in-flight operation.
    InstanceDesiredChanged,
    /// A non-terminal instance was marked `crashed` on daemon restart
    /// (`docs/architecture.md` §8).
    InstanceRecovered,
    /// An operation reached a terminal state.
    OperationFinished,
}

/// One audit-log row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Autoincrement event id (`0` for a not-yet-persisted event).
    pub id: u64,
    /// What happened.
    pub kind: AuditKind,
    /// Object kind the event is about, e.g. `"model"` (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_type: Option<String>,
    /// Object id the event is about (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    /// The operation that caused the event (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Structured event detail (opaque JSON, `{}` for none).
    #[serde(default)]
    pub payload: serde_json::Value,
    /// When the event was recorded, RFC 3339 UTC.
    pub created_at: DateTime<Utc>,
}

impl AuditEvent {
    /// Build a not-yet-persisted event (id `0`).
    #[must_use]
    pub fn new(
        kind: AuditKind,
        subject_type: Option<String>,
        subject_id: Option<String>,
        operation_id: Option<String>,
        payload: serde_json::Value,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id: 0,
            kind,
            subject_type,
            subject_id,
            operation_id,
            payload,
            created_at,
        }
    }
}

/// Filter for [`InstancesRepo::list`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceQuery {
    /// Restrict to one model (the model `id` — the stable UUID the
    /// `instances.model_id` column stores).
    pub model_id: Option<String>,
    /// Restrict to one actual lifecycle state.
    pub state: Option<InstanceState>,
}

impl InstanceQuery {
    /// No restrictions.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }
}

/// Filter for [`OperationsRepo::list`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperationQuery {
    /// Restrict to one progress state.
    pub state: Option<OperationState>,
    /// Restrict to one instance.
    pub instance_id: Option<String>,
    /// Restrict to one model (model `id`).
    pub model_id: Option<String>,
}

impl OperationQuery {
    /// No restrictions.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }
}

/// Indexed model artifact rows.
///
/// Model `id` (the stable UUID, `models.id`) is the identity; `key` is the
/// unique readable API identifier. `upsert` preserves scanner bookkeeping
/// (`root_id`, `first_seen_at`) on conflict, so it doubles as the
/// re-appearing-file restore path for the scan job (M1 job 4).
#[derive(Debug)]
pub struct ModelsRepo;

impl ModelsRepo {
    const UPSERT: &str = "
        INSERT INTO models (
            id, key, path, display_name, artifact_kind, size_bytes, mtime,
            default_runtime_id, default_load_config, metadata, deleted,
            first_seen_at, last_seen_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (key) DO UPDATE SET
            path = excluded.path,
            display_name = excluded.display_name,
            artifact_kind = excluded.artifact_kind,
            size_bytes = excluded.size_bytes,
            mtime = excluded.mtime,
            default_runtime_id = excluded.default_runtime_id,
            default_load_config = excluded.default_load_config,
            metadata = excluded.metadata,
            deleted = excluded.deleted,
            last_seen_at = excluded.last_seen_at
    ";

    /// Insert or update a model by `key`. On conflict every stored field is
    /// refreshed (including `deleted`, so an upsert with `deleted = false`
    /// restores a soft-deleted row) while the row's `id` / `root_id` /
    /// `first_seen_at` are preserved.
    ///
    /// # Errors
    ///
    /// `InvalidRequest` on a uniqueness collision (e.g. the model UUID is
    /// already used by another row), `Internal` on storage/serialization
    /// failure.
    pub async fn upsert<'c, E>(exec: E, model: &Model) -> Result<()>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let load_config = match &model.default_load_config {
            Some(cfg) => Some(to_json(cfg, "default_load_config")?),
            None => None,
        };
        let metadata = model
            .metadata
            .as_ref()
            .map(|m| to_json(m, "metadata"))
            .transpose()?;
        let seen_at = ts_string(now());
        sqlx::query(Self::UPSERT)
            .bind(&model.id)
            .bind(&model.key)
            .bind(&model.path)
            .bind(&model.display_name)
            .bind(wire_token(&model.artifact_kind))
            .bind(as_i64(model.size_bytes))
            .bind(ts_string(model.mtime))
            .bind(&model.default_runtime_id)
            .bind(load_config)
            .bind(metadata)
            .bind(model.deleted)
            .bind(&seen_at)
            .bind(&seen_at)
            .execute(exec)
            .await
            .map_err(|e| storage_error(&e, "upsert model"))?;
        Ok(())
    }

    /// Fetch one model by its readable `key`, regardless of `deleted`.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn get_by_key<'c, E>(exec: E, key: &str) -> Result<Option<Model>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let row: Option<ModelRow> = sqlx::query_as(
            "SELECT id, key, path, display_name, artifact_kind, size_bytes, mtime, \
                 default_runtime_id, default_load_config, metadata, deleted \
             FROM models WHERE key = ?1",
        )
        .bind(key)
        .fetch_optional(exec)
        .await
        .map_err(|e| storage_error(&e, "get model"))?;
        row.map(model_from_row).transpose()
    }

    /// List models in `key` order.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn list<'c, E>(exec: E, include_deleted: bool) -> Result<Vec<Model>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let sql = if include_deleted {
            "SELECT id, key, path, display_name, artifact_kind, size_bytes, mtime, \
             default_runtime_id, default_load_config, metadata, deleted \
             FROM models ORDER BY key"
        } else {
            "SELECT id, key, path, display_name, artifact_kind, size_bytes, mtime, \
             default_runtime_id, default_load_config, metadata, deleted \
             FROM models WHERE deleted = 0 ORDER BY key"
        };
        let rows: Vec<ModelRow> = sqlx::query_as(sql)
            .fetch_all(exec)
            .await
            .map_err(|e| storage_error(&e, "list models"))?;
        rows.into_iter().map(model_from_row).collect()
    }

    /// Soft-delete (`deleted = true`) or restore (`deleted = false`) a model
    /// by `key`. Returns `false` when no such model exists.
    ///
    /// # Errors
    ///
    /// `Internal` on storage failure.
    pub async fn set_deleted<'c, E>(exec: E, key: &str, deleted: bool) -> Result<bool>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let result =
            sqlx::query("UPDATE models SET deleted = ?2, last_seen_at = ?3 WHERE key = ?1")
                .bind(key)
                .bind(deleted)
                .bind(ts_string(now()))
                .execute(exec)
                .await
                .map_err(|e| storage_error(&e, "set model deleted"))?;
        Ok(result.rows_affected() > 0)
    }
}

#[derive(sqlx::FromRow)]
struct ModelRow {
    id: String,
    key: String,
    path: String,
    display_name: Option<String>,
    artifact_kind: String,
    size_bytes: i64,
    mtime: String,
    default_runtime_id: Option<String>,
    default_load_config: Option<String>,
    metadata: Option<String>,
    deleted: bool,
}

fn model_from_row(row: ModelRow) -> Result<Model> {
    Ok(Model {
        id: row.id,
        key: row.key,
        path: row.path,
        display_name: row.display_name,
        artifact_kind: parse_wire(&row.artifact_kind, "artifact_kind")?,
        size_bytes: from_i64::<u64>(row.size_bytes, "size_bytes")?,
        mtime: parse_ts(&row.mtime, "mtime")?,
        default_runtime_id: row.default_runtime_id,
        default_load_config: match row.default_load_config {
            Some(raw) => Some(from_json::<LoadConfig>(&raw, "default_load_config")?),
            None => None,
        },
        metadata: match row.metadata {
            Some(raw) => Some(from_json::<serde_json::Value>(&raw, "metadata")?),
            None => None,
        },
        deleted: row.deleted,
    })
}

/// Controlled scan directory rows.
#[derive(Debug)]
pub struct ModelRootsRepo;

impl ModelRootsRepo {
    const UPSERT: &str = "
        INSERT INTO model_roots (id, path, enabled, last_scan_at, last_scan_result, created_at)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT (id) DO UPDATE SET
            path = excluded.path,
            enabled = excluded.enabled,
            last_scan_at = excluded.last_scan_at,
            last_scan_result = excluded.last_scan_result
    ";

    /// Write (upsert) one model-root record by `id`. On conflict every
    /// stored field is refreshed while the row's `created_at` is preserved.
    ///
    /// # Errors
    ///
    /// `InvalidRequest` when `path` is already registered to another root,
    /// `Internal` on storage failure.
    pub async fn upsert<'c, E>(exec: E, root: &ModelRoot) -> Result<()>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let scan_result = match &root.last_scan_result {
            Some(v) => Some(to_json(v, "last_scan_result")?),
            None => None,
        };
        let created_at = ts_string(now());
        sqlx::query(Self::UPSERT)
            .bind(&root.id)
            .bind(&root.path)
            .bind(root.enabled)
            .bind(root.last_scan_at.map(ts_string))
            .bind(scan_result)
            .bind(created_at)
            .execute(exec)
            .await
            .map_err(|e| storage_error(&e, "upsert model root"))?;
        Ok(())
    }

    /// Fetch one root by `id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn get<'c, E>(exec: E, id: &str) -> Result<Option<ModelRoot>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let row: Option<ModelRootRow> = sqlx::query_as(
            "SELECT id, path, enabled, last_scan_at, last_scan_result \
                            FROM model_roots WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(exec)
        .await
        .map_err(|e| storage_error(&e, "get model root"))?;
        row.map(model_root_from_row).transpose()
    }

    /// Find the root registered at `path` (paths are unique).
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn find_by_path<'c, E>(exec: E, path: &str) -> Result<Option<ModelRoot>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let row: Option<ModelRootRow> = sqlx::query_as(
            "SELECT id, path, enabled, last_scan_at, last_scan_result \
                            FROM model_roots WHERE path = ?1",
        )
        .bind(path)
        .fetch_optional(exec)
        .await
        .map_err(|e| storage_error(&e, "find model root by path"))?;
        row.map(model_root_from_row).transpose()
    }

    /// List all roots in `id` order.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn list<'c, E>(exec: E) -> Result<Vec<ModelRoot>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let rows: Vec<ModelRootRow> = sqlx::query_as(
            "SELECT id, path, enabled, last_scan_at, last_scan_result FROM model_roots ORDER BY id",
        )
        .fetch_all(exec)
        .await
        .map_err(|e| storage_error(&e, "list model roots"))?;
        rows.into_iter().map(model_root_from_row).collect()
    }

    /// Remove a root by `id` (models referencing it keep their row; their
    /// `root_id` is set to `NULL`). Returns `false` when no such root exists.
    ///
    /// # Errors
    ///
    /// `Internal` on storage failure.
    pub async fn delete<'c, E>(exec: E, id: &str) -> Result<bool>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let result = sqlx::query("DELETE FROM model_roots WHERE id = ?1")
            .bind(id)
            .execute(exec)
            .await
            .map_err(|e| storage_error(&e, "delete model root"))?;
        Ok(result.rows_affected() > 0)
    }
}

#[derive(sqlx::FromRow)]
struct ModelRootRow {
    id: String,
    path: String,
    enabled: bool,
    last_scan_at: Option<String>,
    last_scan_result: Option<String>,
}

fn model_root_from_row(row: ModelRootRow) -> Result<ModelRoot> {
    Ok(ModelRoot {
        id: row.id,
        path: row.path,
        enabled: row.enabled,
        last_scan_at: match row.last_scan_at {
            Some(raw) => Some(parse_ts(&raw, "last_scan_at")?),
            None => None,
        },
        last_scan_result: match row.last_scan_result {
            Some(raw) => Some(from_json::<serde_json::Value>(&raw, "last_scan_result")?),
            None => None,
        },
    })
}

/// Registered runtime rows (plus the last-probe columns written by the
/// runtime-probe job, M1 job 5).
#[derive(Debug)]
pub struct RuntimeRepo;

impl RuntimeRepo {
    const UPSERT: &str = "
        INSERT INTO runtimes (
            id, kind, executable_path, enabled, version_text, capabilities, fixed_args,
            last_probe_ok, last_probed_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (id) DO UPDATE SET
            kind = excluded.kind,
            executable_path = excluded.executable_path,
            enabled = excluded.enabled,
            version_text = excluded.version_text,
            capabilities = excluded.capabilities,
            fixed_args = excluded.fixed_args,
            last_probe_ok = excluded.last_probe_ok,
            last_probed_at = excluded.last_probed_at
    ";

    /// Insert or update a runtime record by `id`. The `last_probe_ok` /
    /// `last_probed_at` fields carry the record's most recent probe outcome.
    ///
    /// # Errors
    ///
    /// `Internal` on storage/serialization failure.
    pub async fn upsert<'c, E>(exec: E, record: &RuntimeWithProbe) -> Result<()>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let runtime = &record.runtime;
        sqlx::query(Self::UPSERT)
            .bind(&runtime.id)
            .bind(wire_token(&runtime.kind))
            .bind(&runtime.executable_path)
            .bind(runtime.enabled)
            .bind(&runtime.version_text)
            .bind(to_json(&runtime.capabilities, "capabilities")?)
            .bind(to_json(&runtime.fixed_args, "fixed_args")?)
            .bind(record.last_probe_ok)
            .bind(record.last_probed_at.map(ts_string))
            .execute(exec)
            .await
            .map_err(|e| storage_error(&e, "upsert runtime"))?;
        Ok(())
    }

    /// Fetch one runtime record by `id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn get<'c, E>(exec: E, id: &str) -> Result<Option<RuntimeWithProbe>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let row: Option<RuntimeRow> = sqlx::query_as(
            "SELECT id, kind, executable_path, enabled, version_text, capabilities, fixed_args, \
                 last_probe_ok, last_probed_at FROM runtimes WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(exec)
        .await
        .map_err(|e| storage_error(&e, "get runtime"))?;
        row.map(runtime_from_row).transpose()
    }

    /// List all runtimes in `id` order.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn list<'c, E>(exec: E) -> Result<Vec<RuntimeWithProbe>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let rows: Vec<RuntimeRow> = sqlx::query_as(
            "SELECT id, kind, executable_path, enabled, version_text, capabilities, \
             fixed_args, last_probe_ok, last_probed_at FROM runtimes ORDER BY id",
        )
        .fetch_all(exec)
        .await
        .map_err(|e| storage_error(&e, "list runtimes"))?;
        rows.into_iter().map(runtime_from_row).collect()
    }

    /// Remove a runtime by `id` (models defaulting to it keep their row;
    /// `default_runtime_id` is set to `NULL`). Returns `false` when no such
    /// runtime exists.
    ///
    /// # Errors
    ///
    /// `Internal` on storage failure.
    pub async fn delete<'c, E>(exec: E, id: &str) -> Result<bool>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let result = sqlx::query("DELETE FROM runtimes WHERE id = ?1")
            .bind(id)
            .execute(exec)
            .await
            .map_err(|e| storage_error(&e, "delete runtime"))?;
        Ok(result.rows_affected() > 0)
    }
}

#[derive(sqlx::FromRow)]
struct RuntimeRow {
    id: String,
    kind: String,
    executable_path: String,
    enabled: bool,
    version_text: Option<String>,
    capabilities: String,
    fixed_args: String,
    last_probe_ok: Option<bool>,
    last_probed_at: Option<String>,
}

fn runtime_from_row(row: RuntimeRow) -> Result<RuntimeWithProbe> {
    Ok(RuntimeWithProbe {
        runtime: Runtime {
            id: row.id,
            kind: parse_wire::<RuntimeKind>(&row.kind, "kind")?,
            executable_path: row.executable_path,
            enabled: row.enabled,
            version_text: row.version_text,
            capabilities: from_json::<Capabilities>(&row.capabilities, "capabilities")?,
            fixed_args: from_json::<Vec<String>>(&row.fixed_args, "fixed_args")?,
        },
        last_probe_ok: row.last_probe_ok,
        last_probed_at: match row.last_probed_at {
            Some(raw) => Some(parse_ts(&raw, "last_probed_at")?),
            None => None,
        },
    })
}

/// Instance rows.
///
/// `instances.model_id` stores the model's stable `id` (the `models.id`
/// UUID); `desired_state` is the state an in-flight operation drives the
/// instance toward, written in the same transaction as the operation
/// (`docs/architecture.md` §9).
#[derive(Debug)]
pub struct InstancesRepo;

impl InstancesRepo {
    /// Insert or update an instance row. On insert `desired_state` starts at
    /// the instance's current state; on conflict the stored `desired_state`
    /// is preserved (drive it explicitly via [`Self::set_desired_state`]).
    ///
    /// # Errors
    ///
    /// `InvalidRequest` on a constraint violation (unknown model / runtime),
    /// `InvalidStateTransition` if the stored state tokens escape the state
    /// fences (defensive), `Internal` otherwise.
    pub async fn upsert<'c, E>(exec: E, instance: &Instance) -> Result<()>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let at = now();
        let state = instance.state();
        let health = instance
            .health
            .as_ref()
            .map(|h| to_json(h, "health"))
            .transpose()?;
        let failure = instance
            .failure
            .as_ref()
            .map(|f| to_json(f, "failure"))
            .transpose()?;
        let at_str = ts_string(at);
        sqlx::query(
            "INSERT INTO instances (
                instance_id, model_id, runtime_id, load_config,
                state, desired_state, pid, port, device_ids,
                started_at, last_used_at, active_requests, health, failure,
                created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (instance_id) DO UPDATE SET
                model_id = excluded.model_id,
                runtime_id = excluded.runtime_id,
                load_config = excluded.load_config,
                state = excluded.state,
                pid = excluded.pid,
                port = excluded.port,
                device_ids = excluded.device_ids,
                started_at = excluded.started_at,
                last_used_at = excluded.last_used_at,
                active_requests = excluded.active_requests,
                health = excluded.health,
                failure = excluded.failure,
                updated_at = excluded.updated_at",
        )
        .bind(&instance.instance_id)
        .bind(&instance.model_id)
        .bind(&instance.runtime_id)
        .bind(to_json(&instance.load_config, "load_config")?)
        .bind(wire_token(&state))
        .bind(wire_token(&state))
        .bind(instance.pid.map(u64::from).map(as_i64))
        .bind(instance.port.map(i32::from))
        .bind(to_json(&instance.device_ids, "device_ids")?)
        .bind(instance.started_at.map(ts_string))
        .bind(instance.last_used_at.map(ts_string))
        .bind(i64::from(instance.active_requests))
        .bind(health)
        .bind(failure)
        .bind(&at_str)
        .bind(&at_str)
        .execute(exec)
        .await
        .map_err(|e| storage_error(&e, "upsert instance"))?;
        Ok(())
    }

    /// Drive an instance's `desired_state` (written together with the
    /// triggering operation + audit event in one transaction —
    /// `docs/architecture.md` §9). The token is fenced by the schema's
    /// `instance_desired_state_valid` `CHECK`.
    ///
    /// # Errors
    ///
    /// `InvalidStateTransition` for a token outside the state vocabulary,
    /// `Internal` otherwise.
    pub async fn set_desired_state<'c, E>(
        exec: E,
        instance_id: &str,
        desired: InstanceState,
    ) -> Result<()>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        sqlx::query(
            "UPDATE instances SET desired_state = ?2, updated_at = ?3 WHERE instance_id = ?1",
        )
        .bind(instance_id)
        .bind(wire_token(&desired))
        .bind(ts_string(now()))
        .execute(exec)
        .await
        .map_err(|e| storage_error(&e, "set instance desired state"))?;
        Ok(())
    }

    /// Write an instance's actual lifecycle state (plus an optional
    /// structured failure and the `updated_at` stamp). Callers validate the
    /// move through the domain state machine first (e.g. via
    /// [`Instance::with_state`]); the schema fence still guards the write.
    ///
    /// # Errors
    ///
    /// `InvalidStateTransition` for a fenced-out token, `Internal` otherwise.
    pub async fn write_state<'c, E>(
        exec: E,
        instance_id: &str,
        state: InstanceState,
        failure: &Option<InstanceFailure>,
        updated_at: DateTime<Utc>,
    ) -> Result<()>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let failure_json = failure
            .as_ref()
            .map(|f| to_json(f, "failure"))
            .transpose()?;
        sqlx::query(
            "UPDATE instances SET state = ?2, failure = ?3, updated_at = ?4 WHERE instance_id = ?1",
        )
        .bind(instance_id)
        .bind(wire_token(&state))
        .bind(failure_json)
        .bind(ts_string(updated_at))
        .execute(exec)
        .await
        .map_err(|e| storage_error(&e, "write instance state"))?;
        Ok(())
    }

    /// Fetch one instance by `instance_id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row, `InvalidStateTransition` if a stored
    /// state token escaped the fences (should be impossible).
    pub async fn get<'c, E>(exec: E, instance_id: &str) -> Result<Option<Instance>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let row: Option<InstanceRow> = sqlx::query_as(
            "SELECT instance_id, model_id, runtime_id, load_config, state, \
                 pid, port, device_ids, started_at, last_used_at, active_requests, \
                 health, failure FROM instances WHERE instance_id = ?1",
        )
        .bind(instance_id)
        .fetch_optional(exec)
        .await
        .map_err(|e| storage_error(&e, "get instance"))?;
        row.map(instance_from_row).transpose()
    }

    /// List instances filtered by [`InstanceQuery`] (model `id` and/or
    /// actual state), ordered by `instance_id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn list<'c, E>(exec: E, query: InstanceQuery) -> Result<Vec<Instance>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let mut clauses: Vec<String> = Vec::new();
        if let Some(model_id) = query.model_id {
            clauses.push(format!("model_id = {model_id:?}"));
        }
        if let Some(state) = query.state {
            clauses.push(format!("state = {:?}", wire_token(&state)));
        }
        let mut sql = String::from(
            "SELECT instance_id, model_id, runtime_id, load_config, state, \
             pid, port, device_ids, started_at, last_used_at, active_requests, \
             health, failure FROM instances",
        );
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY instance_id");
        let rows: Vec<InstanceRow> = sqlx::query_as(&sql)
            .fetch_all(exec)
            .await
            .map_err(|e| storage_error(&e, "list instances"))?;
        rows.into_iter().map(instance_from_row).collect()
    }

    /// The docs/architecture.md §8 restart-recovery query: every instance in
    /// a **non-terminal** state (`queued` / `loading` / `ready` /
    /// `draining` / `unloading`). The token list is derived from the domain
    /// state machine (`InstanceState::is_terminal`) so the two can never
    /// drift, and the result is served by the partial index
    /// `idx_instances_state_nonterminal`.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn non_terminal<'c, E>(exec: E) -> Result<Vec<Instance>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let states = non_terminal_state_tokens();
        let sql = format!(
            "SELECT instance_id, model_id, runtime_id, load_config, state, \
             pid, port, device_ids, started_at, last_used_at, active_requests, \
             health, failure FROM instances WHERE state IN ({}) ORDER BY instance_id",
            states
                .iter()
                .map(|t| format!("{t:?}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let rows: Vec<InstanceRow> = sqlx::query_as(&sql)
            .fetch_all(exec)
            .await
            .map_err(|e| storage_error(&e, "query non-terminal instances"))?;
        rows.into_iter().map(instance_from_row).collect()
    }

    /// The stored `desired_state` of one instance (`None` when the instance
    /// does not exist).
    ///
    /// # Errors
    ///
    /// `Internal` if the stored token escaped the state fences (should be
    /// impossible).
    pub async fn desired_state<'c, E>(exec: E, instance_id: &str) -> Result<Option<InstanceState>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let raw: Option<String> =
            sqlx::query_scalar("SELECT desired_state FROM instances WHERE instance_id = ?1")
                .bind(instance_id)
                .fetch_optional(exec)
                .await
                .map_err(|e| storage_error(&e, "get instance desired state"))?;
        match raw {
            Some(token) => Ok(Some(parse_wire(&token, "desired_state")?)),
            None => Ok(None),
        }
    }
}

#[derive(sqlx::FromRow)]
struct InstanceRow {
    instance_id: String,
    model_id: String,
    runtime_id: String,
    load_config: String,
    state: String,
    pid: Option<i64>,
    port: Option<i64>,
    device_ids: String,
    started_at: Option<String>,
    last_used_at: Option<String>,
    active_requests: i64,
    health: Option<String>,
    failure: Option<String>,
}

/// The wire tokens of the non-terminal instance states, derived from the
/// domain state machine so the §8 recovery query and `is_terminal` can never
/// drift apart.
#[must_use]
fn non_terminal_state_tokens() -> Vec<String> {
    let all = [
        InstanceState::Unloaded,
        InstanceState::Queued,
        InstanceState::Loading,
        InstanceState::Ready,
        InstanceState::Draining,
        InstanceState::Unloading,
        InstanceState::Failed,
        InstanceState::Crashed,
    ];
    all.into_iter()
        .filter(|s| !s.is_terminal())
        .map(|s| wire_token(&s))
        .collect()
}

/// Rebuild a domain `Instance` from a stored row.
///
/// The stored state is reached from the initial `unloaded` state by a
/// validated path in the domain state machine (every state in the vocabulary
/// has one — see `state_machine.rs`), so the reconstruction never bypasses
/// the state machine.
fn instance_from_row(row: InstanceRow) -> Result<Instance> {
    let load_config = from_json::<LoadConfig>(&row.load_config, "load_config")?;
    let state = parse_wire::<InstanceState>(&row.state, "state")?;
    let mut instance = Instance::new(row.instance_id, row.model_id, row.runtime_id, load_config);
    for step in reach_path(state) {
        instance = instance.with_state(*step)?;
    }
    if let Some(pid) = row.pid.and_then(|v| from_i64::<u32>(v, "pid").ok()) {
        instance.pid = Some(pid);
    }
    if let Some(port) = row.port.and_then(|v| from_i64::<u16>(v, "port").ok()) {
        instance.port = Some(port);
    }
    instance.device_ids = from_json::<Vec<u32>>(&row.device_ids, "device_ids")?;
    if let Some(raw) = &row.started_at {
        instance.started_at = Some(parse_ts(raw, "started_at")?);
    }
    if let Some(raw) = &row.last_used_at {
        instance.last_used_at = Some(parse_ts(raw, "last_used_at")?);
    }
    instance.active_requests = from_i64(row.active_requests, "active_requests")?;
    instance.health = match row.health {
        Some(raw) => Some(from_json::<InstanceHealth>(&raw, "health")?),
        None => None,
    };
    instance.failure = match row.failure {
        Some(raw) => Some(from_json::<InstanceFailure>(&raw, "failure")?),
        None => None,
    };
    Ok(instance)
}

/// The validated single-step path from the initial `unloaded` state to
/// `target` (each entry is legal from the previous; `state_machine.rs`).
const fn reach_path(target: InstanceState) -> &'static [InstanceState] {
    use InstanceState as S;
    match target {
        S::Unloaded => &[],
        S::Queued => &[S::Queued],
        S::Loading => &[S::Queued, S::Loading],
        S::Ready => &[S::Queued, S::Loading, S::Ready],
        S::Draining => &[S::Queued, S::Loading, S::Ready, S::Draining],
        S::Unloading => &[S::Queued, S::Loading, S::Unloading],
        S::Failed => &[S::Queued, S::Failed],
        S::Crashed => &[S::Queued, S::Crashed],
    }
}

/// Load / unload / rescan operation rows.
///
/// `operations.model_id` stores the model's stable `id` (UUID); `error` is
/// the structured [`OperationError`] JSON, `result` the structured result
/// payload JSON (`docs/api.md` §5).
#[derive(Debug)]
pub struct OperationsRepo;

impl OperationsRepo {
    /// Insert a new operation. `created_at` falls back to "now" when the
    /// domain value is `None`.
    ///
    /// # Errors
    ///
    /// `InvalidRequest` on a constraint violation (unknown instance / model
    /// reference), `InvalidStateTransition` if the stored state token escapes
    /// the fence, `Internal` otherwise.
    pub async fn create<'c, E>(exec: E, operation: &Operation) -> Result<()>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let created = operation.created_at.unwrap_or_else(now);
        let error_json = operation
            .error
            .as_ref()
            .map(|e| to_json(e, "error"))
            .transpose()?;
        let result_json = operation
            .result
            .as_ref()
            .map(|r| to_json(r, "result"))
            .transpose()?;
        sqlx::query(
            "INSERT INTO operations (
                operation_id, kind, state, instance_id, model_id,
                created_at, finished_at, error, result
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&operation.operation_id)
        .bind(wire_token(&operation.kind))
        .bind(wire_token(&operation.state()))
        .bind(&operation.instance_id)
        .bind(&operation.model_id)
        .bind(ts_string(created))
        .bind(operation.finished_at.map(ts_string))
        .bind(error_json)
        .bind(result_json)
        .execute(exec)
        .await
        .map_err(|e| storage_error(&e, "create operation"))?;
        Ok(())
    }

    /// Persist a state advance of an existing operation (terminal or not):
    /// `state`, `finished_at`, the structured `error` and `result` are
    /// rewritten from the passed domain value. Returns `false` when no such
    /// operation exists.
    ///
    /// # Errors
    ///
    /// `InvalidStateTransition` if the stored state token escapes the fence,
    /// `Internal` otherwise.
    pub async fn advance<'c, E>(exec: E, operation: &Operation) -> Result<bool>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let error_json = operation
            .error
            .as_ref()
            .map(|e| to_json(e, "error"))
            .transpose()?;
        let result_json = operation
            .result
            .as_ref()
            .map(|r| to_json(r, "result"))
            .transpose()?;
        let result = sqlx::query(
            "UPDATE operations SET state = ?2, finished_at = ?3, error = ?4, result = ?5 \
             WHERE operation_id = ?1",
        )
        .bind(&operation.operation_id)
        .bind(wire_token(&operation.state()))
        .bind(operation.finished_at.map(ts_string))
        .bind(error_json)
        .bind(result_json)
        .execute(exec)
        .await
        .map_err(|e| storage_error(&e, "advance operation"))?;
        Ok(result.rows_affected() > 0)
    }

    /// Fetch one operation by `operation_id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row (e.g. malformed stored `error` / `result`
    /// JSON).
    pub async fn get<'c, E>(exec: E, operation_id: &str) -> Result<Option<Operation>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let row: Option<OperationRow> = sqlx::query_as(
            "SELECT operation_id, kind, state, instance_id, model_id, \
                 created_at, finished_at, error, result \
             FROM operations WHERE operation_id = ?1",
        )
        .bind(operation_id)
        .fetch_optional(exec)
        .await
        .map_err(|e| storage_error(&e, "get operation"))?;
        row.map(operation_from_row).transpose()
    }

    /// List operations filtered by [`OperationQuery`], ordered by
    /// `created_at` then `operation_id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn list<'c, E>(exec: E, query: OperationQuery) -> Result<Vec<Operation>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let mut clauses: Vec<String> = Vec::new();
        if let Some(state) = query.state {
            clauses.push(format!("state = {:?}", wire_token(&state)));
        }
        if let Some(instance_id) = query.instance_id {
            clauses.push(format!("instance_id = {instance_id:?}"));
        }
        if let Some(model_id) = query.model_id {
            clauses.push(format!("model_id = {model_id:?}"));
        }
        let mut sql = String::from(
            "SELECT operation_id, kind, state, instance_id, model_id, \
             created_at, finished_at, error, result FROM operations",
        );
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY created_at, operation_id");
        let rows: Vec<OperationRow> = sqlx::query_as(&sql)
            .fetch_all(exec)
            .await
            .map_err(|e| storage_error(&e, "list operations"))?;
        rows.into_iter().map(operation_from_row).collect()
    }

    /// Every operation in a non-terminal state (`queued` / `running`), for
    /// the supervisor to settle on daemon restart (M1 job 6). Tokens are
    /// derived from `OperationState::is_terminal` so the two cannot drift.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn non_terminal<'c, E>(exec: E) -> Result<Vec<Operation>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let all = [
            OperationState::Queued,
            OperationState::Running,
            OperationState::Succeeded,
            OperationState::Failed,
            OperationState::Cancelled,
        ];
        let states = all
            .into_iter()
            .filter(|s| !s.is_terminal())
            .map(|s| format!("{:?}", wire_token(&s)))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT operation_id, kind, state, instance_id, model_id, \
             created_at, finished_at, error, result FROM operations \
             WHERE state IN ({states}) ORDER BY created_at, operation_id"
        );
        let rows: Vec<OperationRow> = sqlx::query_as(&sql)
            .fetch_all(exec)
            .await
            .map_err(|e| storage_error(&e, "query non-terminal operations"))?;
        rows.into_iter().map(operation_from_row).collect()
    }
}

#[derive(sqlx::FromRow)]
struct OperationRow {
    operation_id: String,
    kind: String,
    state: String,
    instance_id: Option<String>,
    model_id: Option<String>,
    created_at: String,
    finished_at: Option<String>,
    error: Option<String>,
    result: Option<String>,
}

/// Rebuild a domain `Operation` from a stored row (the state is reached from
/// the initial `queued` state by the validated path; the other fields are
/// public and assigned directly).
fn operation_from_row(row: OperationRow) -> Result<Operation> {
    let kind = parse_wire::<OperationKind>(&row.kind, "kind")?;
    let state = parse_wire::<OperationState>(&row.state, "state")?;
    let mut operation = Operation::new(row.operation_id, kind);
    for step in operation_reach_path(state) {
        operation = operation.with_state(*step)?;
    }
    operation.instance_id = row.instance_id;
    operation.model_id = row.model_id;
    operation.created_at = Some(parse_ts(&row.created_at, "created_at")?);
    operation.finished_at = match row.finished_at {
        Some(raw) => Some(parse_ts(&raw, "finished_at")?),
        None => None,
    };
    operation.error = match row.error {
        Some(raw) => Some(from_json::<OperationError>(&raw, "error")?),
        None => None,
    };
    operation.result = match row.result {
        Some(raw) => Some(from_json::<serde_json::Value>(&raw, "result")?),
        None => None,
    };
    Ok(operation)
}

/// The validated single-step path from the initial `queued` state to
/// `target` (each entry is legal from the previous; `state_machine.rs`).
const fn operation_reach_path(target: OperationState) -> &'static [OperationState] {
    use OperationState as S;
    match target {
        S::Queued => &[],
        S::Running => &[S::Running],
        S::Succeeded => &[S::Running, S::Succeeded],
        S::Failed => &[S::Running, S::Failed],
        S::Cancelled => &[S::Cancelled],
    }
}

/// Append-only audit / event log.
///
/// `append` is insert-only by design (`docs/architecture.md` §9: the audit
/// event is one of the three tables committed together with the operation
/// and the instance desired state); events are never updated or deleted.
#[derive(Debug)]
pub struct AuditRepo;

impl AuditRepo {
    /// Append one event. The autoincrement row id becomes the event id.
    ///
    /// # Errors
    ///
    /// `Internal` on storage failure.
    pub async fn append<'c, E>(exec: E, event: &AuditEvent) -> Result<()>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        sqlx::query(
            "INSERT INTO audit_events \
             (kind, subject_type, subject_id, operation_id, payload, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(wire_token(&event.kind))
        .bind(&event.subject_type)
        .bind(&event.subject_id)
        .bind(&event.operation_id)
        .bind(to_json(&event.payload, "payload")?)
        .bind(ts_string(event.created_at))
        .execute(exec)
        .await
        .map_err(|e| storage_error(&e, "append audit event"))?;
        Ok(())
    }

    /// Events recorded at or after `since` (all events when `None`), in
    /// event-id order — the `Last-Event-ID` replay shape (`docs/api.md` §5).
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn list<'c, E>(exec: E, since: Option<DateTime<Utc>>) -> Result<Vec<AuditEvent>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let sql = match since {
            Some(_) => {
                "SELECT id, kind, subject_type, subject_id, operation_id, payload, created_at \
                        FROM audit_events WHERE created_at >= ?1 ORDER BY id"
            }
            None => {
                "SELECT id, kind, subject_type, subject_id, operation_id, payload, created_at \
                     FROM audit_events ORDER BY id"
            }
        };
        let rows: Vec<AuditEventRow> = match since {
            Some(ts) => sqlx::query_as(sql)
                .bind(ts_string(ts))
                .fetch_all(exec)
                .await
                .map_err(|e| storage_error(&e, "list audit events"))?,
            None => sqlx::query_as(sql)
                .fetch_all(exec)
                .await
                .map_err(|e| storage_error(&e, "list audit events"))?,
        };
        rows.into_iter().map(audit_from_row).collect()
    }

    /// The most recent `limit` events, oldest first.
    ///
    /// # Errors
    ///
    /// `Internal` on a corrupt row or storage failure.
    pub async fn latest<'c, E>(exec: E, limit: u32) -> Result<Vec<AuditEvent>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let rows: Vec<AuditEventRow> = sqlx::query_as(
            "SELECT id, kind, subject_type, subject_id, operation_id, payload, created_at \
             FROM audit_events ORDER BY id DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(exec)
        .await
        .map_err(|e| storage_error(&e, "list latest audit events"))?;
        let mut events: Vec<AuditEvent> = Vec::with_capacity(rows.len());
        for row in rows.into_iter().rev() {
            events.push(audit_from_row(row)?);
        }
        Ok(events)
    }
}

#[derive(sqlx::FromRow)]
struct AuditEventRow {
    id: i64,
    kind: String,
    subject_type: Option<String>,
    subject_id: Option<String>,
    operation_id: Option<String>,
    payload: String,
    created_at: String,
}

fn audit_from_row(row: AuditEventRow) -> Result<AuditEvent> {
    Ok(AuditEvent {
        id: from_i64(row.id, "audit_events.id")?,
        kind: parse_wire::<AuditKind>(&row.kind, "kind")?,
        subject_type: row.subject_type,
        subject_id: row.subject_id,
        operation_id: row.operation_id,
        payload: from_json::<serde_json::Value>(&row.payload, "payload")?,
        created_at: parse_ts(&row.created_at, "created_at")?,
    })
}

/// A stored daemon-wide key/value setting (`docs/api.md` §5: GET/PATCH
/// `/admin/v1/settings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    /// The setting key.
    pub key: String,
    /// The raw string value.
    pub value: String,
    /// Whether the value only takes effect after a daemon restart.
    pub restart_required: bool,
    /// When the value was last written, RFC 3339 UTC.
    pub updated_at: DateTime<Utc>,
}

/// Daemon-wide key/value setting rows.
#[derive(Debug)]
pub struct SettingsRepo;

impl SettingsRepo {
    /// Write (upsert) one setting value.
    ///
    /// # Errors
    ///
    /// `Internal` on storage failure.
    pub async fn set<'c, E>(exec: E, key: &str, value: &str, restart_required: bool) -> Result<()>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        sqlx::query(
            "INSERT INTO settings (key, value, restart_required, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT (key) DO UPDATE SET
                value = excluded.value,
                restart_required = excluded.restart_required,
                updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(restart_required)
        .bind(ts_string(now()))
        .execute(exec)
        .await
        .map_err(|e| storage_error(&e, "set setting"))?;
        Ok(())
    }

    /// Read one setting value (`None` when the key is unset).
    ///
    /// # Errors
    ///
    /// `Internal` on storage failure.
    pub async fn get<'c, E>(exec: E, key: &str) -> Result<Option<String>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let value: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?1")
            .bind(key)
            .fetch_optional(exec)
            .await
            .map_err(|e| storage_error(&e, "get setting"))?;
        Ok(value)
    }

    /// All settings in key order.
    ///
    /// # Errors
    ///
    /// `Internal` on storage failure.
    pub async fn list<'c, E>(exec: E) -> Result<Vec<Setting>>
    where
        E: sqlx::Executor<'c, Database = Sqlite> + 'c,
    {
        let rows: Vec<SettingRow> = sqlx::query_as(
            "SELECT key, value, restart_required, updated_at FROM settings ORDER BY key",
        )
        .fetch_all(exec)
        .await
        .map_err(|e| storage_error(&e, "list settings"))?;
        rows.into_iter()
            .map(|r| {
                Ok(Setting {
                    key: r.key,
                    value: r.value,
                    restart_required: r.restart_required,
                    updated_at: parse_ts(&r.updated_at, "updated_at")?,
                })
            })
            .collect()
    }
}

#[derive(sqlx::FromRow)]
struct SettingRow {
    key: String,
    value: String,
    restart_required: bool,
    updated_at: String,
}
