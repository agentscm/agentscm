use std::io::ErrorKind;
use std::process::Command;

#[test]
fn native_feedback_uses_the_loopback_collector() {
    let home = tempfile::tempdir().unwrap();
    let python = || {
        let mut command = Command::new("python3");
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home.path())
            .env("TMPDIR", home.path())
            .current_dir(home.path())
            .args(["-I", "-B", "-c"]);
        command
    };
    let probe = match python()
        .arg("import sys; print(sys.version_info >= (3, 10))")
        .output()
    {
        Ok(probe) => probe,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            eprintln!("skipping native feedback smoke test: python3 is unavailable");
            return;
        }
        Err(error) => panic!("could not probe python3: {error}"),
    };
    assert!(
        probe.status.success(),
        "python3 probe failed: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    match probe.stdout.as_slice() {
        b"True\n" => (),
        b"False\n" => {
            eprintln!("skipping native feedback smoke test: Python 3.10 or newer is required");
            return;
        }
        other => panic!("unexpected python3 version probe output: {other:?}"),
    }

    let binary = std::env::var_os("NEXTEST_BIN_EXE_agentscm")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_agentscm").into());
    let result = python()
        .env("AGENTSCM_FEEDBACK_BIN", binary)
        .arg(include_str!("../../src/client_feedback/test_feedback.py"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "native feedback smoke test failed ({}):\n{}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
