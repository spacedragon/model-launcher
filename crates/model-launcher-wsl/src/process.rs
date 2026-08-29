use std::{
    io,
    net::{SocketAddr, TcpListener},
};
use thiserror::Error;

pub const LAUNCH_SCRIPT: &str = "printf 'MODEL_LAUNCHER_PID=%s\\n' \"$$\"; exec \"$@\"";
pub const LAUNCH_SENTINEL: &str = "model-launcher";

pub fn launch_argv(
    distribution: &str,
    executable: &str,
    model: &str,
    settings: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = [
        "-d",
        distribution,
        "--",
        "sh",
        "-c",
        LAUNCH_SCRIPT,
        LAUNCH_SENTINEL,
        executable,
        model,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    argv.extend_from_slice(settings);
    argv
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}
pub fn signal_argv(distribution: &str, pid: u32, signal: Signal) -> Vec<String> {
    let signal = match signal {
        Signal::Term => "-TERM",
        Signal::Kill => "-KILL",
    };
    [
        "-d",
        distribution,
        "--",
        "kill",
        signal,
        "--",
        &pid.to_string(),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PidError {
    #[error("invalid PID control line")]
    Invalid,
}
pub fn parse_pid_control_line(line: &str) -> Result<u32, PidError> {
    let raw = line
        .strip_suffix('\n')
        .and_then(|line| line.strip_prefix("MODEL_LAUNCHER_PID="))
        .ok_or(PidError::Invalid)?;
    if raw.is_empty() || raw.starts_with('0') || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(PidError::Invalid);
    }
    raw.parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or(PidError::Invalid)
}

#[derive(Default)]
pub struct InternalPortAllocator;
pub struct PortReservation {
    listener: Option<TcpListener>,
    addr: SocketAddr,
}
impl InternalPortAllocator {
    pub fn reserve(&self) -> io::Result<PortReservation> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        Ok(PortReservation {
            listener: Some(listener),
            addr,
        })
    }
}
impl PortReservation {
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }
    pub fn release(mut self) -> SocketAddr {
        drop(self.listener.take());
        self.addr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_uses_fixed_script_and_positional_arguments() {
        let argv = launch_argv(
            "Ubuntu",
            "/opt/llama server",
            "/mnt/c/a;echo bad.gguf",
            &["--ctx-size".into(), "4096".into()],
        );
        assert_eq!(
            &argv[..7],
            &[
                "-d",
                "Ubuntu",
                "--",
                "sh",
                "-c",
                LAUNCH_SCRIPT,
                LAUNCH_SENTINEL
            ]
        );
        assert_eq!(
            &argv[7..],
            &[
                "/opt/llama server",
                "/mnt/c/a;echo bad.gguf",
                "--ctx-size",
                "4096"
            ]
        );
        assert_eq!(
            LAUNCH_SCRIPT,
            "printf 'MODEL_LAUNCHER_PID=%s\\n' \"$$\"; exec \"$@\""
        );
    }

    #[test]
    fn parses_only_exact_control_line_and_structured_stop() {
        assert_eq!(
            parse_pid_control_line("MODEL_LAUNCHER_PID=42\n").unwrap(),
            42
        );
        for bad in [
            "MODEL_LAUNCHER_PID=0\n",
            " MODEL_LAUNCHER_PID=42\n",
            "MODEL_LAUNCHER_PID=42 x\n",
            "42\n",
        ] {
            assert!(parse_pid_control_line(bad).is_err());
        }
        assert_eq!(
            signal_argv("Ubuntu", 42, Signal::Term),
            vec!["-d", "Ubuntu", "--", "kill", "-TERM", "--", "42"]
        );
        assert_eq!(
            signal_argv("Ubuntu", 42, Signal::Kill),
            vec!["-d", "Ubuntu", "--", "kill", "-KILL", "--", "42"]
        );
    }

    #[test]
    fn internal_port_allocator_reserves_loopback_ephemeral_port() {
        let reservation = InternalPortAllocator.reserve().unwrap();
        assert_eq!(reservation.addr().ip().to_string(), "127.0.0.1");
        assert_ne!(reservation.addr().port(), 0);
    }
}
