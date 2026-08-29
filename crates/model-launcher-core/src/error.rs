use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AppError {
    #[error("model key is invalid")]
    InvalidModelKey,
    #[error("model was not found")]
    ModelNotFound,
    #[error("another model is busy")]
    ModelBusy,
    #[error("model is starting")]
    ModelStarting,
    #[error("model failed to load: {0}")]
    ModelLoadFailed(String),
    #[error("engine is unavailable")]
    EngineUnavailable,
    #[error("invalid setting: {0}")]
    InvalidSetting(&'static str),
    #[error("engine process failed: {0}")]
    EngineProcess(String),
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
            Self::EngineProcess(_) => "engine_process",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_errors_have_stable_codes() {
        assert_eq!(AppError::InvalidModelKey.code(), "invalid_model_key");
        assert_eq!(AppError::EngineUnavailable.code(), "engine_unavailable");
    }
}
