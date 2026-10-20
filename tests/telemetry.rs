use std::fs;
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tempfile::TempDir;

fn run(config: &TempDir, data: &TempDir, arguments: &[&str]) -> Output {
    command(config, data, arguments)
        .output()
        .expect("run agentscm")
}

fn command(config: &TempDir, data: &TempDir, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
    command
        .env("XDG_CONFIG_HOME", config.path())
        .env("HOME", config.path())
        .env_remove("CI")
        .env_remove("AGENTSCM_TELEMETRY")
        .env_remove("DO_NOT_TRACK")
        .env_remove("AGENTSCM_TELEMETRY_TEST")
        .env_remove("AGENTSCM_VERIFIED_INSTALL")
        .arg("--data-dir")
        .arg(data.path())
        .args(arguments);
    command
}

fn json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

fn state_path(config: &TempDir) -> std::path::PathBuf {
    config.path().join("agentscm/telemetry.json")
}

#[test]
fn endpoint_unset_leaves_no_state_or_config_directory() {
    let config = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    assert!(run(&config, &data, &["init"]).status.success());
    assert!(!config.path().join("agentscm").exists());
    assert!(!state_path(&config).exists());
    let status = json(&run(&config, &data, &["telemetry", "status"]));
    assert_eq!(status["effective"], "waiting");
    assert_eq!(status["reason"], "endpoint_unset");
    assert_eq!(status["pending"], 0);
    assert!(!config.path().join("agentscm").exists());
}

#[test]
fn consent_profile_needs_explicit_on_and_environment_still_wins() {
    let config = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let configured = json(&run(
        &config,
        &data,
        &[
            "telemetry",
            "configure",
            "--endpoint",
            "https://127.0.0.1:9/v1/telemetry/batches",
            "--profile",
            "consent-required",
        ],
    ));
    assert_eq!(configured["effective"], "waiting");
    assert_eq!(configured["reason"], "notice_required");

    let quickstart = run(
        &config,
        &data,
        &[
            "quickstart",
            "--yes",
            "--hub",
            "skip",
            "--hook",
            "none",
            "--no-collisions",
        ],
    );
    assert!(
        quickstart.status.success(),
        "{}",
        String::from_utf8_lossy(&quickstart.stderr)
    );
    let after_generic_yes = json(&run(&config, &data, &["telemetry", "status"]));
    assert_eq!(after_generic_yes["effective"], "waiting");
    assert_eq!(after_generic_yes["reason"], "notice_required");
    assert!(after_generic_yes["notice_seen_at"].is_null());
    assert!(after_generic_yes["consent_granted_at"].is_null());

    let enabled = json(&run(&config, &data, &["telemetry", "on"]));
    assert_eq!(enabled["effective"], "enabled");

    let overridden = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .env("XDG_CONFIG_HOME", config.path())
        .env("DO_NOT_TRACK", "1")
        .args(["telemetry", "status"])
        .output()
        .unwrap();
    let overridden = json(&overridden);
    assert_eq!(overridden["effective"], "disabled");
    assert_eq!(overridden["reason"], "environment_override");
}

#[test]
fn global_and_ci_optouts_keep_eligible_sender_queue_unchanged() {
    let config = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    json(&run(
        &config,
        &data,
        &[
            "telemetry",
            "configure",
            "--endpoint",
            "https://127.0.0.1:9/v1/telemetry/batches",
            "--profile",
            "permitted-default",
        ],
    ));
    json(&run(&config, &data, &["telemetry", "on"]));

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut state: Value = serde_json::from_slice(&fs::read(state_path(&config)).unwrap()).unwrap();
    state["installation_id"] = Value::String("0123456789abcdef0123456789abcdef".into());
    state["queue"] = serde_json::json!([{
        "event_id": "abcdef0123456789abcdef0123456789",
        "day": "2026-10-20",
        "version": "0.1.0",
        "os": "linux",
        "arch": "x86_64",
        "feature": "installation",
        "outcome": "succeeded",
        "count": 1,
        "source": "installation",
        "dedup_key": "Installation:Installation:once",
        "attempts": 0,
        "created_at": timestamp,
        "next_attempt_at": timestamp
    }]);
    fs::write(state_path(&config), serde_json::to_vec(&state).unwrap()).unwrap();
    let seeded = json(&run(&config, &data, &["telemetry", "inspect"]));
    assert_eq!(seeded["effective"], "enabled");
    assert_eq!(seeded["pending"], 1);
    assert_eq!(seeded["queued_events"][0]["attempts"], 0);
    let before = fs::read(state_path(&config)).unwrap();

    let output = run(&config, &data, &["--no-telemetry", "telemetry", "send"]);

    assert!(output.status.success());
    assert_eq!(fs::read(state_path(&config)).unwrap(), before);

    let disabled = command(&config, &data, &["telemetry", "status"])
        .env("CI", "1")
        .output()
        .unwrap();
    let disabled = json(&disabled);
    assert_eq!(disabled["effective"], "disabled");
    assert_eq!(disabled["reason"], "environment_override");
    let output = command(&config, &data, &["telemetry", "send"])
        .env("CI", "1")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read(state_path(&config)).unwrap(), before);
}

