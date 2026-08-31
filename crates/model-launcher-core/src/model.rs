use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use uuid::Uuid;

use crate::{AppError, CatalogIdentity, LaunchSettings};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(Uuid);

impl ModelId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for ModelId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ModelKey(String);

impl ModelKey {
    pub fn parse(value: impl Into<String>) -> Result<Self, AppError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 255
            && !value.starts_with('/')
            && !value.ends_with('/')
            && value.split('/').all(|segment| {
                !segment.is_empty()
                    && segment != "."
                    && segment != ".."
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            });

        if valid {
            Ok(Self(value))
        } else {
            Err(AppError::InvalidModelKey)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ModelKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl std::fmt::Display for ModelKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    Available,
    Missing,
    Unlaunchable { reason: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchProfile {
    pub settings: LaunchSettings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRecord {
    pub id: ModelId,
    pub key: ModelKey,
    pub display_name: String,
    pub path: PathBuf,
    /// Best-effort platform file identity used to reconnect moves without hashing model data.
    #[serde(default)]
    pub file_identity: CatalogIdentity,
    pub size_bytes: u64,
    pub state: ModelState,
    pub launch_profile: LaunchProfile,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_key_accepts_catalog_keys() {
        let key = ModelKey::parse("Qwen/qwen3-8b-q4").expect("valid model key");

        assert_eq!(key.as_str(), "Qwen/qwen3-8b-q4");
    }

    #[test]
    fn model_key_rejects_parent_traversal() {
        assert!(ModelKey::parse("../escape").is_err());
    }

    #[test]
    fn model_key_deserialization_rejects_invalid_values() {
        assert!(serde_json::from_str::<ModelKey>(r#""../escape""#).is_err());
    }

    #[test]
    fn model_key_serialization_round_trips_valid_values() {
        let key = ModelKey::parse("Qwen/qwen3-8b-q4").expect("valid model key");
        let json = serde_json::to_string(&key).expect("serialize model key");

        assert_eq!(
            serde_json::from_str::<ModelKey>(&json).expect("deserialize model key"),
            key
        );
    }
}
