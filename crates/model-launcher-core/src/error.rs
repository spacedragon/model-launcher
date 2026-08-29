use thiserror::Error;

pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("model key is invalid")]
    InvalidModelKey,
    #[error("model was not found")]
    ModelNotFound,
    #[error("another model is busy")]
    ModelBusy,
    #[error("model is starting")]
    ModelStarting,
    #[error("model failed to load")]
    ModelLoadFailed(#[source] BoxError),
    #[error("engine is unavailable")]
    EngineUnavailable,
    #[error("invalid setting: {0}")]
    InvalidSetting(&'static str),
    #[error("invalid log limit: {0}")]
    InvalidLogLimit(&'static str),
    #[error("engine process failed")]
    EngineProcess(#[source] BoxError),
    #[error("configuration I/O failed")]
    ConfigIo(#[source] BoxError),
    #[error("configuration format is invalid")]
    ConfigFormat(#[source] BoxError),
}

impl AppError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidModelKey => "invalid_model_key",
            Self::ModelNotFound => "model_not_found",
            Self::ModelBusy => "model_busy",
            Self::ModelStarting => "model_starting",
            Self::ModelLoadFailed(_) => "model_load_failed",
            Self::EngineUnavailable => "engine_unavailable",
            Self::InvalidSetting(_) => "invalid_setting",
            Self::InvalidLogLimit(_) => "invalid_log_limit",
            Self::EngineProcess(_) => "engine_process",
            Self::ConfigIo(_) => "config_io",
            Self::ConfigFormat(_) => "config_format",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, io};

    use super::*;

    #[test]
    fn application_errors_preserve_source_chain() {
        let cases = [
            (
                AppError::ModelLoadFailed(Box::new(io::Error::other("disk diagnostic"))),
                "model failed to load",
                "disk diagnostic",
            ),
            (
                AppError::EngineProcess(Box::new(io::Error::other("process diagnostic"))),
                "engine process failed",
                "process diagnostic",
            ),
        ];

        for (error, message, diagnostic) in cases {
            assert_eq!(error.to_string(), message);
            assert_eq!(
                error.source().expect("owned source").to_string(),
                diagnostic
            );
        }
    }

    #[test]
    fn application_errors_have_stable_codes() {
        let cases = [
            (AppError::InvalidModelKey, "invalid_model_key"),
            (AppError::ModelNotFound, "model_not_found"),
            (AppError::ModelBusy, "model_busy"),
            (AppError::ModelStarting, "model_starting"),
            (
                AppError::ModelLoadFailed(Box::new(io::Error::other("load diagnostic"))),
                "model_load_failed",
            ),
            (AppError::EngineUnavailable, "engine_unavailable"),
            (
                AppError::InvalidSetting("context_length"),
                "invalid_setting",
            ),
            (
                AppError::InvalidLogLimit("broadcast_capacity"),
                "invalid_log_limit",
            ),
            (
                AppError::EngineProcess(Box::new(io::Error::other("process diagnostic"))),
                "engine_process",
            ),
            (
                AppError::ConfigIo(Box::new(io::Error::other("config diagnostic"))),
                "config_io",
            ),
            (
                AppError::ConfigFormat(Box::new(io::Error::other("format diagnostic"))),
                "config_format",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(error.code(), expected);
        }
    }
}
