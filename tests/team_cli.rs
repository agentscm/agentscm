#[path = "common/team_pg.rs"]
mod team_pg;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};
use team_pg::Cluster;

fn agentscm(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .output()
        .expect("run agentscm")
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn success(output: &Output) -> Value {
    assert!(output.status.success(), "team command failed");
    assert!(
        output.stderr.is_empty(),
        "successful team command wrote stderr"
    );
    serde_json::from_slice(&output.stdout).expect("team command JSON stdout")
}

fn failure(output: &Output) -> Value {
    assert!(
        !output.status.success(),
        "team command unexpectedly succeeded"
    );
    assert!(output.stdout.is_empty(), "failed team command wrote stdout");
    serde_json::from_slice(&output.stderr).expect("team command JSON stderr")
}

fn assert_secret_absent(output: &Output, secret: &str) {
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
}

fn assert_private_credential(path: &Path) -> String {
    let secret = fs::read_to_string(path).expect("read credential");
    assert_eq!(secret.len(), 64);
    assert!(
        secret
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    secret
}

fn team_config(cluster: &Cluster, objects: &Path) -> Value {
    json!({
        "database": {
            "socket": cluster.config.socket,
            "port": cluster.config.port,
            "database": cluster.config.database,
        },
        "objects": objects,
        "origin": "https://team.example",
        "bind": "127.0.0.1:0",
        "max_concurrency": 1,
    })
}

fn bootstrap_request(repository: &str, operation: &str) -> Value {
    json!({
        "scope": {"trust_domain": "cli", "repository": repository},
        "authority_generation": 1,
        "operation": operation,
        "max_bytes": 8_000_000,
        "max_objects": 8_000,
        "principal": "operator",
        "roles": ["reader", "contributor", "administrator"],
    })
}

fn utf8(path: &Path) -> &str {
    path.to_str().expect("UTF-8 temporary path")
}

#[cfg(unix)]
#[test]
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_team_cli_credentials_bootstrap_retry_and_whoami_are_safe() {
    let cluster = Cluster::new();
    let fixture = tempfile::tempdir().unwrap();
    let data_dir = fixture.path().join("unused-main-data");
    let config = fixture.path().join("team-config.json");
    let objects = fixture.path().join("objects");
    write_json(&config, &team_config(&cluster, &objects));

    let credential = fixture.path().join("operator.credential");
    let created = agentscm(
        &data_dir,
        &["team", "credential", "--output", utf8(&credential)],
    );
    let secret = assert_private_credential(&credential);
    assert_secret_absent(&created, &secret);
    assert_eq!(success(&created), json!({"ok": true}));
    assert!(!data_dir.exists());

    let bootstrap = fixture.path().join("bootstrap.json");
    write_json(
        &bootstrap,
        &bootstrap_request("primary", "bootstrap-primary"),
    );
    let bootstrap_args = [
        "team",
        "bootstrap",
        "--config",
        utf8(&config),
        "--input",
        utf8(&bootstrap),
        "--credential-file",
        utf8(&credential),
    ];
    let first = agentscm(&data_dir, &bootstrap_args);
    assert_secret_absent(&first, &secret);
    let first_outcome = success(&first);
    assert_eq!(first_outcome["action"], "bootstrap");
    assert_eq!(first_outcome["principal"], "operator");
    assert_eq!(first_outcome["sequence"], 1);

    let replay = agentscm(&data_dir, &bootstrap_args);
    assert_secret_absent(&replay, &secret);
    assert_eq!(success(&replay), first_outcome);

    let scope = fixture.path().join("scope.json");
    write_json(
        &scope,
        &json!({"trust_domain": "cli", "repository": "primary"}),
    );
    let whoami = agentscm(
        &data_dir,
        &[
            "team",
            "whoami",
            "--config",
            utf8(&config),
            "--scope",
            utf8(&scope),
            "--credential-file",
            utf8(&credential),
        ],
    );
    assert_secret_absent(&whoami, &secret);
    let identity = success(&whoami);
    assert_eq!(
        identity["scope"],
        json!({"trust_domain":"cli","repository":"primary"})
    );
    assert_eq!(identity["authority_generation"], 1);
    assert_eq!(identity["principal"]["principal"], "operator");
    assert_eq!(
        identity["principal"]["roles"],
        json!(["reader", "contributor", "administrator"])
    );

    fs::create_dir(&objects).unwrap();
    let retained_source = objects.join("retained-source-object");
    fs::write(&retained_source, b"retained source bytes").unwrap();
    let retirement = fixture.path().join("retire-access.json");
    write_json(
        &retirement,
        &json!({
            "scope": {"trust_domain": "cli", "repository": "primary"},
            "authority_generation": 1,
            "operation": "retire-primary",
        }),
    );
    let retirement_args = [
        "team",
        "retire-access",
        "--config",
        utf8(&config),
        "--input",
        utf8(&retirement),
    ];
    let retired = agentscm(&data_dir, &retirement_args);
    assert_secret_absent(&retired, &secret);
    let retired_result = success(&retired);
    assert_eq!(retired_result["historical"], false);
    assert_eq!(retired_result["currently_retired"], true);
    assert_eq!(retired_result["outcome"]["action"], "retire_access");
    assert_eq!(retired_result["outcome"]["actor"], Value::Null);
    assert_eq!(retired_result["outcome"]["retired"], true);
    assert_eq!(retired_result["outcome"]["operation"], "retire-primary");
    assert_eq!(retired_result["outcome"]["sequence"], 2);
    assert_eq!(
        retired_result["current_authority_generation"],
        retired_result["outcome"]["authority_generation"]
    );

    let retired_replay = agentscm(&data_dir, &retirement_args);
    assert_secret_absent(&retired_replay, &secret);
    let retired_replay_result = success(&retired_replay);
    assert_eq!(retired_replay_result["historical"], false);
    assert_eq!(retired_replay_result["currently_retired"], true);
    assert_eq!(
        retired_replay_result["current_authority_generation"],
        retired_result["current_authority_generation"]
    );
    assert_eq!(retired_replay_result["outcome"], retired_result["outcome"]);

    let retired_whoami = agentscm(
        &data_dir,
        &[
            "team",
            "whoami",
            "--config",
            utf8(&config),
            "--scope",
            utf8(&scope),
            "--credential-file",
            utf8(&credential),
        ],
    );
    assert_secret_absent(&retired_whoami, &secret);
    assert_eq!(
        failure(&retired_whoami)["error"],
        "invalid: team authentication required"
    );
    assert_eq!(fs::read_to_string(&credential).unwrap(), secret);
    assert_eq!(
        fs::read(&retained_source).unwrap(),
        b"retained source bytes"
    );
    let mut retained = cluster.admin();
    let retained_state = retained
        .query_one(
            "SELECT r.retired, \
                    count(DISTINCT p.principal), \
                    count(DISTINCT CASE WHEN p.enabled THEN p.principal END), \
                    count(DISTINCT e.sequence), \
                    count(DISTINCT o.operation) \
             FROM team_repositories r \
             LEFT JOIN team_principals p USING(trust_domain,repository) \
             LEFT JOIN team_events e USING(trust_domain,repository) \
             LEFT JOIN team_outcomes o USING(trust_domain,repository) \
             WHERE r.trust_domain='cli' AND r.repository='primary' \
             GROUP BY r.retired",
            &[],
        )
        .unwrap();
    assert!(retained_state.get::<_, bool>(0));
    assert_eq!(retained_state.get::<_, i64>(1), 1);
    assert_eq!(retained_state.get::<_, i64>(2), 0);
    assert_eq!(retained_state.get::<_, i64>(3), 2);
    assert_eq!(retained_state.get::<_, i64>(4), 2);

    let failed_credential = fixture.path().join("retry.credential");
    let failed_request = fixture.path().join("failed-bootstrap.json");
    write_json(
        &failed_request,
        &bootstrap_request("retry", "bootstrap-retry"),
    );
    let failed_args = [
        "team",
        "bootstrap",
        "--config",
        utf8(&config),
        "--input",
        utf8(&failed_request),
        "--credential-file",
        utf8(&failed_credential),
    ];

    let mut admin = cluster.admin();
    admin
        .batch_execute(
            "CREATE FUNCTION reject_cli_bootstrap() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF NEW.kind='identity' AND NEW.repository='retry' THEN \
                 RAISE EXCEPTION 'raw CLI database failure detail'; \
               END IF; \
               RETURN NEW; \
             END $$; \
             CREATE TRIGGER reject_cli_bootstrap \
             BEFORE INSERT ON team_outbox FOR EACH ROW \
             EXECUTE FUNCTION reject_cli_bootstrap();",
        )
        .unwrap();

    let rejected = agentscm(&data_dir, &failed_args);
    let retry_secret = assert_private_credential(&failed_credential);
    assert_secret_absent(&rejected, &retry_secret);
    assert!(!String::from_utf8_lossy(&rejected.stderr).contains("raw CLI database failure detail"));
    let rejected_error = failure(&rejected);
    assert_eq!(rejected_error["error"], "invalid: team service unavailable");

    admin
        .batch_execute(
            "DROP TRIGGER reject_cli_bootstrap ON team_outbox; \
             DROP FUNCTION reject_cli_bootstrap();",
        )
        .unwrap();
    let recovered = agentscm(&data_dir, &failed_args);
    assert_secret_absent(&recovered, &retry_secret);
    let recovered_outcome = success(&recovered);
    assert_eq!(recovered_outcome["operation"], "bootstrap-retry");
    assert_eq!(recovered_outcome["sequence"], 1);

    let recovered_replay = agentscm(&data_dir, &failed_args);
    assert_secret_absent(&recovered_replay, &retry_secret);
    assert_eq!(success(&recovered_replay), recovered_outcome);
    assert_eq!(
        fs::read_to_string(&failed_credential).unwrap(),
        retry_secret
    );
    assert!(!data_dir.exists());
}
