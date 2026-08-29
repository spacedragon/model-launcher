use async_trait::async_trait;
use model_launcher_wsl::{
    CommandOutput, CommandRunner, WslError, WslProber, endpoint_owner_argv, launch_argv,
    ss_output_owns_pid,
};
use std::sync::{Arc, Mutex};

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
    let public_gateway_port = 1234_u16;
    let argv = launch_argv(
        "Ubuntu",
        "/llama",
        "/model",
        &["--port".into(), "45001".into()],
    );
    assert_eq!(public_gateway_port, 1234);
    assert!(
        !argv
            .iter()
            .any(|value| value == &public_gateway_port.to_string())
    );
}
