use model_launcher_core::{EngineCapabilities, ModelRecord};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct ApiModel {
    pub record: ModelRecord,
    pub publisher: String,
    pub architecture: Option<String>,
    pub quantization: Option<String>,
    pub params_string: Option<String>,
    pub capabilities: EngineCapabilities,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LmModel {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub publisher: String,
    pub key: String,
    pub display_name: String,
    pub architecture: Option<String>,
    pub quantization: Option<String>,
    pub size_bytes: u64,
    pub params_string: Option<String>,
}

impl From<&ApiModel> for LmModel {
    fn from(model: &ApiModel) -> Self {
        Self {
            kind: "llm",
            publisher: model.publisher.clone(),
            key: model.record.key.to_string(),
            display_name: model.record.display_name.clone(),
            architecture: model.architecture.clone(),
            quantization: model.quantization.clone(),
            size_bytes: model.record.size_bytes,
            params_string: model.params_string.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LoadRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_batch_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flash_attention: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_experts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offload_kv_cache_to_gpu: Option<bool>,
    #[serde(default)]
    pub echo_load_config: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnloadRequest {
    pub instance_id: String,
}

#[derive(Debug, Serialize)]
pub struct LoadResponse {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub model_instance_id: String,
    pub load_time_seconds: f64,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_config: Option<LoadConfig>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct LoadConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_batch_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flash_attention: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_experts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offload_kv_cache_to_gpu: Option<bool>,
}

impl From<&LoadRequest> for LoadConfig {
    fn from(value: &LoadRequest) -> Self {
        Self {
            context_length: value.context_length,
            eval_batch_size: value.eval_batch_size,
            flash_attention: value.flash_attention,
            num_experts: value.num_experts,
            offload_kv_cache_to_gpu: value.offload_kv_cache_to_gpu,
        }
    }
}
