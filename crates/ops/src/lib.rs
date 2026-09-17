//! Durable load/unload operation coordination.
//!
//! Job 7 stops at the persistence/runtime boundary: Job 6 owns subprocess
//! supervision and adapters turn supervisor outcomes into calls to
//! [`OperationManager`]. Every workflow mutation runs in a SQLite
//! `BEGIN IMMEDIATE` transaction, advances the instance and operation state
//! machines together, compares monotonic revisions, and appends its audit
//! event before commit.

use model_serving_domain::error::{DomainError, ErrorCode, Result};
use model_serving_domain::model::{
    FailureClass, Instance, InstanceFailure, InstanceState, Operation, OperationError,
    OperationKind, OperationState,
};
use model_serving_domain::state_machine::{transition_instance, transition_operation};
use model_serving_persistence::{
    AuditEvent, AuditKind, AuditRepo, InstancesRepo, OperationsRepo, SqliteStore, now,
    storage_error, ts_string, wire_token,
};
use serde_json::Value;
use sqlx::pool::PoolConnection;
use sqlx::{Executor, Row, Sqlite, SqliteConnection};

/// Monotonic revisions held by the worker responsible for an operation.
/// A later call with an older handle is rejected even after an ABA state cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationHandle {
    /// Durable operation id.
    pub operation_id: String,
    /// Operation revision expected by the next transition.
    pub operation_revision: i64,
    /// Instance acted on by this operation.
    pub instance_id: String,
    /// Instance revision expected by the next transition.
    pub instance_revision: i64,
}

/// Process facts published only after the supervisor confirms readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyProcess {
    /// Runtime child process id.
    pub pid: u32,
    /// Confirmed loopback listening port.
    pub port: u16,
}

/// Counts returned by restart recovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Non-terminal operations settled during this pass.
    pub operations: usize,
    /// Orphaned non-terminal instances settled during this pass.
    pub instances: usize,
}

/// Coordinates durable operation and instance transitions.
#[derive(Debug)]
pub struct OperationManager {
    store: SqliteStore,
}

impl OperationManager {
    /// Create a manager over a migrated store.
    #[must_use]
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    /// Access the underlying store for read-side repositories.
    #[must_use]
    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    /// Persist a load operation and its queued instance atomically. A fresh
    /// `unloaded` instance is inserted; an existing matching `unloaded` or
    /// `crashed` instance is re-queued through the domain state machine.
    ///
    /// # Errors
    ///
    /// Returns `InvalidStateTransition` for a reused instance/stale state,
    /// `InvalidRequest` for a constraint violation, or `Internal` on storage
    /// failure.
    pub async fn enqueue_load(
        &self,
        operation_id: impl Into<String>,
        instance: Instance,
    ) -> Result<OperationHandle> {
        transition_instance(instance.state(), InstanceState::Queued)?;
        let operation_id = operation_id.into();
        let instance_id = instance.instance_id.clone();
        let mut connection = self.begin_immediate().await?;
        let outcome = self
            .enqueue_load_in(&mut connection, &operation_id, &instance)
            .await;
        let instance_revision = outcome.as_ref().copied().unwrap_or_default();
        finish_transaction(&mut connection, outcome.map(|_| ()))
            .await
            .map(|()| OperationHandle {
                operation_id,
                operation_revision: 0,
                instance_id,
                instance_revision,
            })
    }

