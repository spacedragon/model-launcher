use serde::{Deserialize, Serialize};

use crate::AppError;

macro_rules! positive_setting {
    ($name:ident, $code:literal) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

impl LaunchSettings {
    #[must_use]
    pub fn to_args(&self, capabilities: &EngineCapabilities) -> Vec<String> {
        let mut args = Vec::new();
        push_numeric(
            &mut args,
            capabilities.context_length,
            "--ctx-size",
            self.context_length.map(ContextLength::get),
        );
        push_numeric(
            &mut args,
            capabilities.gpu_layers,
            "--gpu-layers",
            self.gpu_layers.map(GpuLayers::get),
        );
        push_numeric(
            &mut args,
            capabilities.cpu_threads,
            "--threads",
            self.cpu_threads.map(CpuThreads::get),
        );
        push_numeric(
            &mut args,
            capabilities.batch_size,
            "--batch-size",
            self.batch_size.map(BatchSize::get),
        );
        push_numeric(
            &mut args,
            capabilities.parallel_slots,
            "--parallel",
            self.parallel_slots.map(ParallelSlots::get),
        );
        if capabilities.flash_attention && self.flash_attention == Some(true) {
            args.push("--flash-attn".into());
        }
        if capabilities.kv_cache_type
            && let Some(value) = self.kv_cache_type
        {
            args.extend(["--cache-type-k".into(), value.as_arg().into()]);
        }
        args
    }
}

fn push_numeric(args: &mut Vec<String>, supported: bool, flag: &str, value: Option<u32>) {
    if supported && let Some(value) = value {
        args.extend([flag.into(), value.to_string()]);
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
}
