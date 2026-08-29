use model_launcher_core::{
    AppError, EngineLogFramer, LogLevel, LogSource, LogStore, MAX_ENGINE_LOG_LINE_BYTES, ModelId,
};
use std::{
    io,
    net::{SocketAddr, TcpListener},
};
use thiserror::Error;

pub const LAUNCH_SCRIPT: &str = "printf 'MODEL_LAUNCHER_PID=%s\\n' \"$$\"; exec \"$@\"";
pub const LAUNCH_SENTINEL: &str = "model-launcher";
pub const SIGNAL_SENTINEL: &str = "model-launcher-signal";
pub const GUARDED_SIGNAL_SCRIPT: &str = "signal=$1; pid=$2; expected=$3; stat=$(cat \"/proc/$pid/stat\" 2>/dev/null) || { printf 'AlreadyExited\\n'; exit 0; }; rest=${stat##*) }; set -- $rest; [ \"$20\" = \"$expected\" ] || { printf 'IdentityMismatch\\n'; exit 0; }; kill \"$signal\" -- \"$pid\" && printf 'Signaled\\n'";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedPid {
    pub pid: u32,
    pub start_time: u64,
}
pub fn parse_proc_start_time(stat: &str) -> Result<u64, PidError> {
    let close = stat.rfind(')').ok_or(PidError::Invalid)?;
    stat[close + 1..]
        .split_ascii_whitespace()
        .nth(19)
        .and_then(|value| value.parse().ok())
        .ok_or(PidError::Invalid)
}
pub fn proc_stat_argv(distribution: &str, pid: u32) -> Vec<String> {
    [
        "-d",
        distribution,
        "--",
        "cat",
        &format!("/proc/{pid}/stat"),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
pub fn guarded_signal_argv(distribution: &str, owned: OwnedPid, signal: Signal) -> Vec<String> {
    let signal = match signal {
        Signal::Term => "-TERM",
        Signal::Kill => "-KILL",
    };
    [
        "-d",
        distribution,
        "--",
        "sh",
        "-c",
        GUARDED_SIGNAL_SCRIPT,
        SIGNAL_SENTINEL,
        signal,
        &owned.pid.to_string(),
        &owned.start_time.to_string(),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

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
        "--model",
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
pub trait PortLease: Send {
    fn addr(&self) -> SocketAddr;
    fn release(self: Box<Self>) -> SocketAddr;
}
impl PortLease for PortReservation {
    fn addr(&self) -> SocketAddr {
        self.addr()
    }
    fn release(self: Box<Self>) -> SocketAddr {
        (*self).release()
    }
}
pub trait PortAllocator: Send + Sync {
    fn reserve(&self) -> io::Result<Box<dyn PortLease>>;
}
impl PortAllocator for InternalPortAllocator {
    fn reserve(&self) -> io::Result<Box<dyn PortLease>> {
        InternalPortAllocator::reserve(self).map(|lease| Box::new(lease) as Box<dyn PortLease>)
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

pub struct EngineStreamCapture {
    stdout: EngineLogFramer,
    stderr: EngineLogFramer,
    control: Vec<u8>,
    pid_seen: bool,
}

impl EngineStreamCapture {
    pub fn new(
        store: LogStore,
        generation: Option<u64>,
        model_id: Option<ModelId>,
    ) -> Result<Self, AppError> {
        Ok(Self {
            stdout: EngineLogFramer::new(
                store.clone(),
                LogSource::EngineStdout,
                LogLevel::Info,
                generation,
                model_id,
                MAX_ENGINE_LOG_LINE_BYTES,
            )?,
            stderr: EngineLogFramer::new(
                store,
                LogSource::EngineStderr,
                LogLevel::Error,
                generation,
                model_id,
                MAX_ENGINE_LOG_LINE_BYTES,
            )?,
            control: Vec::new(),
            pid_seen: false,
        })
    }
    pub fn push_stdout(
        &mut self,
        timestamp_ms: u64,
        bytes: &[u8],
    ) -> Result<Option<u32>, PidError> {
        if self.pid_seen {
            self.stdout.push(timestamp_ms, bytes);
            return Ok(None);
        }
        self.control.extend_from_slice(bytes);
        let Some(newline) = self.control.iter().position(|byte| *byte == b'\n') else {
            return Ok(None);
        };
        let remainder = self.control.split_off(newline + 1);
        let line = std::str::from_utf8(&self.control).map_err(|_| PidError::Invalid)?;
        let pid = parse_pid_control_line(line)?;
        self.control.clear();
        self.pid_seen = true;
        self.stdout.push(timestamp_ms, &remainder);
        Ok(Some(pid))
    }
    pub fn push_stderr(&mut self, timestamp_ms: u64, bytes: &[u8]) {
        self.stderr.push(timestamp_ms, bytes);
    }
    pub fn finish(&mut self, timestamp_ms: u64) {
        self.stdout.finish(timestamp_ms);
        self.stderr.finish(timestamp_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model_launcher_core::{LogLevel, LogSource, LogStore, LogStoreLimits, ModelId};

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
                "--model",
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
            "MODEL_LAUNCHER_PID=01\n",
            "MODEL_LAUNCHER_PID=+42\n",
            "MODEL_LAUNCHER_PID=4294967296\n",
            "MODEL_LAUNCHER_PID=42\nextra",
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
    fn owned_pid_identity_uses_structured_proc_stat_and_guarded_signal_script() {
        let stat = "42 (llama server) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 98765 20";
        let owned = OwnedPid {
            pid: 42,
            start_time: parse_proc_start_time(stat).unwrap(),
        };
        assert_eq!(owned.start_time, 98765);
        assert_eq!(
            proc_stat_argv("Ubuntu", 42),
            ["-d", "Ubuntu", "--", "cat", "/proc/42/stat"]
        );
        let argv = guarded_signal_argv("Ubuntu", owned, Signal::Kill);
        assert_eq!(
            &argv[..7],
            &[
                "-d",
                "Ubuntu",
                "--",
                "sh",
                "-c",
                GUARDED_SIGNAL_SCRIPT,
                SIGNAL_SENTINEL
            ]
        );
        assert_eq!(&argv[7..], &["-KILL", "42", "98765"]);
    }

    #[test]
    fn internal_port_allocator_reserves_loopback_ephemeral_port() {
        let reservation = match InternalPortAllocator.reserve() {
            Ok(reservation) => reservation,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("unexpected bind error: {error}"),
        };
        assert_eq!(reservation.addr().ip().to_string(), "127.0.0.1");
        assert_ne!(reservation.addr().port(), 0);
    }

    #[test]
    fn pid_control_is_separate_and_following_arbitrary_chunks_are_structured_logs() {
        let store = LogStore::new(LogStoreLimits::new(16, 4096, 4)).unwrap();
        let model_id = ModelId::new();
        let mut capture = EngineStreamCapture::new(store.clone(), Some(7), Some(model_id)).unwrap();
        assert_eq!(
            capture
                .push_stdout(10, b"MODEL_LAUNCHER_PID=42\nfirst ")
                .unwrap(),
            Some(42)
        );
        assert_eq!(capture.push_stdout(11, b"line\npartial").unwrap(), None);
        capture.push_stderr(12, b"Authorization: Bearer secret\nwarn");
        capture.finish(13);
        let records = store.snapshot();
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].message, "first line");
        assert_eq!(records[0].source, LogSource::EngineStdout);
        assert_eq!(records[0].level, LogLevel::Info);
        assert_eq!(records[0].generation, Some(7));
        assert_eq!(records[0].model_id, Some(model_id));
        let partial = records
            .iter()
            .find(|record| record.message == "partial")
            .unwrap();
        assert_eq!(partial.source, LogSource::EngineStdout);
        let authorization = records
            .iter()
            .find(|record| record.message.starts_with("Authorization:"))
            .unwrap();
        assert_eq!(authorization.message, "Authorization: [REDACTED]");
        assert_eq!(authorization.source, LogSource::EngineStderr);
        assert_eq!(authorization.level, LogLevel::Error);
        assert!(
            records
                .iter()
                .all(|record| !record.message.contains("MODEL_LAUNCHER_PID"))
        );
    }
}