#[test]
fn invocation_optout_prevents_install_marker_and_sticky_off_deletes_state() {
    let config = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    json(&run(
        &config,
        &data,
        &[
            "telemetry",
            "configure",
            "--endpoint",
            "https://127.0.0.1:9/v1/telemetry/batches",
            "--profile",
            "permitted-default",
        ],
    ));
    json(&run(&config, &data, &["telemetry", "on"]));
    let ignored = command(&config, &data, &["--no-telemetry", "telemetry", "status"])
        .env("AGENTSCM_VERIFIED_INSTALL", "1")
        .output()
        .unwrap();
    assert!(ignored.status.success());
    let untouched = json(&run(&config, &data, &["telemetry", "inspect"]));
    assert!(untouched["installation_id"].is_null());
    assert_eq!(untouched["pending"], 0);

    let started = Instant::now();
    let observation = command(&config, &data, &["telemetry", "status"])
        .env("AGENTSCM_VERIFIED_INSTALL", "1")
        .output()
        .unwrap();
    assert!(observation.status.success());
    assert!(observation.stderr.is_empty());
    assert!(started.elapsed() < Duration::from_secs(2));
    let observed = json(&run(&config, &data, &["telemetry", "inspect"]));
    assert!(observed["installation_id"].is_string());

    let disabled = json(&run(&config, &data, &["telemetry", "off"]));
    assert_eq!(disabled["effective"], "disabled");
    let inspected = json(&run(&config, &data, &["telemetry", "inspect"]));
    assert!(inspected["installation_id"].is_null());
    assert!(inspected["hub_id"].is_null());
    assert_eq!(inspected["pending"], 0);
    assert_eq!(inspected["retained_dedup_keys"], 0);
}

#[test]
fn off_recovers_from_corrupt_state_without_emitting_an_event() {
    let config = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    fs::create_dir_all(config.path().join("agentscm")).unwrap();
    // The advisory lock file intentionally survives processes; its presence must never wedge off.
    fs::write(config.path().join("agentscm/telemetry.lock"), b"").unwrap();
    fs::write(state_path(&config), b"not json").unwrap();
    let disabled = json(&run(&config, &data, &["telemetry", "off"]));
    assert_eq!(disabled["requested"], "off");
    assert_eq!(disabled["pending"], 0);
    let state: Value = serde_json::from_slice(&fs::read(state_path(&config)).unwrap()).unwrap();
    assert!(state["queue"].as_array().unwrap().is_empty());
    assert!(state["installation_id"].is_null());
}

#[test]
fn endpoint_validation_rejects_redirectable_or_credentialed_shapes() {
    let config = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    for endpoint in [
        "http://example.test/v1/telemetry/batches",
        "https://user@example.test/v1/telemetry/batches",
        "https://example.test/v1/telemetry/batch",
        "https://example.test/v1/telemetry/batches?repo=x",
    ] {
        let output = run(
            &config,
            &data,
            &["telemetry", "configure", "--endpoint", endpoint],
        );
        assert!(!output.status.success(), "{endpoint}");
    }
}

#[test]
fn configure_alone_waits_for_notice_and_recipient_rotation_clears_collection() {
    let config = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let provisioned = json(&run(
        &config,
        &data,
        &[
            "telemetry",
            "configure",
            "--endpoint",
            "https://127.0.0.1:9/v1/telemetry/batches",
            "--profile",
            "permitted-default",
        ],
    ));
    assert_eq!(provisioned["effective"], "waiting");
    assert_eq!(provisioned["reason"], "notice_required");
    assert!(provisioned["notice_seen_at"].is_null());

    let enabled = json(&run(&config, &data, &["telemetry", "on"]));
    assert_eq!(enabled["effective"], "enabled");
    assert!(
        command(&config, &data, &["telemetry", "status"])
            .env("AGENTSCM_VERIFIED_INSTALL", "1")
            .status()
            .unwrap()
            .success()
    );
    let observed = json(&run(&config, &data, &["telemetry", "inspect"]));
    assert!(observed["installation_id"].is_string());

    let rotated = json(&run(
        &config,
        &data,
        &[
            "telemetry",
            "configure",
            "--endpoint",
            "https://127.0.0.1:10/v1/telemetry/batches",
        ],
    ));
    assert_eq!(rotated["effective"], "waiting");
    assert_eq!(rotated["reason"], "notice_required");
    assert_eq!(rotated["requested"], "unset");
    assert!(rotated["notice_seen_at"].is_null());
    assert!(rotated["consent_granted_at"].is_null());
    let rotated = json(&run(&config, &data, &["telemetry", "inspect"]));
    assert!(rotated["installation_id"].is_null());
    assert_eq!(rotated["pending"], 0);
    assert_eq!(rotated["retained_dedup_keys"], 0);

    json(&run(&config, &data, &["telemetry", "off"]));
    let reprovisioned = json(&run(
        &config,
        &data,
        &[
            "telemetry",
            "configure",
            "--endpoint",
            "https://127.0.0.1:11/v1/telemetry/batches",
        ],
    ));
    assert_eq!(reprovisioned["requested"], "off");
    assert_eq!(reprovisioned["effective"], "disabled");
}

