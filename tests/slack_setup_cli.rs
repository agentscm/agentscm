#![cfg(unix)]
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::{Value, json};

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(args)
        .env("AGENTSCM_TELEMETRY", "off")
        .env("AGENTSCM_TEST_PROVIDER_SECRET", "synthetic-parent-secret")
        .output()
        .unwrap()
}

fn private(path: &Path, value: &str) {
    std::fs::write(path, value).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn setup_derives_identity_is_idempotent_and_doctor_does_not_create_a_journal() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    private(&root.join("agent"), &"a".repeat(64));
    private(&root.join("signing"), "synthetic-signing-secret");
    private(&root.join("bot"), "xoxb-synthetic-only");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let who = json!({"token":{"repository":"acme/widgets","repository_id":101,"read_only":true,"expires_at":i64::MAX},"owner":{"provider":"github","host":"github.com","user_id":42}});
        for read_only in [true, true, false] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = [0u8; 4096];
            let n = stream.read(&mut bytes).unwrap();
            assert!(
                std::str::from_utf8(&bytes[..n])
                    .unwrap()
                    .starts_with("GET /v1/hub/whoami ")
            );
            let mut response = who.clone();
            response["token"]["read_only"] = json!(read_only);
            let body = response.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
    });
    let destination = root.join("managed/slack");
    let agent = root.join("agent");
    let signing = root.join("signing");
    let bot = root.join("bot");
    let args = [
        "hub",
        "slack",
        "setup",
        "--directory",
        destination.to_str().unwrap(),
        "--hub-url",
        "https://hub.example.test",
        "--api-url",
        &origin,
        "--repo",
        "acme/widgets",
        "--app-id",
        "A123",
        "--team-id",
        "T123",
        "--bot-id",
        "B123",
        "--slack-user",
        "U123",
        "--agent-token-file",
        agent.to_str().unwrap(),
        "--signing-secret-file",
        signing.to_str().unwrap(),
        "--bot-token-file",
        bot.to_str().unwrap(),
        "--allow-coordination-writes",
    ];
    for _ in 0..2 {
        let output = cli(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["hub_identity_verified"], true);
        assert_eq!(report["live_verified"], false);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("xoxb-"));
    }
    let bytes = std::fs::read(destination.join("config.json")).unwrap();
    let config: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(config["bindings"][0]["repository_id"], 101);
    assert_eq!(config["bindings"][0]["forge_user_id"], 42);
    assert_eq!(config["bindings"][0]["allow_coordination_writes"], true);
    let denied = cli(&args);
    assert!(!denied.status.success());
    assert_eq!(
        std::fs::read(destination.join("config.json")).unwrap(),
        bytes
    );
    server.join().unwrap();
    let config_file = destination.join("config.json");
    let output = cli(&[
        "hub",
        "slack",
        "doctor",
        "--config-file",
        config_file.to_str().unwrap(),
    ]);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["configuration"]["local_valid"], true);
    assert_eq!(report["live_verified"], false);
    assert!(!destination.join("install.sqlite").exists());
}

#[test]
fn provider_install_captures_output_and_does_not_claim_verified_installation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let program = root.join("slack");
    // This fixture deliberately emits secret-looking output; it must not escape the wrapper.
    private(
        &program,
        r#"#!/bin/sh
printf 'xoxb-synthetic-provider-output\n'
printf 'xoxb-synthetic-provider-error\n' >&2
for arg in "$@"; do test "$arg" != --force || exit 2; done
test -z "${AGENTSCM_TEST_PROVIDER_SECRET+x}" || exit 4
test -f "$SLACK_CLI_APP_ICON_PATH" || exit 3
printf '   Updated app icon: %s\n' "$SLACK_CLI_APP_ICON_PATH"
umask 077
printf '%s\n' '{"apps":{"T123":{"app_id":"A123","team_id":"T123","team_domain":"synthetic"}},"default":"synthetic"}' > .slack/apps.json
exit 0
"#,
    );
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let project = root.join("project");
    let output = cli(&[
        "hub",
        "slack",
        "install",
        "--directory",
        project.to_str().unwrap(),
        "--team-id",
        "T123",
        "--slack-cli",
        program.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["cli_exit_success"], true);
    assert_eq!(report["local_app_recorded"], true);
    assert_eq!(report["installation_verified"], false);
    assert_eq!(report["live_verified"], false);
    assert_eq!(report["icon_upload_reported"], true);
    assert_eq!(report["app_record"]["app_id"], "A123");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("xoxb-"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("xoxb-"));
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(project.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest["settings"]["function_runtime"].is_null());
    assert_eq!(manifest["settings"]["org_deploy_enabled"], false);
    assert_eq!(
        manifest["oauth_config"]["scopes"]["bot"],
        json!(["commands"])
    );
    assert!(manifest["features"]["slash_commands"].is_null());
    assert!(!project.join("package.json").exists());

    // A cached app record and a new icon acknowledgement do not establish approval.
    private(
        &program,
        "#!/bin/sh\nprintf 'Updated app icon: %s/icon.png\\n' \"$PWD\"\nexit 0\n",
    );
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let cached = cli(&[
        "hub",
        "slack",
        "install",
        "--directory",
        project.to_str().unwrap(),
        "--team-id",
        "T123",
        "--slack-cli",
        program.to_str().unwrap(),
    ]);
    assert!(cached.status.success());
    let cached: Value = serde_json::from_slice(&cached.stdout).unwrap();
    assert_eq!(cached["app_record"]["app_id"], "A123");
    assert_eq!(cached["installation_verified"], false);

    // Exit zero and an old app receipt cannot conceal a new upload/approval failure.
    private(
        &program,
        "#!/bin/sh\nprintf 'Error updating app icon: xoxb-synthetic-error\\n'\nexit 0\n",
    );
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let denied = cli(&[
        "hub",
        "slack",
        "install",
        "--directory",
        project.to_str().unwrap(),
        "--team-id",
        "T123",
        "--slack-cli",
        program.to_str().unwrap(),
    ]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("did not confirm"));
    assert!(!String::from_utf8_lossy(&denied.stdout).contains("xoxb-"));
    assert!(!String::from_utf8_lossy(&denied.stderr).contains("xoxb-"));
}
