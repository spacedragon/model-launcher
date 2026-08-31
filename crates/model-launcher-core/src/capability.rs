use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::AppError;

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
                args.extend(["--cache-type-k".into(), value.as_arg().into()]);
            } else {
                unsupported.push(SettingId::KvCacheType);
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
}