    async fn enqueue_load_in(
        &self,
        connection: &mut SqliteConnection,
        operation_id: &str,
        instance: &Instance,
    ) -> Result<i64> {
        let instance_revision = match InstancesRepo::get(&mut *connection, &instance.instance_id)
            .await?
        {
            None => {
                if instance.state() != InstanceState::Unloaded {
                    return Err(conflict("a new instance must start unloaded"));
                }
                let queued = instance.clone().with_state(InstanceState::Queued)?;
                insert_instance(connection, &queued, InstanceState::Ready).await?;
                0
            }
            Some(stored) => {
                if stored.model_id != instance.model_id
                    || stored.runtime_id != instance.runtime_id
                    || stored.load_config != instance.load_config
                {
                    return Err(conflict(
                        "an existing instance cannot be reloaded with different identity or config",
                    ));
                }
                let snapshot = instance_snapshot(connection, &instance.instance_id).await?;
                transition_instance(snapshot.value.state(), InstanceState::Queued)?;
                update_instance(
                    connection,
                    &snapshot,
                    InstanceState::Queued,
                    InstanceState::Ready,
                    None,
                    RuntimePatch::Clear,
                )
                .await?;
                snapshot.revision + 1
            }
        };
        let mut operation = Operation::new(operation_id, OperationKind::Load);
        operation.instance_id = Some(instance.instance_id.clone());
        operation.model_id = Some(instance.model_id.clone());
        operation.created_at = Some(now());
        OperationsRepo::create(&mut *connection, &operation).await?;
        append_event(
            connection,
            AuditKind::InstanceDesiredChanged,
            &instance.instance_id,
            operation_id,
            serde_json::json!({"instance_state":"queued","operation_state":"queued","desired_state":"ready"}),
        )
        .await?;
        Ok(instance_revision)
    }

    /// Persist an unload operation and move a ready instance to draining.
    ///
    /// # Errors
    ///
    /// Returns `InstanceNotFound`, `InvalidStateTransition`, or a mapped
    /// persistence error when the atomic write cannot be committed.
    pub async fn enqueue_unload(
        &self,
        operation_id: impl Into<String>,
        instance_id: impl Into<String>,
    ) -> Result<OperationHandle> {
        let operation_id = operation_id.into();
        let instance_id = instance_id.into();
        let mut connection = self.begin_immediate().await?;
        let outcome = self
            .enqueue_unload_in(&mut connection, &operation_id, &instance_id)
            .await;
        let instance_revision = outcome.as_ref().copied().unwrap_or_default();
        finish_transaction(&mut connection, outcome.map(|_| ()))
            .await
            .map(|()| OperationHandle {
                operation_id,
                operation_revision: 0,
                instance_id,
                instance_revision,
            })
    }

    async fn enqueue_unload_in(
        &self,
        connection: &mut SqliteConnection,
        operation_id: &str,
        instance_id: &str,
    ) -> Result<i64> {
        let instance = instance_snapshot(connection, instance_id).await?;
        transition_instance(instance.value.state(), InstanceState::Draining)?;
        update_instance(
            connection,
            &instance,
            InstanceState::Draining,
            InstanceState::Unloaded,
            None,
            RuntimePatch::Keep,
        )
        .await?;
        let mut operation = Operation::new(operation_id, OperationKind::Unload);
        operation.instance_id = Some(instance_id.to_owned());
        operation.model_id = Some(instance.value.model_id.clone());
        operation.created_at = Some(now());
        OperationsRepo::create(&mut *connection, &operation).await?;
        append_event(
            connection,
            AuditKind::InstanceDesiredChanged,
            instance_id,
            operation_id,
            serde_json::json!({"instance_state":"draining","operation_state":"queued","desired_state":"unloaded"}),
        )
        .await?;
        Ok(instance.revision + 1)
    }

    /// Move a queued operation into its active instance state.
    ///
    /// # Errors
    ///
    /// Returns `InvalidStateTransition` when either revision/state is stale
    /// or the operation/instance pair is inconsistent, and `Internal` on
    /// storage failure.
    pub async fn start(&self, expected: &OperationHandle) -> Result<OperationHandle> {
        let mut connection = self.begin_immediate().await?;
        let outcome = self.start_in(&mut connection, expected).await;
        finish_transaction(&mut connection, outcome)
            .await
            .map(|()| OperationHandle {
                operation_id: expected.operation_id.clone(),
                operation_revision: expected.operation_revision + 1,
                instance_id: expected.instance_id.clone(),
                instance_revision: expected.instance_revision + 1,
            })
    }

