use async_trait::async_trait;
use model_launcher_wsl::{
    CommandOutput, CommandRunner, PortLease, WslError, WslProber, endpoint_owner_argv, launch_argv,
    spawn_after_release, ss_output_owns_pid,
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct FakeRunner {
    calls: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl CommandRunner for FakeRunner {
    async fn output(&self, program: &str, argv: &[String]) -> Result<CommandOutput, WslError> {
        assert_eq!(program, "wsl.exe");
        self.calls.lock().expect("calls").push(argv.to_vec());
        let stdout = if argv.contains(&"stat".to_owned()) {
            "7\t8\t9\t10\n"
        } else if argv.last().is_some_and(|arg| arg == "--version") {
            "llama v1\n"
        } else {
            "--ctx-size --threads\n"
        };
        Ok(CommandOutput {
            success: true,
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }
}

#[tokio::test]
async fn fake_runner_probes_without_wsl_and_keeps_values_as_argv() {
    let runner = Arc::new(FakeRunner::default());
    WslProber::new(runner.clone())
        .probe("Ubuntu Space", "/opt/llama server", None)
        .await
        .expect("probe");
    let calls = runner.calls.lock().expect("calls");
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|argv| argv[1] == "Ubuntu Space"));
    assert!(calls.iter().all(|argv| {
        argv.last()
            .is_some_and(|arg| arg == "/opt/llama server" || arg.starts_with("--"))
    }));
}

#[test]
fn ownership_probe_is_structured_and_pid_match_is_exact() {
    assert_eq!(
        endpoint_owner_argv("Ubuntu Space", 3210),
        [
            "-d",
            "Ubuntu Space",
            "--",
            "ss",
            "-H",
            "-ltnp",
            "sport",
            "=",
            ":3210"
        ]
    );
    assert!(ss_output_owns_pid("users:((\"llama\",pid=42,fd=3))", 42));
    assert!(!ss_output_owns_pid("users:((\"other\",pid=142,fd=3))", 42));
}

#[test]
fn internal_retry_arguments_do_not_contain_or_mutate_a_public_gateway_port() {
    struct StableGateway {
        identity: u64,
        public_port: u16,
        served: usize,
    }
    impl StableGateway {
        fn request(&mut self) -> (u64, u16) {
            self.served += 1;
            (self.identity, self.public_port)
        }
    }
    let mut gateway = StableGateway {
        identity: 9,
        public_port: 1234,
        served: 0,
    };
    let before = gateway.request();
    let attempts: Vec<_> = [45001_u16, 45002, 45003]
        .into_iter()
        .map(|port| {
            launch_argv(
                "Ubuntu",
                "/llama",
                "/model",
                &["--port".into(), port.to_string()],
            )
        })
        .collect();
    let after = gateway.request();
    assert_eq!(before, after);
    assert_eq!(gateway.served, 2);
    assert!(attempts.windows(2).all(|pair| pair[0] != pair[1]));
    assert!(
        attempts
            .iter()
            .flatten()
            .all(|value| value != &gateway.public_port.to_string())
    );
}

struct RecordingLease(Arc<Mutex<Vec<&'static str>>>);
impl PortLease for RecordingLease {
    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 45001))
    }
    fn release(self: Box<Self>) -> SocketAddr {
        self.0.lock().unwrap().push("release");
        self.addr()
    }
}
struct SpawnFailRunner(Arc<Mutex<Vec<&'static str>>>);
#[async_trait]
impl CommandRunner for SpawnFailRunner {
    async fn output(&self, _: &str, _: &[String]) -> Result<CommandOutput, WslError> {
        panic!("generic spawn failure must not guess a PID to signal")
    }
    async fn spawn(
        &self,
        _: &str,
        _: &[String],
    ) -> Result<Box<dyn model_launcher_wsl::WslChild>, WslError> {
        self.0.lock().unwrap().push("spawn");
        Err(WslError::Command("spawn failed".into()))
    }
}
#[tokio::test]
async fn reservation_is_released_strictly_before_generic_spawn_failure() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let result = spawn_after_release(
        &SpawnFailRunner(events.clone()),
        Box::new(RecordingLease(events.clone())),
        &["argv".into()],
    )
    .await;
    assert!(
        result.is_err(),
        "generic spawn errors fail immediately rather than retrying an unowned attempt"
    );
    assert_eq!(*events.lock().unwrap(), ["release", "spawn"]);
}
