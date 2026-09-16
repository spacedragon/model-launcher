//! Error vocabulary and constructors for the scanner.
//!
//! No new error codes are invented: every failure maps onto the frozen
//! `model-serving-domain` catalog (`docs/api.md` §2.2):
//!
//! - an unusable *configuration* (relative / symlinked / missing / disabled
//!   model root, no such root id) is a client-visible request problem →
//!   [`ErrorCode::InvalidRequest`], except a missing root id, which is exactly
//!   [`ErrorCode::ModelNotFound`]'s "the object does not exist" meaning;
//! - ledger / filesystem failures while scanning are [`ErrorCode::Internal`].

use std::path::Path;

use model_serving_domain::error::{DomainError, ErrorCode};

/// The scanner's fallible operations all resolve to the frozen domain error
/// catalog (`docs/api.md` §2.2) — the scanner invents no new error codes.
pub type Result<T> = std::result::Result<T, DomainError>;

/// A configured model root did not pass validation.
#[must_use]
pub fn invalid_root(path: &Path, reason: &str) -> DomainError {
    DomainError::with_message(
        ErrorCode::InvalidRequest,
        format!("invalid model root `{}`: {reason}", path.display()),
    )
}

/// The requested root id is not registered.
#[must_use]
pub fn root_not_found(root_id: &str) -> DomainError {
    DomainError::with_message(
        ErrorCode::ModelNotFound,
        format!("model root {root_id} is not registered"),
    )
}

/// The root exists but is disabled, so it must not be scanned.
#[must_use]
pub fn root_disabled(root_id: &str) -> DomainError {
    DomainError::with_message(
        ErrorCode::InvalidRequest,
        format!("model root {root_id} is disabled"),
    )
}

/// A filesystem or ledger failure inside the scan (the transaction rolls back).
#[must_use]
pub fn storage(message: String) -> DomainError {
    DomainError::with_message(ErrorCode::Internal, message)
}

/// A failed reconciliation write, with the table/statement in the message.
#[must_use]
pub fn reconcile(context: &str, message: &str) -> DomainError {
    DomainError::with_message(ErrorCode::Internal, format!("{context}: {message}"))
}