    async fn start_in(
        &self,
        connection: &mut SqliteConnection,
        expected: &OperationHandle,
    ) -> Result<()> {
        let operation = checked_operation(connection, expected).await?;
        let instance = checked_instance(connection, expected).await?;
        let target = match operation.value.kind {
            OperationKind::Load => InstanceState::Loading,
            OperationKind::Unload => InstanceState::Unloading,
            OperationKind::Rescan => return Err(conflict("rescan is not an instance operation")),
        };
        transition_operation(operation.value.state(), OperationState::Running)?;
        transition_instance(instance.value.state(), target)?;
        update_instance(
            connection,
            &instance,
            target,
            instance.desired,
            None,
            RuntimePatch::Keep,
        )
        .await?;
        update_operation(connection, &operation, OperationState::Running, None, None).await?;
        append_event(
            connection,
            AuditKind::InstanceDesiredChanged,
            &expected.instance_id,
            &expected.operation_id,
            serde_json::json!({"instance_state":wire_token(&target),"operation_state":"running"}),
        )
        .await
    }

    /// Finish a running operation successfully and publish the terminal
    /// instance state in the same commit.
    ///
    /// # Errors
    ///
    /// Returns `InvalidRequest` when a successful load lacks confirmed
    /// process facts, `InvalidStateTransition` for stale/illegal progress,
    /// or `Internal` on storage failure.
    pub async fn succeed(
        &self,
        expected: &OperationHandle,
        ready_process: Option<ReadyProcess>,
        result: Option<Value>,
    ) -> Result<()> {
        let mut connection = self.begin_immediate().await?;
        let outcome = self
            .succeed_in(&mut connection, expected, ready_process, result)
            .await;
        finish_transaction(&mut connection, outcome).await
    }

    async fn succeed_in(
        &self,
        connection: &mut SqliteConnection,
        expected: &OperationHandle,
        ready_process: Option<ReadyProcess>,
        result: Option<Value>,
    ) -> Result<()> {
        let operation = checked_operation(connection, expected).await?;
        let instance = checked_instance(connection, expected).await?;
        transition_operation(operation.value.state(), OperationState::Succeeded)?;
        let (target, patch) = match operation.value.kind {
            OperationKind::Load => {
                let process = ready_process.ok_or_else(|| {
                    DomainError::with_message(
                        ErrorCode::InvalidRequest,
                        "successful load requires confirmed pid and port",
                    )
                })?;
                (InstanceState::Ready, RuntimePatch::Ready(process))
            }
            OperationKind::Unload => (InstanceState::Unloaded, RuntimePatch::Clear),
            OperationKind::Rescan => return Err(conflict("rescan is not an instance operation")),
        };
        transition_instance(instance.value.state(), target)?;
        update_instance(connection, &instance, target, target, None, patch).await?;
        update_operation(
            connection,
            &operation,
            OperationState::Succeeded,
            None,
            result,
        )
        .await?;
        append_event(
            connection,
            AuditKind::OperationFinished,
            &expected.instance_id,
            &expected.operation_id,
            serde_json::json!({"instance_state":wire_token(&target),"operation_state":"succeeded"}),
        )
        .await
    }

    /// Settle a queued or running operation as failed.
    /// A queued operation first advances to running in this transaction.
    ///
    /// # Errors
    ///
    /// Returns `InvalidStateTransition` for stale or inconsistent state and
    /// `Internal` when the terminal settlement cannot be persisted.
    pub async fn fail(
        &self,
        expected: &OperationHandle,
        error: OperationError,
        failure: InstanceFailure,
    ) -> Result<()> {
        let mut connection = self.begin_immediate().await?;
        let outcome = self
            .fail_in(&mut connection, expected, error, failure)
            .await;
        finish_transaction(&mut connection, outcome).await
    }

