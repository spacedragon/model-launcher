use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{AppError, CatalogMetadata};

macro_rules! positive_setting {
    ($name:ident, $code:literal) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
        #[serde(transparent)]
        pub struct $name(u32);

        impl $name {
            pub fn new(value: u32) -> Result<Self, AppError> {
                if value == 0 {
                    Err(AppError::InvalidSetting($code))
                } else {
                    Ok(Self(value))
                }
            }

            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(u32::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }
    };
}

positive_setting!(ContextLength, "context_length");
positive_setting!(CpuThreads, "cpu_threads");
positive_setting!(BatchSize, "batch_size");
positive_setting!(ParallelSlots, "parallel_slots");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GpuLayers(u32);

impl GpuLayers {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvCacheType {
    F16,
    Q8_0,
    Q4_0,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpeculativeType {
    DraftMtp,
    DraftDflash,
}

impl SpeculativeType {
    const fn as_arg(self) -> &'static str {
        match self {
            Self::DraftMtp => "draft-mtp",
            Self::DraftDflash => "draft-dflash",
        }
    }
}

impl KvCacheType {
    const fn as_arg(self) -> &'static str {
        match self {
            Self::F16 => "f16",
            Self::Q8_0 => "q8_0",
            Self::Q4_0 => "q4_0",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSettings {
    pub context_length: Option<ContextLength>,
    pub gpu_layers: Option<GpuLayers>,
    pub cpu_threads: Option<CpuThreads>,
    pub batch_size: Option<BatchSize>,
    pub parallel_slots: Option<ParallelSlots>,
    pub flash_attention: Option<bool>,
    pub kv_cache_type: Option<KvCacheType>,
    pub speculative_type: Option<SpeculativeType>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingId {
    ContextLength,
    GpuLayers,
    CpuThreads,
    BatchSize,
    ParallelSlots,
    FlashAttention,
    KvCacheType,
    SpeculativeType,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedLaunchSettings {
    pub args: Vec<String>,
    pub unsupported: Vec<SettingId>,
}

impl LaunchSettings {
    #[must_use]
    pub fn to_args(&self, capabilities: &EngineCapabilities) -> Vec<String> {
        self.render(capabilities).args
    }

    #[must_use]
    pub fn render(&self, capabilities: &EngineCapabilities) -> RenderedLaunchSettings {
        let mut args = Vec::new();
        let mut unsupported = Vec::new();
        render_numeric(
            &mut args,
            &mut unsupported,
            capabilities.context_length,
            SettingId::ContextLength,
            "--ctx-size",
            self.context_length.map(ContextLength::get),
        );
        render_numeric(
            &mut args,
            &mut unsupported,
            capabilities.gpu_layers,
            SettingId::GpuLayers,
            "--gpu-layers",
            self.gpu_layers.map(GpuLayers::get),
        );
        render_numeric(
            &mut args,
            &mut unsupported,
            capabilities.cpu_threads,
            SettingId::CpuThreads,
            "--threads",
            self.cpu_threads.map(CpuThreads::get),
        );
        render_numeric(
            &mut args,
            &mut unsupported,
            capabilities.batch_size,
            SettingId::BatchSize,
            "--batch-size",
            self.batch_size.map(BatchSize::get),
        );
        render_numeric(
            &mut args,
            &mut unsupported,
            capabilities.parallel_slots,
            SettingId::ParallelSlots,
            "--parallel",
            self.parallel_slots.map(ParallelSlots::get),
        );
        if self.flash_attention == Some(true) {
            if capabilities.flash_attention {
                args.extend(["--flash-attn".into(), "on".into()]);
            } else {
                unsupported.push(SettingId::FlashAttention);
            }
        }
        if let Some(value) = self.kv_cache_type {
            if capabilities.kv_cache_type {
                args.extend([
                    "--cache-type-k".into(),
                    value.as_arg().into(),
                    "--cache-type-v".into(),
                    value.as_arg().into(),
                ]);
            } else {
                unsupported.push(SettingId::KvCacheType);
            }
        }
        if let Some(value) = self.speculative_type {
            if capabilities.speculative_type {
                args.extend(["--spec-type".into(), value.as_arg().into()]);
            } else {
                unsupported.push(SettingId::SpeculativeType);
            }
        }
        RenderedLaunchSettings { args, unsupported }
    }
}

fn render_numeric(
    args: &mut Vec<String>,
    unsupported: &mut Vec<SettingId>,
    supported: bool,
    id: SettingId,
    flag: &str,
    value: Option<u32>,
) {
    if let Some(value) = value {
        if supported {
            args.extend([flag.into(), value.to_string()]);
        } else {
            unsupported.push(id);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineCapabilities {
    pub context_length: bool,
    pub gpu_layers: bool,
    pub cpu_threads: bool,
    pub batch_size: bool,
    pub parallel_slots: bool,
    pub flash_attention: bool,
    pub kv_cache_type: bool,
    pub speculative_type: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContextEstimate {
    pub model_context_limit: u32,
    pub vram_context_limit: Option<u32>,
    pub recommended_context: u32,
    pub kv_bytes_per_token: Option<u64>,
    pub estimated_weight_bytes: u64,
    pub safety_reserve_bytes: u64,
}

/// Estimates the llama.cpp KV allocation using the same tensor dimensions used by
/// `llama_kv_cache`: one K and one V row per cache cell for every attention layer.
#[must_use]
pub fn estimate_context(
    metadata: &CatalogMetadata,
    model_size_bytes: u64,
    settings: &LaunchSettings,
    total_vram_bytes: u64,
    free_vram_bytes: u64,
) -> ContextEstimate {
    const STEP: u64 = 512;
    const DEFAULT_CONTEXT: u64 = 32_768;
    const MAX_CONTEXT: u64 = 1_048_576;
    const GIB: u64 = 1024 * 1024 * 1024;

    let model_context_limit = metadata
        .context_length
        .unwrap_or(DEFAULT_CONTEXT)
        .clamp(STEP, MAX_CONTEXT);
    let block_count = metadata.block_count.filter(|value| *value > 0);
    let attention_layers = block_count.and_then(|blocks| {
        metadata
            .full_attention_interval
            .filter(|interval| *interval > 0)
            .map_or(Some(blocks), |interval| {
                blocks.checked_add(interval - 1).map(|v| v / interval)
            })
    });
    let head_count = metadata.attention_head_count.filter(|value| *value > 0);
    let kv_head_count = metadata
        .attention_head_count_kv
        .filter(|value| *value > 0)
        .or(head_count);
    let default_head_length = metadata
        .embedding_length
        .zip(head_count)
        .map(|(embedding, heads)| embedding / heads)
        .filter(|value| *value > 0);
    let key_width = metadata
        .attention_key_length
        .or(default_head_length)
        .zip(kv_head_count)
        .and_then(|(length, heads)| length.checked_mul(heads));
    let value_width = metadata
        .attention_value_length
        .or(default_head_length)
        .zip(kv_head_count)
        .and_then(|(length, heads)| length.checked_mul(heads));
    let cache_type = settings.kv_cache_type.unwrap_or(KvCacheType::F16);
    let kv_bytes_per_token =
        attention_layers
            .zip(key_width)
            .zip(value_width)
            .and_then(|((layers, key), value)| {
                kv_row_bytes(key, cache_type)
                    .checked_add(kv_row_bytes(value, cache_type))?
                    .checked_mul(layers)
            });

    let estimated_weight_bytes = match (settings.gpu_layers, block_count) {
        (Some(layers), Some(blocks)) => {
            let offloaded = u64::from(layers.get()).min(blocks);
            u64::try_from(u128::from(model_size_bytes) * u128::from(offloaded) / u128::from(blocks))
                .unwrap_or(u64::MAX)
        }
        (Some(layers), None) if layers.get() == 0 => 0,
        _ => model_size_bytes,
    };
    let safety_reserve_bytes = GIB.max(total_vram_bytes / 10);
    let available_for_kv = free_vram_bytes
        .saturating_sub(safety_reserve_bytes)
        .saturating_sub(estimated_weight_bytes);
    let vram_context_limit = kv_bytes_per_token.map(|bytes| {
        let tokens = available_for_kv / bytes.max(1);
        let stepped = tokens / STEP * STEP;
        u32::try_from(stepped.clamp(STEP, model_context_limit)).unwrap_or(u32::MAX)
    });
    let effective_limit = vram_context_limit
        .map(u64::from)
        .unwrap_or(model_context_limit)
        .min(model_context_limit);
    let recommended = (effective_limit.saturating_mul(4) / 5) / STEP * STEP;

    ContextEstimate {
        model_context_limit: u32::try_from(model_context_limit).unwrap_or(u32::MAX),
        vram_context_limit,
        recommended_context: u32::try_from(recommended.max(STEP)).unwrap_or(u32::MAX),
        kv_bytes_per_token,
        estimated_weight_bytes,
        safety_reserve_bytes,
    }
}

fn kv_row_bytes(elements: u64, cache_type: KvCacheType) -> u64 {
    match cache_type {
        KvCacheType::F16 => elements.saturating_mul(2),
        KvCacheType::Q8_0 => elements.div_ceil(32).saturating_mul(34),
        KvCacheType::Q4_0 => elements.div_ceil(32).saturating_mul(18),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_settings_validate_during_deserialization() {
        assert!(serde_json::from_str::<ContextLength>("0").is_err());
        assert!(serde_json::from_str::<CpuThreads>("0").is_err());
        assert!(serde_json::from_str::<BatchSize>("0").is_err());
        assert!(serde_json::from_str::<ParallelSlots>("0").is_err());

        assert_eq!(
            serde_json::from_str::<ContextLength>("8192").expect("positive context length"),
            ContextLength::new(8192).expect("positive context length")
        );
        assert_eq!(
            serde_json::from_str::<CpuThreads>("8").expect("positive CPU threads"),
            CpuThreads::new(8).expect("positive CPU threads")
        );
        assert_eq!(
            serde_json::from_str::<BatchSize>("512").expect("positive batch size"),
            BatchSize::new(512).expect("positive batch size")
        );
        assert_eq!(
            serde_json::from_str::<ParallelSlots>("4").expect("positive parallel slots"),
            ParallelSlots::new(4).expect("positive parallel slots")
        );
    }

    #[test]
    fn launch_arguments_are_gated_by_engine_capabilities() {
        let settings = LaunchSettings {
            context_length: Some(ContextLength::new(8192).expect("valid context length")),
            gpu_layers: Some(GpuLayers::new(32)),
            ..LaunchSettings::default()
        };
        let caps = EngineCapabilities {
            context_length: true,
            ..EngineCapabilities::default()
        };

        assert_eq!(settings.to_args(&caps), vec!["--ctx-size", "8192"]);
    }

    #[test]
    fn launch_render_reports_retained_unsupported_settings() {
        let gpu_layers = GpuLayers::new(32);
        let settings = LaunchSettings {
            context_length: Some(ContextLength::new(8192).expect("valid context length")),
            gpu_layers: Some(gpu_layers),
            ..LaunchSettings::default()
        };
        let caps = EngineCapabilities {
            context_length: true,
            ..EngineCapabilities::default()
        };

        let rendered = settings.render(&caps);

        assert_eq!(settings.gpu_layers, Some(gpu_layers));
        assert_eq!(rendered.args, vec!["--ctx-size", "8192"]);
        assert_eq!(rendered.unsupported, vec![SettingId::GpuLayers]);
    }

    #[test]
    fn disabled_flash_attention_does_not_require_capability() {
        let settings = LaunchSettings {
            flash_attention: Some(false),
            ..LaunchSettings::default()
        };

        let rendered = settings.render(&EngineCapabilities::default());

        assert!(rendered.args.is_empty());
        assert!(rendered.unsupported.is_empty());
    }

    #[test]
    fn enabled_flash_attention_uses_the_typed_llama_cpp_value() {
        let settings = LaunchSettings {
            flash_attention: Some(true),
            ..LaunchSettings::default()
        };
        let capabilities = EngineCapabilities {
            flash_attention: true,
            ..EngineCapabilities::default()
        };

        assert_eq!(settings.to_args(&capabilities), ["--flash-attn", "on"]);
    }

    #[test]
    fn speculative_decoding_uses_supported_llama_cpp_types() {
        let settings = LaunchSettings {
            speculative_type: Some(SpeculativeType::DraftMtp),
            ..LaunchSettings::default()
        };
        let capabilities = EngineCapabilities {
            speculative_type: true,
            ..EngineCapabilities::default()
        };

        assert_eq!(
            settings.to_args(&capabilities),
            ["--spec-type", "draft-mtp"]
        );
    }

    #[test]
    fn kv_cache_type_applies_to_keys_and_values() {
        let settings = LaunchSettings {
            kv_cache_type: Some(KvCacheType::Q8_0),
            ..LaunchSettings::default()
        };
        let capabilities = EngineCapabilities {
            kv_cache_type: true,
            ..EngineCapabilities::default()
        };

        assert_eq!(
            settings.to_args(&capabilities),
            ["--cache-type-k", "q8_0", "--cache-type-v", "q8_0"]
        );
    }

    #[test]
    fn context_estimate_uses_gqa_dimensions_and_quant_block_overhead() {
        let metadata = CatalogMetadata {
            context_length: Some(131_072),
            block_count: Some(32),
            embedding_length: Some(4096),
            attention_head_count: Some(32),
            attention_head_count_kv: Some(8),
            ..CatalogMetadata::default()
        };
        let gib = 1024 * 1024 * 1024;

        let f16 = estimate_context(
            &metadata,
            4 * gib,
            &LaunchSettings::default(),
            16 * gib,
            12 * gib,
        );
        let q8 = estimate_context(
            &metadata,
            4 * gib,
            &LaunchSettings {
                kv_cache_type: Some(KvCacheType::Q8_0),
                ..LaunchSettings::default()
            },
            16 * gib,
            12 * gib,
        );
        let q4 = estimate_context(
            &metadata,
            4 * gib,
            &LaunchSettings {
                kv_cache_type: Some(KvCacheType::Q4_0),
                ..LaunchSettings::default()
            },
            16 * gib,
            12 * gib,
        );

        assert_eq!(f16.kv_bytes_per_token, Some(131_072));
        assert_eq!(q8.kv_bytes_per_token, Some(69_632));
        assert_eq!(q4.kv_bytes_per_token, Some(36_864));
        assert!(f16.vram_context_limit < q8.vram_context_limit);
        assert!(q8.vram_context_limit < q4.vram_context_limit);
        assert_eq!(q4.model_context_limit, 131_072);
    }

    #[test]
    fn hybrid_attention_interval_counts_only_kv_layers() {
        let metadata = CatalogMetadata {
            context_length: Some(262_144),
            block_count: Some(64),
            embedding_length: Some(5120),
            attention_head_count: Some(40),
            attention_head_count_kv: Some(8),
            attention_key_length: Some(256),
            attention_value_length: Some(256),
            full_attention_interval: Some(4),
            ..CatalogMetadata::default()
        };
        let estimate = estimate_context(
            &metadata,
            0,
            &LaunchSettings::default(),
            32 * 1024 * 1024 * 1024,
            30 * 1024 * 1024 * 1024,
        );

        assert_eq!(estimate.kv_bytes_per_token, Some(131_072));
    }
}
