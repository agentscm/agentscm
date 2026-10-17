use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Output};

use agentscm::model::FileChange;
use agentscm::store::Engine;
use agentscm::sync::{
    SOURCE_PROFILE_V2, import_bundle, qualify_source, write_bundle, write_offer, write_request,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const MAX_PROCESS_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_JSON_INPUT_BYTES: u64 = 16 * 1024 * 1024;

fn put(path: &str, content: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(content.to_vec()),
    }
}

fn run(data: &Path, args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(
        output.stdout.len() <= MAX_PROCESS_OUTPUT_BYTES,
        "{args:?}: stdout exceeded the fixture bound"
    );
    assert!(
        output.stderr.len() <= MAX_PROCESS_OUTPUT_BYTES,
        "{args:?}: stderr exceeded the fixture bound"
    );
    output
}

fn success(data: &Path, args: &[&str]) -> Value {
    let output = run(data, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn failure(data: &Path, args: &[&str]) -> Value {
    let output = run(data, args);
    assert!(
        !output.status.success(),
        "{args:?}: unexpectedly succeeded with {}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stderr).unwrap()
}

fn input(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn initialize(data: &Path) -> String {
    success(data, &["init"])["repository"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn transfer(
    source: &Engine,
    source_repository: &str,
    receiver: &Engine,
    roots: &[String],
    directory: &Path,
) {
    let offer = directory.join("offer.json");
    let request = directory.join("request.json");
    let bundle = directory.join("source.bundle");
    write_offer(source, source_repository, roots, &offer).unwrap();
    write_request(receiver, &offer, &request).unwrap();
    write_bundle(source, &offer, &request, &bundle).unwrap();
    let outcome = import_bundle(receiver, &bundle, &directory.join("import-state")).unwrap();
    assert_eq!(outcome.roots, roots.to_vec());
}

#[test]
fn failed_begin_recovers_after_bundle_and_complete_is_restart_idempotent() {
    let temp = TempDir::new().unwrap();
    let source_data = temp.path().join("source");
    let receiver_data = temp.path().join("receiver");
    let source_repository = initialize(&source_data);
    initialize(&receiver_data);

    let source = Engine::open(&source_data).unwrap();
    let receiver = Engine::open(&receiver_data).unwrap();
    let base = source
        .checkpoint(None, &[put("file.txt", b"one\ntwo\nthree\nfour\n")])
        .unwrap();
    let left = source
        .checkpoint(Some(&base), &[put("file.txt", b"ONE\ntwo\nthree\nfour\n")])
        .unwrap();
    let right = source
        .checkpoint(Some(&base), &[put("file.txt", b"one\ntwo\nthree\nFOUR\n")])
        .unwrap();

    let failed = failure(
        &receiver_data,
        &["merge", "begin", "begin-before-transfer", &left, &right],
    );
    assert_eq!(failed["ok"], false);
    let operation = success(
        &receiver_data,
        &["merge", "operation", "begin-before-transfer"],
    );
    let session = operation["session"].as_str().unwrap().to_owned();
    let planning = success(&receiver_data, &["merge", "show", &session]);
    assert_eq!(planning["state"]["phase"], "planning");
    assert_eq!(planning["state"]["generation"], 0);

    let exchange = temp.path().join("exchange");
    fs::create_dir(&exchange).unwrap();
    transfer(
        &source,
        &source_repository,
        &receiver,
        &[left.clone(), right.clone()],
        &exchange,
    );

    let recovered = success(&receiver_data, &["merge", "recover", &session]);
    assert_eq!(recovered["outcome"]["outcome"], "prepared");
    assert_eq!(recovered["outcome"]["phase"], "ready");
    let generation = recovered["outcome"]["generation"].as_u64().unwrap();
    let generation_arg = generation.to_string();
    let complete_args = [
        "merge",
        "complete",
        &session,
        "complete-after-restart",
        &generation_arg,
    ];
    let completed = success(&receiver_data, &complete_args);
    assert_eq!(completed["outcome"]["outcome"], "completed");
    assert_eq!(completed, success(&receiver_data, &complete_args));

    let merged = completed["outcome"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let revision = receiver.load_revision(&merged).unwrap();
    assert_eq!(revision.version, 2);
    assert_eq!(revision.parents, [left, right]);
    assert_eq!(
        receiver.read_file(&merged, "file.txt").unwrap(),
        b"ONE\ntwo\nthree\nFOUR\n"
    );
    let qualified = qualify_source(&receiver, &[merged]).unwrap();
    assert_eq!(qualified.version(), 2);
    assert_eq!(qualified.profile(), SOURCE_PROFILE_V2);
}

#[test]
fn base_and_conflict_commands_enforce_bounds_and_abort_preserves_sources() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("repository");
    initialize(&data);
    let engine = Engine::open(&data).unwrap();

    let unrelated_left = engine
        .checkpoint(None, &[put("left.txt", b"left\n")])
        .unwrap();
    let unrelated_right = engine
        .checkpoint(None, &[put("right.txt", b"right\n")])
        .unwrap();
    let awaiting = success(
        &data,
        &[
            "merge",
            "begin",
            "begin-unrelated",
            &unrelated_left,
            &unrelated_right,
        ],
    );
    assert_eq!(awaiting["outcome"]["phase"], "awaiting_base");
    let unrelated_session = awaiting["outcome"]["session"].as_str().unwrap().to_owned();
    let unrelated_generation = awaiting["outcome"]["generation"].as_u64().unwrap();
    let unrelated_generation_arg = unrelated_generation.to_string();

    let bases = success(
        &data,
        &["merge", "bases", &unrelated_session, "--limit", "1"],
    );
    assert_eq!(bases["page"]["entries"], json!([]));
    failure(
        &data,
        &["merge", "bases", &unrelated_session, "--limit", "1001"],
    );
    let oversized_cursor = temp.path().join("oversized-cursor.json");
    File::create(&oversized_cursor)
        .unwrap()
        .set_len(1_025)
        .unwrap();
    failure(
        &data,
        &[
            "merge",
            "bases",
            &unrelated_session,
            "--cursor",
            oversized_cursor.to_str().unwrap(),
        ],
    );

    let invalid_base = temp.path().join("invalid-base.json");
    input(&invalid_base, &json!({"choice":"empty","unexpected":true}));
    failure(
        &data,
        &[
            "merge",
            "base",
            &unrelated_session,
            "invalid-base-schema",
            &unrelated_generation_arg,
            invalid_base.to_str().unwrap(),
        ],
    );
    let oversized_input = temp.path().join("oversized-input.json");
    File::create(&oversized_input)
        .unwrap()
        .set_len(MAX_JSON_INPUT_BYTES + 1)
        .unwrap();
    failure(
        &data,
        &[
            "merge",
            "base",
            &unrelated_session,
            "oversized-base-input",
            &unrelated_generation_arg,
            oversized_input.to_str().unwrap(),
        ],
    );
    let unchanged = success(&data, &["merge", "show", &unrelated_session]);
    assert_eq!(unchanged["state"]["phase"], "awaiting_base");
    assert_eq!(unchanged["state"]["generation"], unrelated_generation);

    let empty_base = temp.path().join("empty-base.json");
    input(&empty_base, &json!({"choice":"empty"}));
    let selected = success(
        &data,
        &[
            "merge",
            "base",
            &unrelated_session,
            "select-empty-base",
            &unrelated_generation_arg,
            empty_base.to_str().unwrap(),
        ],
    );
    assert_eq!(selected["outcome"]["phase"], "ready");

    let base = engine
        .checkpoint(None, &[put("overlap.txt", b"base\n")])
        .unwrap();
    let left = engine
        .checkpoint(Some(&base), &[put("overlap.txt", b"left\n")])
        .unwrap();
    let right = engine
        .checkpoint(Some(&base), &[put("overlap.txt", b"right\n")])
        .unwrap();
    let conflict_begin = success(&data, &["merge", "begin", "begin-overlap", &left, &right]);
    assert_eq!(conflict_begin["outcome"]["phase"], "conflicts");
    let session = conflict_begin["outcome"]["session"]
        .as_str()
        .unwrap()
        .to_owned();
    let generation = conflict_begin["outcome"]["generation"].as_u64().unwrap();
    let generation_arg = generation.to_string();

    failure(&data, &["merge", "conflicts", &session, "--limit", "0"]);
    let conflicts = success(&data, &["merge", "conflicts", &session, "--limit", "1"]);
    assert_eq!(conflicts["page"]["entries"][0]["kind"], "content");
    assert_eq!(conflicts["page"]["entries"][0]["resolved"], false);
    assert!(conflicts["page"]["entries"][0]["preview"].is_null());
    let conflict_id = conflicts["page"]["entries"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let detail = success(&data, &["merge", "conflict", &session, &conflict_id]);
    assert_eq!(detail["conflict"]["resolved"], false);
    let preview: Vec<u8> = serde_json::from_value(detail["conflict"]["preview"].clone()).unwrap();
    assert!(preview.windows(7).any(|bytes| bytes == b"<<<<<<<"));
    assert!(preview.windows(7).any(|bytes| bytes == b">>>>>>>"));

    let invalid_resolution = temp.path().join("invalid-resolution.json");
    input(
        &invalid_resolution,
        &json!({"choice":"right","unexpected":true}),
    );
    failure(
        &data,
        &[
            "merge",
            "resolve",
            &session,
            "invalid-resolution-schema",
            &generation_arg,
            &conflict_id,
            invalid_resolution.to_str().unwrap(),
        ],
    );
    let valid_resolution = temp.path().join("right-resolution.json");
    input(&valid_resolution, &json!({"choice":"right"}));
    let stale_generation = (generation + 1).to_string();
    failure(
        &data,
        &[
            "merge",
            "resolve",
            &session,
            "stale-resolution",
            &stale_generation,
            &conflict_id,
            valid_resolution.to_str().unwrap(),
        ],
    );

    let abort_args = ["merge", "abort", &session, "abort-overlap", &generation_arg];
    let aborted = success(&data, &abort_args);
    assert_eq!(aborted["outcome"]["outcome"], "aborted");
    assert_eq!(aborted, success(&data, &abort_args));
    assert_eq!(
        success(&data, &["merge", "show", &session])["state"]["phase"],
        "aborted"
    );
    let retained = success(&data, &["merge", "conflict", &session, &conflict_id]);
    assert_eq!(retained["conflict"]["resolved"], false);
    assert_eq!(engine.read_file(&left, "overlap.txt").unwrap(), b"left\n");
    assert_eq!(engine.read_file(&right, "overlap.txt").unwrap(), b"right\n");
}