    async fn fail_in(
        &self,
        connection: &mut SqliteConnection,
        expected: &OperationHandle,
        error: OperationError,
        failure: InstanceFailure,
    ) -> Result<()> {
        let mut operation = checked_operation(connection, expected).await?;
        let instance = checked_instance(connection, expected).await?;
        if operation.value.state() == OperationState::Queued {
            update_operation(connection, &operation, OperationState::Running, None, None).await?;
            operation.value = operation.value.with_state(OperationState::Running)?;
            operation.revision += 1;
        }
        let target = match operation.value.kind {
            OperationKind::Load => InstanceState::Failed,
            OperationKind::Unload => InstanceState::Crashed,
            OperationKind::Rescan => return Err(conflict("rescan is not an instance operation")),
        };
        transition_instance(instance.value.state(), target)?;
        update_instance(
            connection,
            &instance,
            target,
            target,
            Some(&failure),
            RuntimePatch::Clear,
        )
        .await?;
        update_operation(
            connection,
            &operation,
            OperationState::Failed,
            Some(&error),
            None,
        )
        .await?;
        append_event(
            connection,
            AuditKind::OperationFinished,
            &expected.instance_id,
            &expected.operation_id,
            serde_json::json!({"instance_state":wire_token(&target),"operation_state":"failed","code":error.code.code_str()}),
        )
        .await
    }

    /// Durably settle a queued or running operation as cancelled.
    ///
    /// For a running operation this is the **post-supervisor** acknowledgement:
    /// the caller must first terminate and reap the managed process using the
    /// Job 6 supervisor. This method does not signal a process; it publishes
    /// `unloaded` only after that external lifecycle action has completed.
    ///
    /// # Errors
    ///
    /// Returns `InvalidStateTransition` for stale or incompatible progress
    /// and `Internal` when the atomic settlement cannot be persisted.
    pub async fn settle_cancelled(&self, expected: &OperationHandle) -> Result<()> {
        let mut connection = self.begin_immediate().await?;
        let outcome = self.cancel_in(&mut connection, expected).await;
        finish_transaction(&mut connection, outcome).await
    }

    async fn cancel_in(
        &self,
        connection: &mut SqliteConnection,
        expected: &OperationHandle,
    ) -> Result<()> {
        let operation = checked_operation(connection, expected).await?;
        let mut instance = checked_instance(connection, expected).await?;
        transition_operation(operation.value.state(), OperationState::Cancelled)?;
        let target = match (operation.value.kind, instance.value.state()) {
            (OperationKind::Load, InstanceState::Queued)
            | (OperationKind::Unload, InstanceState::Unloading) => InstanceState::Unloaded,
            (OperationKind::Load, InstanceState::Loading) => {
                update_instance(
                    connection,
                    &instance,
                    InstanceState::Unloading,
                    InstanceState::Unloaded,
                    None,
                    RuntimePatch::Keep,
                )
                .await?;
                instance.value = instance.value.with_state(InstanceState::Unloading)?;
                instance.revision += 1;
                InstanceState::Unloaded
            }
            (OperationKind::Unload, InstanceState::Draining) => InstanceState::Ready,
            _ => return Err(conflict("operation and instance states do not match")),
        };
        transition_instance(instance.value.state(), target)?;
        let patch = if target == InstanceState::Unloaded {
            RuntimePatch::Clear
        } else {
            RuntimePatch::Keep
        };
        update_instance(connection, &instance, target, target, None, patch).await?;
        update_operation(
            connection,
            &operation,
            OperationState::Cancelled,
            None,
            None,
        )
        .await?;
        append_event(
            connection,
            AuditKind::OperationFinished,
            &expected.instance_id,
            &expected.operation_id,
            serde_json::json!({"instance_state":wire_token(&target),"operation_state":"cancelled"}),
        )
        .await
    }

