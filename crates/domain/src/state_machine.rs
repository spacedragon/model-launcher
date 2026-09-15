//! Pure state machines: the instance load-state machine and the operation
//! state machine (`docs/architecture.md` §3, §5).
//!
//! Both are expressed as explicit target-state transition tables so every legal
//! move is visible in one place and illegal moves are rejected deterministically.
//! `can_transition` / `transition` are pure functions — no I/O, no mutation of
//! the caller's state — so they are trivially unit-testable and can be reused
//! by the runtime supervisor, the scheduler, and the API layer.
//!
//! ## Instance state machine (`docs/architecture.md` §5)
//!
//! ```text
//! unloaded -> queued -> loading -> ready
//!                    |          |
//!                    v          v
//!                  failed    draining -> unloading -> unloaded
//!                                   |
//!                                   v
//!                                 ready   (drain cancelled / timed-out fallback)
//!
//! any running state --unexpected exit--> crashed
//! crashed --explicit load----------------> queued
//! ```
//!
//! Notes on the encoding choices (documented, and asserted in tests):
//! - `failed` and `crashed` are both resumable via an explicit load
//!   (`-> queued`), so a load can never be stuck in a permanent terminal
//!   state (`docs/development-plan.md` §5: no permanent `loading`).
//! - A load that never reaches `ready` ends in `failed`; an unexpected exit of
//!   a serving (`ready`) process ends in `crashed`. A spawned-but-not-ready
//!   process may also be torn down via `unloading`.
//! - `draining -> ready` is the drain-cancelled / timed-out fallback.
//! - Any running state (`loading`/`ready`/`draining`) may reach `crashed` on an
//!   unexpected child exit (`docs/architecture.md` §5: 任意运行态 → crashed), so
//!   `draining` (still a live process serving existing requests) also crashes.

use crate::error::{DomainError, ErrorCode, Result};
use crate::model::{InstanceState, OperationState};

impl InstanceState {
    /// Target states reachable from `self` in a single step.
    const fn transitions(self) -> &'static [InstanceState] {
        use InstanceState as S;
        match self {
            S::Queued => &[S::Loading, S::Unloaded, S::Failed],
            S::Loading => &[S::Ready, S::Failed, S::Unloading, S::Crashed],
            S::Ready => &[S::Draining, S::Crashed],
            S::Draining => &[S::Unloading, S::Ready, S::Crashed],
            S::Unloading => &[S::Unloaded],
            S::Unloaded | S::Failed | S::Crashed => &[S::Queued],
        }
    }

    /// Whether moving from `self` to `target` is a legal single step.
    #[must_use]
    pub fn can_transition_to(self, target: InstanceState) -> bool {
        self.transitions().contains(&target)
    }

    /// Whether `self` has no outgoing transition (i.e. cannot change). For this
    /// machine no state is truly terminal — even `unloaded` can be re-queued —
    /// so this is always `false`; it exists for symmetry with
    /// [`OperationState::is_terminal`].
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        self.transitions().is_empty()
    }

    /// The states that can legally follow a `ready` instance (drain / crash).
    #[must_use]
    pub const fn ready_followers() -> &'static [InstanceState] {
        InstanceState::Ready.transitions()
    }
}

impl OperationState {
    /// Target states reachable from `self` in a single step.
    const fn transitions(self) -> &'static [OperationState] {
        use OperationState as S;
        match self {
            S::Queued => &[S::Running, S::Cancelled],
            S::Running => &[S::Succeeded, S::Failed, S::Cancelled],
            S::Succeeded | S::Failed | S::Cancelled => &[],
        }
    }

    /// Whether moving from `self` to `target` is a legal single step.
    #[must_use]
    pub fn can_transition_to(self, target: OperationState) -> bool {
        self.transitions().contains(&target)
    }

    /// Whether `self` is a terminal operation state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        self.transitions().is_empty()
    }
}

/// Attempt to move an instance from `from` to `to`.
///
/// Returns the (unchanged) target on success, or a
/// [`ErrorCode::InvalidStateTransition`] error on an illegal move. Pure: it
/// does not mutate anything; the caller applies the returned state.
///
/// # Errors
///
/// [`ErrorCode::InvalidStateTransition`] when `to` is not reachable from
/// `from` in the instance transition table.
pub fn transition_instance(from: InstanceState, to: InstanceState) -> Result<InstanceState> {
    if from.can_transition_to(to) {
        Ok(to)
    } else {
        Err(DomainError::with_message(
            ErrorCode::InvalidStateTransition,
            format!("instance cannot move from {from:?} to {to:?}"),
        ))
    }
}

