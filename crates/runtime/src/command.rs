//! Controlled process launch specification (`docs/architecture.md` §4).
//!
//! [`CommandSpec`] is the *only* way a runtime adapter describes how to start
//! an engine process: an explicit `executable` path plus an ordered `argv`,
//! plus an explicitly ordered environment. There is deliberately **no shell
//! string** anywhere in this type, so a stored runtime can never smuggle in
//! shell metacharacters, pipes, redirections or command chaining.
//!
//! Determinism rules:
//!
//! - `args` is a `Vec<OsString>` in insertion order; duplicate and
//!   non-UTF-8 arguments are preserved verbatim.
//! - `env` is a `Vec<(OsString, OsString)>` in insertion order (not a hash
//!   map), so snapshots and tests are byte-stable.
//! - `clear_env` decides whether the child inherits the parent environment
//!   first (`false`, the default) or starts from an empty environment
//!   (`true`). When clearing, only the entries in `env` are present.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// An argv-style description of an engine process to launch.
///
/// Build one with [`CommandSpec::new`] and the chainable `with_*` helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    executable: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    clear_env: bool,
}

impl CommandSpec {
    /// Start a spec for `executable`. The path is stored verbatim and this
    /// container does **not** itself enforce or resolve anything: because
    /// [`to_std_command`](Self::to_std_command) delegates to
    /// [`std::process::Command`], a bare name would be resolved through
    /// `PATH` by the OS. Callers (the runtime probe and the engine adapters)
    /// are responsible for validating that the path is non-empty and absolute
    /// before building a spec.
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args: Vec::new(),
            env: Vec::new(),
            clear_env: false,
        }
    }

    /// Append one argument.
    #[must_use]
    pub fn with_arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append several arguments in order.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Append one environment entry (order preserved).
    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Set whether the child environment is cleared before `env` is applied.
    #[must_use]
    pub fn with_clear_env(mut self, clear_env: bool) -> Self {
        self.clear_env = clear_env;
        self
    }

    /// The executable path.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// The ordered argument vector.
    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// The ordered environment entries.
    #[must_use]
    pub fn env(&self) -> &[(OsString, OsString)] {
        &self.env
    }

    /// Whether the child environment is cleared before `env` is applied.
    #[must_use]
    pub fn clears_env(&self) -> bool {
        self.clear_env
    }

    /// Materialize a [`std::process::Command`] with the executable, args and
    /// environment applied. The caller owns stdin/stdout/stderr and any
    /// platform extension (process group, job object).
    ///
    /// No shell is ever involved: the executable is spawned directly and the
    /// argument vector is passed as-is.
    #[must_use]
    pub fn to_std_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.executable);
        command.args(&self.args);
        if self.clear_env {
            command.env_clear();
        }
        command.envs(self.env.iter().map(|(key, value)| (key, value)));
        command
    }
}

#[cfg(test)]
mod tests {
    use super::CommandSpec;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn args_and_env_are_deterministic_and_ordered() {
        let spec = CommandSpec::new("/opt/llama/llama-server")
            .with_arg("--model")
            .with_arg("/models/a.gguf")
            .with_args(["--port", "8080"])
            .with_env("A", "1")
            .with_env("B", "2");

        assert_eq!(spec.executable(), Path::new("/opt/llama/llama-server"));
        assert_eq!(
            spec.args(),
            &[
                OsString::from("--model"),
                OsString::from("/models/a.gguf"),
                OsString::from("--port"),
                OsString::from("8080"),
            ]
        );
        assert_eq!(
            spec.env(),
            &[
                (OsString::from("A"), OsString::from("1")),
                (OsString::from("B"), OsString::from("2")),
            ]
        );
        assert!(!spec.clears_env());
    }

    #[test]
    fn clear_env_flag_is_recorded_on_the_command() {
        let spec = CommandSpec::new("/usr/bin/ninfer-serve")
            .with_clear_env(true)
            .with_env("PATH", "/usr/bin");

        assert!(spec.clears_env());
        let command = spec.to_std_command();
        // `Command` has no public env accessor; asserting `get_envs` proves
        // that the single entry survived the round-trip.
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].0, std::ffi::OsStr::new("PATH"));
    }

    #[test]
    fn shell_metacharacters_stay_inside_a_single_argument() {
        let spec = CommandSpec::new("/bin/true")
            .with_arg("; rm -rf /")
            .with_arg("a && b | c");

        // The argument vector is never re-split: two args, not five.
        assert_eq!(spec.args().len(), 2);
        assert_eq!(spec.args()[0], OsString::from("; rm -rf /"));
    }
}