    /// Atomically settle all work orphaned by a daemon restart.
    /// Queued operations are cancelled; running operations fail; unowned
    /// non-terminal instances are marked crashed. A second call is a no-op.
    ///
    /// # Errors
    ///
    /// Returns `InvalidStateTransition` if durable operation/instance state
    /// is inconsistent and `Internal` when recovery cannot commit atomically.
    pub async fn recover_after_restart(&self) -> Result<RecoveryReport> {
        let mut connection = self.begin_immediate().await?;
        let outcome = self.recover_in(&mut connection).await;
        let report = outcome.as_ref().copied().unwrap_or_default();
        finish_transaction(&mut connection, outcome.map(|_| ()))
            .await
            .map(|()| report)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keeping the single restart transaction visible makes its all-or-nothing invariant auditable"
    )]
    async fn recover_in(&self, connection: &mut SqliteConnection) -> Result<RecoveryReport> {
        let operations = OperationsRepo::non_terminal(&mut *connection).await?;
        let mut report = RecoveryReport::default();
        for value in operations {
            let operation = operation_snapshot(connection, value.operation_id()).await?;
            let Some(instance_id) = operation.value.instance_id.clone() else {
                settle_orphan_operation(connection, operation).await?;
                report.operations += 1;
                continue;
            };
            let instance = instance_snapshot(connection, &instance_id).await?;
            match operation.value.state() {
                OperationState::Queued => {
                    // A queued unload already moved a live instance to Draining.
                    // After a daemon restart the supervised process is gone, so
                    // restoring Ready would preserve a stale pid/port.
                    if operation.value.kind == OperationKind::Unload {
                        update_instance(
                            connection,
                            &instance,
                            InstanceState::Crashed,
                            InstanceState::Crashed,
                            Some(&restart_failure()),
                            RuntimePatch::Clear,
                        )
                        .await?;
                        report.instances += 1;
                        update_operation(
                            connection,
                            &operation,
                            OperationState::Cancelled,
                            None,
                            None,
                        )
                        .await?;
                        append_event(
                            connection,
                            AuditKind::OperationFinished,
                            &instance_id,
                            operation.value.operation_id(),
                            serde_json::json!({"reason":"daemon_restart"}),
                        )
                        .await?;
                        report.operations += 1;
                        continue;
                    }
                    let target = match operation.value.kind {
                        OperationKind::Load => InstanceState::Unloaded,
                        OperationKind::Unload => unreachable!("handled above"),
                        OperationKind::Rescan => {
                            settle_orphan_operation(connection, operation).await?;
                            report.operations += 1;
                            continue;
                        }
                    };
                    if instance.value.state().can_transition_to(target) {
                        update_instance(
                            connection,
                            &instance,
                            target,
                            target,
                            None,
                            if target == InstanceState::Unloaded {
                                RuntimePatch::Clear
                            } else {
                                RuntimePatch::Keep
                            },
                        )
                        .await?;
                        report.instances += 1;
                    } else if !instance.value.state().is_terminal() {
                        update_instance(
                            connection,
                            &instance,
                            InstanceState::Crashed,
                            InstanceState::Crashed,
                            Some(&restart_failure()),
                            RuntimePatch::Clear,
                        )
                        .await?;
                        report.instances += 1;
                    }
                    update_operation(
                        connection,
                        &operation,
                        OperationState::Cancelled,
                        None,
                        None,
                    )
                    .await?;
                }
                OperationState::Running => {
                    if !instance.value.state().is_terminal() {
                        update_instance(
                            connection,
                            &instance,
                            InstanceState::Crashed,
                            InstanceState::Crashed,
                            Some(&restart_failure()),
                            RuntimePatch::Clear,
                        )
                        .await?;
                        report.instances += 1;
                    }
                    update_operation(
                        connection,
                        &operation,
                        OperationState::Failed,
                        Some(&restart_error()),
                        None,
                    )
                    .await?;
                }
                _ => continue,
            }
            append_event(
                connection,
                AuditKind::OperationFinished,
                &instance_id,
                operation.value.operation_id(),
                serde_json::json!({"reason":"daemon_restart"}),
            )
            .await?;
            report.operations += 1;
            // Keep snapshots visibly consumed before the orphan scan.
        }

        for value in InstancesRepo::non_terminal(&mut *connection).await? {
            let snapshot = instance_snapshot(connection, value.instance_id()).await?;
            update_instance(
                connection,
                &snapshot,
                InstanceState::Crashed,
                InstanceState::Crashed,
                Some(&restart_failure()),
                RuntimePatch::Clear,
            )
            .await?;
            append_event(
                connection,
                AuditKind::InstanceRecovered,
                value.instance_id(),
                "",
                serde_json::json!({"previous_state":wire_token(&value.state())}),
            )
            .await?;
            report.instances += 1;
        }
        Ok(report)
    }

    async fn begin_immediate(&self) -> Result<PoolConnection<Sqlite>> {
        let mut connection = self
            .store
            .pool()
            .acquire()
            .await
            .map_err(|error| storage_error(&error, "acquire operation connection"))?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *connection)
            .await
            .map_err(|error| storage_error(&error, "begin immediate operation transaction"))?;
        Ok(connection)
    }
}