/// Attempt to move an operation from `from` to `to`.
///
/// Returns the target on success, or a [`ErrorCode::InvalidStateTransition`]
/// error on an illegal move.
///
/// # Errors
///
/// [`ErrorCode::InvalidStateTransition`] when `to` is not reachable from
/// `from` in the operation transition table.
pub fn transition_operation(from: OperationState, to: OperationState) -> Result<OperationState> {
    if from.can_transition_to(to) {
        Ok(to)
    } else {
        Err(DomainError::with_message(
            ErrorCode::InvalidStateTransition,
            format!("operation cannot move from {from:?} to {to:?}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InstanceState as S, OperationState as O};

    /// Every state, for exhaustive matrix testing.
    const ALL_INSTANCE: [InstanceState; 8] = [
        S::Unloaded,
        S::Queued,
        S::Loading,
        S::Ready,
        S::Draining,
        S::Unloading,
        S::Failed,
        S::Crashed,
    ];

    const ALL_OPERATION: [OperationState; 5] =
        [O::Queued, O::Running, O::Succeeded, O::Failed, O::Cancelled];

    #[test]
    fn instance_happy_path_load_and_unload() {
        // unloaded -> queued -> loading -> ready
        assert_eq!(
            transition_instance(S::Unloaded, S::Queued).expect("unloaded->queued"),
            S::Queued
        );
        assert_eq!(
            transition_instance(S::Queued, S::Loading).expect("queued->loading"),
            S::Loading
        );
        assert_eq!(
            transition_instance(S::Loading, S::Ready).expect("loading->ready"),
            S::Ready
        );
        // ready -> draining -> unloading -> unloaded
        assert_eq!(
            transition_instance(S::Ready, S::Draining).expect("ready->draining"),
            S::Draining
        );
        assert_eq!(
            transition_instance(S::Draining, S::Unloading).expect("draining->unloading"),
            S::Unloading
        );
        assert_eq!(
            transition_instance(S::Unloading, S::Unloaded).expect("unloading->unloaded"),
            S::Unloaded
        );
    }

    #[test]
    fn drain_can_fall_back_to_ready() {
        assert_eq!(
            transition_instance(S::Draining, S::Ready).expect("draining->ready"),
            S::Ready
        );
    }

    #[test]
    fn load_failures_reach_failed_or_crashed() {
        assert_eq!(
            transition_instance(S::Queued, S::Failed).expect("queued->failed"),
            S::Failed
        );
        assert_eq!(
            transition_instance(S::Loading, S::Failed).expect("loading->failed"),
            S::Failed
        );
        assert_eq!(
            transition_instance(S::Loading, S::Crashed).expect("loading->crashed"),
            S::Crashed
        );
        assert_eq!(
            transition_instance(S::Loading, S::Unloading).expect("loading->unloading"),
            S::Unloading
        );
    }

    #[test]
    fn ready_can_crash() {
        assert_eq!(
            transition_instance(S::Ready, S::Crashed).expect("ready->crashed"),
            S::Crashed
        );
    }

    /// A draining instance is still a live process serving existing requests,
    /// so an unexpected child exit is a crash, not a clean unload
    /// (`docs/architecture.md` §5: 任意运行态 → crashed).
    #[test]
    fn draining_can_crash() {
        assert_eq!(
            transition_instance(S::Draining, S::Crashed).expect("draining->crashed"),
            S::Crashed
        );
    }

    #[test]
    fn failed_and_crashed_resume_via_explicit_load() {
        assert_eq!(
            transition_instance(S::Failed, S::Queued).expect("failed->queued"),
            S::Queued
        );
        assert_eq!(
            transition_instance(S::Crashed, S::Queued).expect("crashed->queued"),
            S::Queued
        );
    }

    #[test]
    fn queued_can_be_cancelled_back_to_unloaded() {
        assert_eq!(
            transition_instance(S::Queued, S::Unloaded).expect("queued->unloaded"),
            S::Unloaded
        );
    }

    #[test]
    fn instance_illegal_transitions_are_rejected() {
        // unloaded cannot jump straight to ready/loading.
        assert!(matches!(
            transition_instance(S::Unloaded, S::Ready),
            Err(e) if e.code == ErrorCode::InvalidStateTransition
        ));
        assert!(!S::Unloaded.can_transition_to(S::Ready));
        assert!(!S::Unloaded.can_transition_to(S::Loading));

        // ready cannot go straight to unloaded (must drain/unload first).
        assert!(!S::Ready.can_transition_to(S::Unloaded));
        // ready cannot go to loading.
        assert!(!S::Ready.can_transition_to(S::Loading));

        // unloading has only one successor (unloaded), not a crash.
        assert!(!S::Unloading.can_transition_to(S::Crashed));

        // a self-loop is not a legal step.
        assert!(!S::Ready.can_transition_to(S::Ready));
    }

    #[test]
    fn transition_error_reports_code_and_message() {
        let err = transition_instance(S::Unloaded, S::Ready).expect_err("must fail");
        assert_eq!(err.code, ErrorCode::InvalidStateTransition);
        assert!(err.message.contains("Unloaded"), "message: {}", err.message);
        assert!(err.message.contains("Ready"), "message: {}", err.message);
    }

    /// Exhaustive: the transition matrix matches the documented table exactly.
    /// Each entry is the set of legal targets for a given source.
    #[test]
    fn instance_transition_matrix_is_exact() {
        let expected: &[(&InstanceState, &[InstanceState])] = &[
            (&S::Unloaded, &[S::Queued]),
            (&S::Queued, &[S::Loading, S::Unloaded, S::Failed]),
            (
                &S::Loading,
                &[S::Ready, S::Failed, S::Unloading, S::Crashed],
            ),
            (&S::Ready, &[S::Draining, S::Crashed]),
            (&S::Draining, &[S::Unloading, S::Ready, S::Crashed]),
            (&S::Unloading, &[S::Unloaded]),
            (&S::Failed, &[S::Queued]),
            (&S::Crashed, &[S::Queued]),
        ];
        for (source, targets) in expected {
            for candidate in ALL_INSTANCE {
                let legal = targets.contains(&candidate);
                assert_eq!(
                    source.can_transition_to(candidate),
                    legal,
                    "matrix mismatch: {source:?} -> {candidate:?} (expected {legal})"
                );
            }
        }
    }

    #[test]
    fn operation_happy_path_and_terminals() {
        assert_eq!(
            transition_operation(O::Queued, O::Running).expect("queued->running"),
            O::Running
        );
        assert_eq!(
            transition_operation(O::Running, O::Succeeded).expect("running->succeeded"),
            O::Succeeded
        );
        assert!(O::Succeeded.is_terminal());
        assert!(O::Failed.is_terminal());
        assert!(O::Cancelled.is_terminal());
        assert!(!O::Queued.is_terminal());
        assert!(!O::Running.is_terminal());
    }

    #[test]
    fn operation_cancel_from_queued_or_running() {
        assert_eq!(
            transition_operation(O::Queued, O::Cancelled).expect("queued->cancelled"),
            O::Cancelled
        );
        assert_eq!(
            transition_operation(O::Running, O::Cancelled).expect("running->cancelled"),
            O::Cancelled
        );
        assert_eq!(
            transition_operation(O::Running, O::Failed).expect("running->failed"),
            O::Failed
        );
    }

    #[test]
    fn operation_illegal_transitions_are_rejected() {
        // Terminal states have no successors.
        assert!(!O::Succeeded.can_transition_to(O::Running));
        assert!(!O::Failed.can_transition_to(O::Running));
        assert!(!O::Cancelled.can_transition_to(O::Running));
        // queued cannot jump straight to succeeded/failed (must run first).
        assert!(!O::Queued.can_transition_to(O::Succeeded));
        assert!(!O::Queued.can_transition_to(O::Failed));
        // a self-loop is not a legal step.
        assert!(!O::Running.can_transition_to(O::Running));
        assert!(matches!(
            transition_operation(O::Succeeded, O::Queued),
            Err(e) if e.code == ErrorCode::InvalidStateTransition
        ));
    }

    #[test]
    fn operation_transition_matrix_is_exact() {
        let expected: &[(&OperationState, &[OperationState])] = &[
            (&O::Queued, &[O::Running, O::Cancelled]),
            (&O::Running, &[O::Succeeded, O::Failed, O::Cancelled]),
            (&O::Succeeded, &[]),
            (&O::Failed, &[]),
            (&O::Cancelled, &[]),
        ];
        for (source, targets) in expected {
            for candidate in ALL_OPERATION {
                let legal = targets.contains(&candidate);
                assert_eq!(
                    source.can_transition_to(candidate),
                    legal,
                    "matrix mismatch: {source:?} -> {candidate:?} (expected {legal})"
                );
            }
        }
    }
}
