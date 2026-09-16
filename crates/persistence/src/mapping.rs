//! Row <-> domain mapping helpers and the error mapping from `sqlx` failures
//! onto the frozen `model-serving-domain` error catalog.
//!
//! Storage failures never panic and never invent error codes:
//!
//! - a state token that violates one of the state-fence `CHECK` constraints
//!   (`instance_state_valid`, `instance_desired_state_valid`,
//!   `operation_state_valid`) maps to [`ErrorCode::InvalidStateTransition`];
//! - any other integrity violation (unique key, foreign key, other checks)
//!   maps to [`ErrorCode::InvalidRequest`];
//! - anything else (I/O, decode, corrupt JSON, driver) maps to
//!   [`ErrorCode::Internal`].
//!
//! Callers translate the "not found" shape themselves: repositories return
//! [`Option`] variants for that, and the required lookups map `None` to
//! [`ErrorCode::ModelNotFound`] / [`ErrorCode::InstanceNotFound`].

use chrono::{DateTime, SecondsFormat, Utc};
use model_serving_domain::error::{DomainError, ErrorCode, Result};

/// The named `CHECK` constraints that fence the state columns on their
/// state-machine vocabularies (see `migrations/0001_initial.sql`). A
/// constraint violation naming one of these is a rejected state write, not a
/// generic data error.
pub(crate) const STATE_FENCE_CONSTRAINTS: [&str; 3] = [
    "instance_state_valid",
    "instance_desired_state_valid",
    "operation_state_valid",
];

/// Current UTC time for `updated_at` / `created_at` / audit rows.
#[must_use]
pub(crate) fn now() -> DateTime<Utc> {
    Utc::now()
}

/// Render a timestamp to its RFC 3339 UTC TEXT column value (millisecond
/// precision, `Z` suffix — valid RFC 3339).
#[must_use]
pub(crate) fn ts_string(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Parse an RFC 3339 TEXT timestamp column back to `DateTime<Utc>`.
///
/// # Errors
///
/// [`ErrorCode::Internal`] if `raw` is not a parseable RFC 3339 timestamp
/// (corrupt row).
pub(crate) fn parse_ts(raw: &str, column: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|v| v.with_timezone(&Utc))
        .map_err(|e| {
            DomainError::with_message(
                ErrorCode::Internal,
                format!("stored {column} is not RFC 3339 (got {raw:?}): {e}"),
            )
        })
}

/// Serialize a structured payload to its JSON TEXT column value.
///
/// # Errors
///
/// [`ErrorCode::Internal`] if `value` cannot be serialized.
pub(crate) fn to_json<T: serde::Serialize>(value: &T, field: &str) -> Result<String> {
    serde_json::to_string(value).map_err(|e| {
        DomainError::with_message(
            ErrorCode::Internal,
            format!("serialize {field} failed: {e}"),
        )
    })
}

/// Parse a JSON TEXT column back into a structured payload. A corrupt value
/// is a storage integrity problem (`Internal`), never a panic.
///
/// # Errors
///
/// [`ErrorCode::Internal`] if `raw` is not valid JSON of the target type.
pub(crate) fn from_json<T: serde::de::DeserializeOwned>(raw: &str, field: &str) -> Result<T> {
    serde_json::from_str(raw).map_err(|e| {
        DomainError::with_message(
            ErrorCode::Internal,
            format!("stored {field} is not valid JSON: {e}"),
        )
    })
}

/// Render an enum to its docs/api.md wire token (lowercase / `snake_case`
/// string, no surrounding quotes). All state/kind/class enums serialize to
/// plain JSON strings, so this cannot fail.
#[must_use]
pub(crate) fn wire_token<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default()
}

/// Parse a wire token back into its enum. An unknown token is data corruption
/// (the `CHECK` constraint should have prevented it; treat as `Internal`).
///
/// # Errors
///
/// [`ErrorCode::Internal`] if `raw` is not a token of the target type.
pub(crate) fn parse_wire<T: serde::de::DeserializeOwned>(raw: &str, field: &str) -> Result<T> {
    serde_json::from_str(&format!("\"{raw}\"")).map_err(|e| {
        DomainError::with_message(
            ErrorCode::Internal,
            format!("stored {field} is not a known token (got {raw:?}): {e}"),
        )
    })
}

/// Clamp a byte size to the SQLite `INTEGER` domain (values above
/// `i64::MAX` are representably huge files; clamping keeps the row writable).
#[must_use]
pub(crate) fn as_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Decode a non-negative integer column back to an unsigned domain field.
///
/// # Errors
///
/// [`ErrorCode::Internal`] if `value` is negative (corrupt row).
pub(crate) fn from_i64<T: TryFrom<i64> + std::fmt::Display>(value: i64, field: &str) -> Result<T> {
    T::try_from(value).map_err(|_| {
        DomainError::with_message(
            ErrorCode::Internal,
            format!("stored {field} is out of range (got {value})"),
        )
    })
}

/// Translate a `sqlx::Error` into the domain error catalog (see module docs).
#[must_use]
pub(crate) fn storage_error(err: &sqlx::Error, context: &str) -> DomainError {
    match err {
        sqlx::Error::RowNotFound => {
            DomainError::with_message(ErrorCode::Internal, format!("{context}: row not found"))
        }
        sqlx::Error::Database(db) => {
            // `sqlx::DatabaseError` exposes no structured constraint kind for
            // SQLite in 0.8, so the message is the contract: every SQLite
            // constraint violation is reported as "... constraint failed: ...".
            let message = db.message();
            if message.contains("CHECK constraint failed") {
                // Only the *named* state fences are state-machine violations;
                // every other CHECK (unnamed kind fences, value ranges,
                // booleans) is a data-integrity problem -> `InvalidRequest`.
                let fence = STATE_FENCE_CONSTRAINTS
                    .iter()
                    .find(|name| message.contains(*name));
                match fence {
                    Some(name) => DomainError::with_message(
                        ErrorCode::InvalidStateTransition,
                        format!("{context}: state rejected by the {name} constraint"),
                    ),
                    None => DomainError::with_message(
                        ErrorCode::InvalidRequest,
                        format!("{context}: check constraint violated: {message}"),
                    ),
                }
            } else if message.contains("constraint failed") {
                DomainError::with_message(
                    ErrorCode::InvalidRequest,
                    format!("{context}: {message}"),
                )
            } else {
                DomainError::with_message(
                    ErrorCode::Internal,
                    format!("{context}: database error: {err}"),
                )
            }
        }
        _ => DomainError::with_message(ErrorCode::Internal, format!("{context}: {err}")),
    }
}