#[derive(Debug)]
struct Snapshot<T> {
    value: T,
    revision: i64,
}

#[derive(Debug)]
struct InstanceSnapshot {
    value: Instance,
    revision: i64,
    desired: InstanceState,
}

#[derive(Debug, Clone, Copy)]
enum RuntimePatch {
    Keep,
    Ready(ReadyProcess),
    Clear,
}

async fn checked_operation(
    connection: &mut SqliteConnection,
    expected: &OperationHandle,
) -> Result<Snapshot<Operation>> {
    let snapshot = operation_snapshot(connection, &expected.operation_id).await?;
    if snapshot.revision != expected.operation_revision
        || snapshot.value.instance_id.as_deref() != Some(expected.instance_id.as_str())
    {
        return Err(stale("operation", &expected.operation_id));
    }
    Ok(snapshot)
}

async fn checked_instance(
    connection: &mut SqliteConnection,
    expected: &OperationHandle,
) -> Result<InstanceSnapshot> {
    let snapshot = instance_snapshot(connection, &expected.instance_id).await?;
    if snapshot.revision != expected.instance_revision {
        return Err(stale("instance", &expected.instance_id));
    }
    Ok(snapshot)
}

async fn operation_snapshot(
    connection: &mut SqliteConnection,
    operation_id: &str,
) -> Result<Snapshot<Operation>> {
    let revision: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM operations WHERE operation_id = ?1")
            .bind(operation_id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(|error| storage_error(&error, "read operation revision"))?;
    let revision = revision.ok_or_else(|| {
        DomainError::with_message(
            ErrorCode::InvalidRequest,
            format!("operation {operation_id} not found"),
        )
    })?;
    let value = OperationsRepo::get(&mut *connection, operation_id)
        .await?
        .ok_or_else(|| stale("operation", operation_id))?;
    Ok(Snapshot { value, revision })
}

async fn instance_snapshot(
    connection: &mut SqliteConnection,
    instance_id: &str,
) -> Result<InstanceSnapshot> {
    let row = sqlx::query("SELECT revision, desired_state FROM instances WHERE instance_id = ?1")
        .bind(instance_id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| storage_error(&error, "read instance revision"))?
        .ok_or_else(|| {
            DomainError::with_message(
                ErrorCode::InstanceNotFound,
                format!("instance {instance_id} not found"),
            )
        })?;
    let revision = row
        .try_get::<i64, _>("revision")
        .map_err(|error| DomainError::with_message(ErrorCode::Internal, error.to_string()))?;
    let desired_raw = row
        .try_get::<String, _>("desired_state")
        .map_err(|error| DomainError::with_message(ErrorCode::Internal, error.to_string()))?;
    let desired = parse_instance_state(&desired_raw)?;
    let value = InstancesRepo::get(&mut *connection, instance_id)
        .await?
        .ok_or_else(|| stale("instance", instance_id))?;
    Ok(InstanceSnapshot {
        value,
        revision,
        desired,
    })
}

async fn insert_instance(
    connection: &mut SqliteConnection,
    instance: &Instance,
    desired: InstanceState,
) -> Result<()> {
    let at = ts_string(now());
    sqlx::query(
        "INSERT INTO instances (instance_id, model_id, runtime_id, load_config, state, \
         desired_state, pid, port, device_ids, started_at, last_used_at, active_requests, \
         health, failure, created_at, updated_at, revision) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?15,0)",
    )
    .bind(&instance.instance_id)
    .bind(&instance.model_id)
    .bind(&instance.runtime_id)
    .bind(json(&instance.load_config, "load_config")?)
    .bind(wire_token(&instance.state()))
    .bind(wire_token(&desired))
    .bind(instance.pid.map(i64::from))
    .bind(instance.port.map(i64::from))
    .bind(json(&instance.device_ids, "device_ids")?)
    .bind(instance.started_at.map(ts_string))
    .bind(instance.last_used_at.map(ts_string))
    .bind(i64::from(instance.active_requests))
    .bind(optional_json(instance.health.as_ref(), "health")?)
    .bind(optional_json(instance.failure.as_ref(), "failure")?)
    .bind(at)
    .execute(&mut *connection)
    .await
    .map_err(|error| storage_error(&error, "insert queued instance"))?;
    Ok(())
}

async fn update_instance(
    connection: &mut SqliteConnection,
    snapshot: &InstanceSnapshot,
    target: InstanceState,
    desired: InstanceState,
    failure: Option<&InstanceFailure>,
    patch: RuntimePatch,
) -> Result<()> {
    transition_instance(snapshot.value.state(), target)?;
    let (pid, port, clear_runtime) = match patch {
        RuntimePatch::Keep => (None, None, false),
        RuntimePatch::Ready(process) => (
            Some(i64::from(process.pid)),
            Some(i64::from(process.port)),
            false,
        ),
        RuntimePatch::Clear => (None, None, true),
    };
    let result = sqlx::query(
        "UPDATE instances SET state=?2, desired_state=?3, failure=?4, \
         pid=CASE WHEN ?5 THEN NULL ELSE COALESCE(?6,pid) END, \
         port=CASE WHEN ?5 THEN NULL ELSE COALESCE(?7,port) END, \
         health=CASE WHEN ?5 THEN NULL ELSE health END, \
         active_requests=CASE WHEN ?5 THEN 0 ELSE active_requests END, \
         updated_at=?8, revision=revision+1 \
         WHERE instance_id=?1 AND state=?9 AND revision=?10",
    )
    .bind(&snapshot.value.instance_id)
    .bind(wire_token(&target))
    .bind(wire_token(&desired))
    .bind(optional_json(failure, "failure")?)
    .bind(clear_runtime)
    .bind(pid)
    .bind(port)
    .bind(ts_string(now()))
    .bind(wire_token(&snapshot.value.state()))
    .bind(snapshot.revision)
    .execute(&mut *connection)
    .await
    .map_err(|error| storage_error(&error, "advance instance state"))?;
    require_one(
        result.rows_affected(),
        "instance",
        &snapshot.value.instance_id,
    )
}

async fn update_operation(
    connection: &mut SqliteConnection,
    snapshot: &Snapshot<Operation>,
    target: OperationState,
    error: Option<&OperationError>,
    result: Option<Value>,
) -> Result<()> {
    transition_operation(snapshot.value.state(), target)?;
    let terminal = target.is_terminal();
    let updated_at = ts_string(now());
    let finished_at = terminal.then(|| updated_at.clone());
    let written = sqlx::query(
        "UPDATE operations SET state=?2, finished_at=?3, error=?4, result=?5, \
         updated_at=?6, revision=revision+1 \
         WHERE operation_id=?1 AND state=?7 AND revision=?8",
    )
    .bind(&snapshot.value.operation_id)
    .bind(wire_token(&target))
    .bind(finished_at)
    .bind(optional_json(error, "operation error")?)
    .bind(optional_json(result.as_ref(), "operation result")?)
    .bind(updated_at)
    .bind(wire_token(&snapshot.value.state()))
    .bind(snapshot.revision)
    .execute(&mut *connection)
    .await
    .map_err(|error| storage_error(&error, "advance operation state"))?;
    require_one(
        written.rows_affected(),
        "operation",
        &snapshot.value.operation_id,
    )
}

async fn settle_orphan_operation(
    connection: &mut SqliteConnection,
    mut operation: Snapshot<Operation>,
) -> Result<()> {
    if operation.value.state() == OperationState::Queued {
        update_operation(connection, &operation, OperationState::Running, None, None).await?;
        operation.value = operation.value.with_state(OperationState::Running)?;
        operation.revision += 1;
    }
    update_operation(
        connection,
        &operation,
        OperationState::Failed,
        Some(&restart_error()),
        None,
    )
    .await
}

async fn append_event(
    connection: &mut SqliteConnection,
    kind: AuditKind,
    instance_id: &str,
    operation_id: &str,
    payload: Value,
) -> Result<()> {
    AuditRepo::append(
        &mut *connection,
        &AuditEvent::new(
            kind,
            Some("instance".to_owned()),
            Some(instance_id.to_owned()),
            (!operation_id.is_empty()).then(|| operation_id.to_owned()),
            payload,
            now(),
        ),
    )
    .await
}

async fn finish_transaction(connection: &mut SqliteConnection, outcome: Result<()>) -> Result<()> {
    let statement = if outcome.is_ok() {
        "COMMIT"
    } else {
        "ROLLBACK"
    };
    let completion = connection
        .execute(statement)
        .await
        .map_err(|error| storage_error(&error, "finish operation transaction"));
    outcome.and(completion.map(|_| ()))
}

fn require_one(rows: u64, subject: &str, id: &str) -> Result<()> {
    if rows == 1 {
        Ok(())
    } else {
        Err(stale(subject, id))
    }
}

fn stale(subject: &str, id: &str) -> DomainError {
    DomainError::with_message(
        ErrorCode::InvalidStateTransition,
        format!("{subject} {id} changed before the transition applied"),
    )
}

fn conflict(message: impl Into<String>) -> DomainError {
    DomainError::with_message(ErrorCode::InvalidStateTransition, message)
}

fn json<T: serde::Serialize>(value: &T, field: &str) -> Result<String> {
    serde_json::to_string(value).map_err(|error| {
        DomainError::with_message(ErrorCode::Internal, format!("serialize {field}: {error}"))
    })
}

fn optional_json<T: serde::Serialize>(value: Option<&T>, field: &str) -> Result<Option<String>> {
    value.map(|value| json(value, field)).transpose()
}

fn parse_instance_state(raw: &str) -> Result<InstanceState> {
    serde_json::from_str(&format!("\"{raw}\"")).map_err(|error| {
        DomainError::with_message(
            ErrorCode::InvalidStateTransition,
            format!("invalid desired instance state {raw}: {error}"),
        )
    })
}

fn restart_failure() -> InstanceFailure {
    InstanceFailure {
        class: FailureClass::ProcessCrash,
        exit_code: None,
        stderr_tail: None,
        message: Some("daemon restarted while the operation was active".to_owned()),
    }
}

fn restart_error() -> OperationError {
    OperationError {
        code: ErrorCode::ProcessCrash,
        message: "daemon restarted while the operation was active".to_owned(),
    }
}
