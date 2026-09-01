use model_launcher_core::EngineCapabilities;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableIdentity {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_seconds: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSnapshot {
    pub distribution: String,
    pub executable_path: String,
    pub executable_identity: ExecutableIdentity,
    pub version_raw: String,
    pub help_raw: String,
    pub capabilities: EngineCapabilities,
    pub probed_at: u64,
}

impl ProbeSnapshot {
    pub fn new(
        distribution: impl Into<String>,
        executable_path: impl Into<String>,
        executable_identity: ExecutableIdentity,
        version_raw: impl Into<String>,
        help_raw: impl Into<String>,
    ) -> Self {
        let help_raw = help_raw.into();
        Self {
            distribution: distribution.into(),
            executable_path: executable_path.into(),
            executable_identity,
            version_raw: version_raw.into(),
            capabilities: capabilities_from_help(&help_raw),
            help_raw,
            probed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
    #[must_use]
    pub fn is_valid_for(
        &self,
        distribution: &str,
        executable_path: &str,
        identity: &ExecutableIdentity,
    ) -> bool {
        self.distribution == distribution
            && self.executable_path == executable_path
            && &self.executable_identity == identity
    }
    pub fn load(path: &Path) -> Result<Self, ProbeError> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
    pub fn save(&self, path: &Path) -> Result<(), ProbeError> {
        fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("invalid stat output")]
    InvalidIdentity,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub fn probe_argv(distribution: &str, executable: &str, option: &str) -> Vec<String> {
    ["-d", distribution, "--", executable, option]
        .into_iter()
        .map(str::to_owned)
        .collect()
}
pub fn stat_argv(distribution: &str, executable: &str) -> Vec<String> {
    [
        "-d",
        distribution,
        "--",
        "stat",
        "-Lc",
        "%d\t%i\t%s\t%Y",
        "--",
        executable,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
pub fn capture_version(raw: &str) -> String {
    raw.trim_end_matches(['\r', '\n']).to_owned()
}
pub fn parse_identity(raw: &str) -> Result<ExecutableIdentity, ProbeError> {
    let mut fields = raw.trim().split('\t');
    let result = ExecutableIdentity {
        device: fields
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(ProbeError::InvalidIdentity)?,
        inode: fields
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(ProbeError::InvalidIdentity)?,
        size: fields
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(ProbeError::InvalidIdentity)?,
        modified_seconds: fields
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(ProbeError::InvalidIdentity)?,
    };
    if fields.next().is_some() {
        return Err(ProbeError::InvalidIdentity);
    }
    Ok(result)
}
pub fn capabilities_from_help(help: &str) -> EngineCapabilities {
    let has = |aliases: &[&str]| {
        aliases.iter().any(|alias| {
            help.split_ascii_whitespace().any(|token| {
                token.trim_matches(|c: char| matches!(c, ',' | '[' | ']' | '=')) == *alias
            })
        })
    };
    EngineCapabilities {
        context_length: has(&["--ctx-size", "-c"]),
        gpu_layers: has(&["--gpu-layers", "--n-gpu-layers", "-ngl"]),
        cpu_threads: has(&["--threads", "-t"]),
        batch_size: has(&["--batch-size", "-b"]),
        parallel_slots: has(&["--parallel", "-np"]),
        flash_attention: has(&["--flash-attn", "--flash-attention"]),
        kv_cache_type: has(&["--cache-type-k"]) && has(&["--cache-type-v"]),
        speculative_type: has(&["--spec-type"]),
        draft_model: has(&["--spec-draft-model", "--model-draft", "-md"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_use_direct_argv_and_capture_version_and_known_help_aliases() {
        assert_eq!(
            probe_argv("Ubuntu 24.04", "/opt/llama server", "--version"),
            vec!["-d", "Ubuntu 24.04", "--", "/opt/llama server", "--version"]
        );
        let caps = capabilities_from_help(
            "--ctx-size --n-gpu-layers --threads --batch-size --parallel --flash-attn --cache-type-k --cache-type-v --spec-type --spec-draft-model",
        );
        assert!(
            caps.context_length
                && caps.gpu_layers
                && caps.cpu_threads
                && caps.batch_size
                && caps.parallel_slots
                && caps.flash_attention
                && caps.kv_cache_type
                && caps.speculative_type
                && caps.draft_model
        );
        assert!(capabilities_from_help("--model-draft").draft_model);
        assert!(!capabilities_from_help("--unknown").context_length);
        assert_eq!(
            capture_version("llama-server version 4123\n"),
            "llama-server version 4123"
        );
    }

    #[test]
    fn stat_command_is_structured_and_snapshot_round_trips() {
        assert_eq!(
            stat_argv("Distro", "/opt/llama"),
            vec![
                "-d",
                "Distro",
                "--",
                "stat",
                "-Lc",
                "%d\t%i\t%s\t%Y",
                "--",
                "/opt/llama"
            ]
        );
        let identity = parse_identity("12\t34\t56\t78\n").unwrap();
        assert_eq!(
            identity,
            ExecutableIdentity {
                device: 12,
                inode: 34,
                size: 56,
                modified_seconds: 78
            }
        );
        let snapshot = ProbeSnapshot::new("Distro", "/opt/llama", identity, "v1\n", "--ctx-size\n");
        assert_eq!(
            serde_json::from_str::<ProbeSnapshot>(&serde_json::to_string(&snapshot).unwrap())
                .unwrap(),
            snapshot
        );
    }

    #[test]
    fn cache_requires_distribution_path_and_identity_match() {
        let id = ExecutableIdentity {
            device: 1,
            inode: 2,
            size: 3,
            modified_seconds: 4,
        };
        let snapshot = ProbeSnapshot::new("Ubuntu", "/bin/llama", id.clone(), "v1", "--ctx-size");
        assert!(snapshot.is_valid_for("Ubuntu", "/bin/llama", &id));
        assert!(!snapshot.is_valid_for("ubuntu", "/bin/llama", &id));
        let mut changed = id.clone();
        changed.size += 1;
        assert!(!snapshot.is_valid_for("Ubuntu", "/bin/llama", &changed));
        for changed in [
            ExecutableIdentity {
                device: 9,
                ..id.clone()
            },
            ExecutableIdentity {
                inode: 9,
                ..id.clone()
            },
            ExecutableIdentity {
                size: 9,
                ..id.clone()
            },
            ExecutableIdentity {
                modified_seconds: 9,
                ..id.clone()
            },
        ] {
            assert!(!snapshot.is_valid_for("Ubuntu", "/bin/llama", &changed));
        }
    }
}