#[test]
fn headless_quickstart_never_acknowledges_the_telemetry_notice() {
    let config = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    json(&run(
        &config,
        &data,
        &[
            "telemetry",
            "configure",
            "--endpoint",
            "https://127.0.0.1:9/v1/telemetry/batches",
            "--profile",
            "permitted-default",
        ],
    ));

    let automated = run(
        &config,
        &data,
        &[
            "quickstart",
            "--yes",
            "--hub",
            "skip",
            "--hook",
            "none",
            "--no-collisions",
        ],
    );
    assert!(automated.status.success());
    let after_automated = json(&run(&config, &data, &["telemetry", "inspect"]));
    assert_eq!(after_automated["reason"], "notice_required");
    assert!(after_automated["notice_seen_at"].is_null());
    assert!(after_automated["installation_id"].is_null());
    assert_eq!(after_automated["pending"], 0);

    let piped = run(&config, &data, &["quickstart"]);
    assert!(!piped.status.success());
    assert!(!String::from_utf8_lossy(&piped.stderr).contains("AgentSCM can send"));
    let after_piped = json(&run(&config, &data, &["telemetry", "inspect"]));
    assert!(after_piped["notice_seen_at"].is_null());
    assert!(after_piped["installation_id"].is_null());
}

#[test]
fn verified_install_records_after_explicit_choice_and_first_use_stays_separate() {
    let config = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    json(&run(
        &config,
        &data,
        &[
            "telemetry",
            "configure",
            "--endpoint",
            "https://127.0.0.1:9/v1/telemetry/batches",
            "--profile",
            "permitted-default",
        ],
    ));

    let chosen = command(&config, &data, &["telemetry", "on"])
        .env("AGENTSCM_VERIFIED_INSTALL", "1")
        .output()
        .unwrap();
    assert!(chosen.status.success());
    let installed = json(&run(&config, &data, &["telemetry", "inspect"]));
    let installed_events = installed["queued_events"].as_array().unwrap();
    assert_eq!(
        installed_events
            .iter()
            .filter(|event| event["feature"] == "installation")
            .count(),
        1
    );
    assert!(
        !installed_events
            .iter()
            .any(|event| event["feature"] == "first_use")
    );

    assert!(run(&config, &data, &["init"]).status.success());
    let used = json(&run(&config, &data, &["telemetry", "inspect"]));
    let used_events = used["queued_events"].as_array().unwrap();
    assert_eq!(
        used_events
            .iter()
            .filter(|event| event["feature"] == "installation")
            .count(),
        1
    );
    assert!(
        used_events
            .iter()
            .any(|event| event["feature"] == "first_use")
    );
    assert!(
        used_events
            .iter()
            .any(|event| event["feature"] == "native_source")
    );
}

#[cfg(unix)]
#[test]
fn specialized_setup_failure_keeps_its_exit_and_records_failed_feature() {
    let config = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    json(&run(
        &config,
        &data,
        &[
            "telemetry",
            "configure",
            "--endpoint",
            "https://127.0.0.1:9/v1/telemetry/batches",
            "--profile",
            "permitted-default",
        ],
    ));
    json(&run(&config, &data, &["telemetry", "on"]));

    let existing = TempDir::new().unwrap();
    let existing_root = existing.path().canonicalize().unwrap();
    let failed = run(
        &config,
        &data,
        &["setup", "begin", existing_root.to_str().unwrap()],
    );
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty());
    let rendered: Value = serde_json::from_slice(&failed.stderr).expect("specialized JSON failure");
    assert_eq!(rendered["ok"], false);
    assert_eq!(rendered["error"]["kind"], "existing_path");

    let inspected = json(&run(&config, &data, &["telemetry", "inspect"]));
    let events = inspected["queued_events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|event| { event["feature"] == "setup" && event["outcome"] == "failed" })
    );
    assert!(!events.iter().any(|event| event["feature"] == "first_use"));
}
