//! Agent CLI and MCP preserve the workspace promise authority boundary.
use crate::workspace_hub;

use std::process::Command;

use agentscm::hub::connection::NewConnection;
use agentscm::hub::store::{
    self, NewAgentToken, agent_token_digest, browser_session_digest, mint_agent_token,
    mint_bound_agent_token, new_connection_id, unix_now,
};
use agentscm::team::auth::generate_secret;
use reqwest::Method;
use serde_json::{Value, json};

const REPOSITORY: &str = "synthetic/project";

fn cli(env: &workspace_hub::Env, token: &str, args: &[&str]) -> std::process::Output {
    let token_file = env.dir.path().join("promise-agent-token");
    std::fs::write(&token_file, token).unwrap();
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(["--no-telemetry", "promise"])
        .args(args)
        .env("AGENTSCM_HUB_URL", &env.base)
        .env("AGENTSCM_HUB_TOKEN_FILE", &token_file)
        .output()
        .unwrap()
}

fn parsed(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn proposal(repository: &str, operation: &str) -> Value {
    json!({"kind":"proposal","operation_id":operation,"repository_id":repository,
        "original_request":"Keep settings.","commitment":"Existing settings persist.",
        "criteria":[{"id":"settings","text":"Existing settings persist.","evidence_requirements":[]}],
        "constraints":[]})
}

async fn preflight(
    env: &workspace_hub::Env,
    cookie: &str,
    csrf: &str,
    repository_ids: &[&str],
) -> Value {
    let (status, body) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/admission-preflight",
            Some(cookie),
            Some(csrf),
            None,
            Some(json!({"repository_ids": repository_ids})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["admissions"].as_array().unwrap().len(),
        repository_ids.len()
    );
    assert_eq!(body["digest"].as_str().unwrap().len(), 64);
    body
}

async fn mcp(env: &workspace_hub::Env, token: &str, method: &str, params: Value) -> (u16, Value) {
    env.request(
        Method::POST,
        "/mcp",
        None,
        None,
        Some(token),
        Some(json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})),
    )
    .await
}

async fn mcp_tool(env: &workspace_hub::Env, token: &str, name: &str, arguments: Value) -> Value {
    let (status, reply) = mcp(
        env,
        token,
        "tools/call",
        json!({"name":name,"arguments":arguments}),
    )
    .await;
    assert_eq!(status, 200, "{reply}");
    reply["result"].clone()
}

async fn reopen_with_forge_binding(
    storage: workspace_hub::EnvStorage,
    forge_repository_id: Option<i64>,
) -> workspace_hub::Env {
    let path = storage.dir.path().join("workspace.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let host = config["initial_owner"]["host"].as_str().unwrap().to_owned();
    if let Some(forge_repository_id) = forge_repository_id {
        let store = store::open(&format!(
            "sqlite://{}",
            storage.dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let connection = match store.pat_connection(&host).unwrap() {
            Some(connection) => connection,
            None => store
                .insert_connection(
                    &NewConnection {
                        id: new_connection_id().unwrap(),
                        provider: "github".into(),
                        host: host.clone(),
                        kind: "pat".into(),
                        app_id: None,
                        app_slug: None,
                        acts_as: Some("synthetic".into()),
                    },
                    "fixture",
                )
                .unwrap(),
        };
        store
            .upsert_repository(
                &connection.id,
                &host,
                forge_repository_id,
                REPOSITORY,
                None,
                None,
            )
            .unwrap();
    }
    config["forge_repositories"] = forge_repository_id.map_or_else(
        || json!([]),
        |forge_repository_id| {
            json!([{
                "repository_id": REPOSITORY,
                "provider": "github",
                "host": host,
                "forge_repository_id": forge_repository_id,
            }])
        },
    );
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    workspace_hub::Env::open_existing(storage, |state, _| state).await
}

#[tokio::test]
async fn hub_web_session_cannot_replace_workspace_promise_session_or_csrf() {
    let env = workspace_hub::Env::new().await;
    let (_, workspace_cookie) = env.signin(101).await;
    let workspace_session = env.session(&workspace_cookie).await;
    let workspace_csrf = workspace_session["csrf_token"].as_str().unwrap();
    let owner = workspace_session["actor"]["user"]["id"].as_str().unwrap();
    let admission = preflight(&env, &workspace_cookie, workspace_csrf, &[REPOSITORY]).await;
    let mut create = json!({"operation_id":"web_session_boundary_create",
    "expected_admissions_digest":admission["digest"],"definition":{
        "original_request":"Keep account settings.","commitment":"Preserve settings.",
        "criteria":[{"id":"settings","text":"Settings persist.","evidence_requirements":[]}],
        "constraints":[],"repositories":[{"repository_id":REPOSITORY,"relevance":"Settings source."}]
    }});
    let (status, created) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&workspace_cookie),
            Some(workspace_csrf),
            None,
            Some(create.clone()),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    let id = created["ref"]["promise_id"].as_str().unwrap();

    let hub_secret = generate_secret().unwrap();
    let now = unix_now();
    env.store
        .insert_browser_session(
            owner,
            &browser_session_digest(hub_secret.expose()).unwrap(),
            now + 3600,
            now,
        )
        .unwrap();
    let hub_cookie = format!("__Host-agentscm_session={}", hub_secret.expose());
    let (status, hub_session) = env
        .request(
            Method::GET,
            "/v1/hub/session",
            Some(&hub_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{hub_session}");
    let hub_csrf = hub_session["csrf_token"].as_str().unwrap();
    assert_ne!(hub_csrf, workspace_csrf);

    for path in [
        "/v1/hub/workspace/promises".to_owned(),
        format!("/v1/hub/workspace/promises/{id}"),
    ] {
        assert_eq!(
            env.request(Method::GET, &path, Some(&hub_cookie), None, None, None)
                .await
                .0,
            401
        );
        assert_eq!(
            env.request(
                Method::GET,
                &path,
                None,
                None,
                Some(hub_secret.expose()),
                None,
            )
            .await
            .0,
            401
        );
    }
    let preflight_body = json!({"repository_ids":[REPOSITORY]});
    create["operation_id"] = json!("web_session_boundary_refused");
    for (path, body) in [
        (
            "/v1/hub/workspace/promises/admission-preflight",
            preflight_body,
        ),
        ("/v1/hub/workspace/promises", create.clone()),
    ] {
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&hub_cookie),
                Some(hub_csrf),
                None,
                Some(body.clone()),
            )
            .await
            .0,
            401
        );
        assert_eq!(
            env.request(
                Method::POST,
                path,
                None,
                None,
                Some(hub_secret.expose()),
                Some(body),
            )
            .await
            .0,
            401
        );
    }
    let (status, denied) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&workspace_cookie),
            Some(hub_csrf),
            None,
            Some(create.clone()),
        )
        .await;
    assert_eq!((status, &denied["error"]), (403, &json!("csrf_required")));
    assert_eq!(
        env.request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&workspace_cookie),
            Some(workspace_csrf),
            None,
            Some(create),
        )
        .await
        .0,
        200
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_reads_exact_versions_and_keeps_agent_inputs_advisory() {
    let env = workspace_hub::Env::new().await;
    let (_, owner_cookie) = env.signin(101).await;
    let owner_session = env.session(&owner_cookie).await;
    let owner_csrf = owner_session["csrf_token"].as_str().unwrap();
    let admission = preflight(&env, &owner_cookie, owner_csrf, &[REPOSITORY]).await;
    let (status, created) = env.request(Method::POST, "/v1/hub/workspace/promises", Some(&owner_cookie), Some(owner_csrf), None,
        Some(json!({"operation_id":"create_cli","expected_admissions_digest":admission["digest"],"definition":{
            "original_request":"Keep settings.","commitment":"Existing settings persist.",
            "criteria":[{"id":"settings","text":"Existing settings persist.","evidence_requirements":[]}],
            "constraints":[],"repositories":[{"repository_id":REPOSITORY,"relevance":"Settings source."}]
        }}))).await;
    assert_eq!(status, 200, "{created}");
    let id = created["ref"]["promise_id"].as_str().unwrap();
    let digest = created["ref"]["digest"].as_str().unwrap();
    let other_admission = preflight(&env, &owner_cookie, owner_csrf, &["synthetic/other"]).await;
    let (status, hidden) = env.request(Method::POST, "/v1/hub/workspace/promises", Some(&owner_cookie), Some(owner_csrf), None,
        Some(json!({"operation_id":"create_hidden","expected_admissions_digest":other_admission["digest"],"definition":{
            "original_request":"Keep unrelated state.","commitment":"Unrelated state persists.",
            "criteria":[{"id":"unrelated","text":"Unrelated state persists.","evidence_requirements":[]}],
            "constraints":[],"repositories":[{"repository_id":"synthetic/other","relevance":"Other source."}]
        }}))).await;
    assert_eq!(status, 200, "{hidden}");
    let hidden_id = hidden["ref"]["promise_id"].as_str().unwrap();

    let member = json!({"request_id":"member_cli","expected_generation":owner_session["workspace"]["generation"],
        "identity":env.identity(202),"roles":["reader","contributor"],"repository_ids":[REPOSITORY],"enabled":true});
    let (status, body) = env
        .request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(member),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (_, member_cookie) = env.signin(202).await;
    let member_user = env.session(&member_cookie).await["actor"]["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, token) =
        mint_agent_token(&*env.store, &member_user, "promise-cli", REPOSITORY, 3600).unwrap();
    let token = token.expose();

    let page = parsed(&cli(&env, token, &["list"]));
    assert_eq!(page["promises"][0]["current"]["ref"]["promise_id"], id);
    let (status, direct_page) = env
        .request(
            Method::GET,
            "/v1/hub/workspace/promises?limit=20",
            None,
            None,
            Some(token),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(page, direct_page);
    assert!(page["promises"][0].get("assessment").is_none());
    let show = parsed(&cli(&env, token, &["show", id]));
    assert_eq!(show["current"]["ref"]["digest"], digest);
    let (status, direct_show) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{id}"),
            None,
            None,
            Some(token),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(show, direct_show);
    let history = parsed(&cli(&env, token, &["history", id]));
    assert_eq!(history["versions"][0]["ref"]["version"], 1);
    let (status, direct_history) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{id}/history?limit=20"),
            None,
            None,
            Some(token),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(history, direct_history);
    assert!(
        !cli(&env, token, &["list", "--limit", "101"])
            .status
            .success()
    );
    assert!(!cli(&env, token, &["show", "../hidden"]).status.success());
    let hidden_reply = cli(&env, token, &["show", hidden_id]);
    let unknown_reply = cli(&env, token, &["show", "prm_unknown"]);
    assert!(!hidden_reply.status.success());
    assert_eq!(hidden_reply.stderr, unknown_reply.stderr);

    let list_tool = mcp_tool(&env, token, "agentscm_promises_list", json!({})).await;
    assert_eq!(list_tool["isError"], false);
    assert_eq!(list_tool["structuredContent"], page);
    // Two Authorization headers are refused on the MCP path, as on the other Hub routes.
    let doubled = env
        .client
        .post(format!("{}/mcp", env.base))
        .header("authorization", format!("Bearer {token}"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"agentscm_promises_list","arguments":{}}})
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    let status = doubled.status().as_u16();
    let reply: Value = serde_json::from_str(&doubled.text().await.unwrap()).unwrap_or(Value::Null);
    assert!(
        status == 401 || reply["result"]["isError"] == true,
        "{status} {reply}"
    );
    let show_tool = mcp_tool(
        &env,
        token,
        "agentscm_promise_show",
        json!({"promise_id":id}),
    )
    .await;
    assert_eq!(show_tool["structuredContent"], show);
    let history_tool = mcp_tool(
        &env,
        token,
        "agentscm_promise_history",
        json!({"promise_id":id}),
    )
    .await;
    assert_eq!(history_tool["structuredContent"], history);
    let hidden_tool = mcp_tool(
        &env,
        token,
        "agentscm_promise_show",
        json!({"promise_id":hidden_id}),
    )
    .await;
    let unknown_tool = mcp_tool(
        &env,
        token,
        "agentscm_promise_show",
        json!({"promise_id":"prm_unknown"}),
    )
    .await;
    assert_eq!(hidden_tool, unknown_tool);
    assert_eq!(hidden_tool["isError"], true);
    let malformed_tool = mcp_tool(
        &env,
        token,
        "agentscm_promise_show",
        json!({"promise_id":id,"unexpected":true}),
    )
    .await;
    assert_eq!(malformed_tool["isError"], true);

    // A current scope can become visible while a historical version remains hidden.
    // Every retained version is in the authorization scope, even beyond a page/lookahead.
    let (status, changed) = env
        .request(
            Method::POST,
            &format!("/v1/hub/workspace/promises/{hidden_id}/changes"),
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"operation_id":"re_scope_hidden","expected_version":1,
                "expected_digest":hidden["ref"]["digest"],"expected_admissions_digest":admission["digest"],"reason":"Scope changed.",
                "action":{"kind":"amend","content":{
                    "commitment":"Unrelated state persists.",
                    "criteria":[{"id":"unrelated","text":"Unrelated state persists.","evidence_requirements":[]}],
                    "constraints":[],"repositories":[{"repository_id":REPOSITORY,"relevance":"Now relevant here."}]
                }}})),
        )
        .await;
    assert_eq!(status, 200, "{changed}");
    let (status, changed_again) = env
        .request(
            Method::POST,
            &format!("/v1/hub/workspace/promises/{hidden_id}/changes"),
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"operation_id":"re_scope_visible_again","expected_version":2,
                "expected_digest":changed["ref"]["digest"],"expected_admissions_digest":admission["digest"],"reason":"Clarified current scope.",
                "action":{"kind":"amend","content":{
                    "commitment":"Unrelated state still persists.",
                    "criteria":[{"id":"unrelated","text":"Unrelated state persists.","evidence_requirements":[]}],
                    "constraints":[],"repositories":[{"repository_id":REPOSITORY,"relevance":"Still relevant here."}]
                }}})),
        )
        .await;
    assert_eq!(status, 200, "{changed_again}");
    assert!(cli(&env, token, &["show", hidden_id]).status.success());
    for before_version in [None, Some(3), Some(2)] {
        let suffix = before_version.map_or(String::new(), |version| {
            format!("&before_version={version}")
        });
        let history_url = format!("/v1/hub/workspace/promises/{hidden_id}/history?limit=1{suffix}");
        let (status, _) = env
            .request(Method::GET, &history_url, None, None, Some(token), None)
            .await;
        assert_eq!(status, if before_version.is_some() { 400 } else { 404 });
        let mut arguments = json!({"promise_id":hidden_id,"limit":1});
        if let Some(version) = before_version {
            arguments["before_version"] = json!(version);
        }
        let hidden_history = mcp_tool(&env, token, "agentscm_promise_history", arguments).await;
        let mut unknown_arguments = json!({"promise_id":"prm_unknown","limit":1});
        if let Some(version) = before_version {
            unknown_arguments["before_version"] = json!(version);
        }
        let unknown_history =
            mcp_tool(&env, token, "agentscm_promise_history", unknown_arguments).await;
        assert_eq!(hidden_history, unknown_history);
        assert_eq!(hidden_history["isError"], true);
    }
    assert!(
        !cli(&env, token, &["history", hidden_id, "--limit", "1"])
            .status
            .success()
    );

    let input_file = env.dir.path().join("promise-input.json");
    std::fs::write(
        &input_file,
        proposal(REPOSITORY, "cli_proposal").to_string(),
    )
    .unwrap();
    let path = input_file.to_str().unwrap();
    let first = parsed(&cli(&env, token, &["propose", "--request", path]));
    assert_eq!(first["replayed"], false);
    let replay = parsed(&cli(&env, token, &["propose", "--request", path]));
    assert_eq!(replay["input_id"], first["input_id"]);
    assert_eq!(replay["replayed"], true);
    let mut changed = proposal(REPOSITORY, "cli_proposal");
    changed["commitment"] = json!("Different commitment.");
    std::fs::write(&input_file, changed.to_string()).unwrap();
    assert!(
        !cli(&env, token, &["propose", "--request", path])
            .status
            .success()
    );
    let mut mcp_proposal = proposal(REPOSITORY, "mcp_proposal");
    mcp_proposal.as_object_mut().unwrap().remove("kind");
    let mcp_first = mcp_tool(
        &env,
        token,
        "agentscm_promise_propose",
        mcp_proposal.clone(),
    )
    .await;
    assert_eq!(mcp_first["isError"], false);
    assert_eq!(mcp_first["structuredContent"]["replayed"], false);
    let mcp_replay = mcp_tool(&env, token, "agentscm_promise_propose", mcp_proposal).await;
    assert_eq!(
        mcp_replay["structuredContent"]["input_id"],
        mcp_first["structuredContent"]["input_id"]
    );
    assert_eq!(mcp_replay["structuredContent"]["replayed"], true);
    let malformed_proposal = mcp_tool(
        &env,
        token,
        "agentscm_promise_propose",
        json!({"operation_id":"mcp_malformed","repository_id":REPOSITORY,
            "original_request":"Keep settings.","commitment":"Keep settings.",
            "criteria":[{"id":"settings","text":"Keep settings.","evidence_requirements":[],"verified":true}],
            "constraints":[]}),
    )
    .await;
    assert_eq!(malformed_proposal["isError"], true);

    std::fs::write(
        &input_file,
        proposal("synthetic/other", "foreign").to_string(),
    )
    .unwrap();
    assert!(
        !cli(&env, token, &["propose", "--request", path])
            .status
            .success()
    );
    let report = json!({"kind":"report","operation_id":"cli_report","promise_id":id,
        "promise_version":1,"promise_digest":digest,"repository_id":REPOSITORY,"summary":"Possible issue; caller report only."});
    std::fs::write(&input_file, report.to_string()).unwrap();
    assert_eq!(
        parsed(&cli(&env, token, &["report", "--request", path]))["replayed"],
        false
    );
    let mut stale_report = report.clone();
    stale_report["operation_id"] = json!("cli_stale_report");
    stale_report["promise_digest"] = json!("f".repeat(64));
    std::fs::write(&input_file, stale_report.to_string()).unwrap();
    assert!(
        !cli(&env, token, &["report", "--request", path])
            .status
            .success()
    );
    std::fs::write(&input_file, report.to_string()).unwrap();
    let mcp_report = json!({"operation_id":"mcp_report","promise_id":id,
        "promise_version":1,"promise_digest":digest,"repository_id":REPOSITORY,
        "summary":"Caller-observed concern; not evidence."});
    let mcp_report_reply = mcp_tool(&env, token, "agentscm_promise_report", mcp_report).await;
    assert_eq!(mcp_report_reply["structuredContent"]["replayed"], false);
    let wrong_repo = mcp_tool(
        &env,
        token,
        "agentscm_promise_report",
        json!({"operation_id":"mcp_foreign","promise_id":id,
        "promise_version":1,"promise_digest":digest,"repository_id":"synthetic/other",
        "summary":"Should be refused."}),
    )
    .await;
    assert_eq!(wrong_repo["isError"], true);
    let oversized = mcp_tool(
        &env,
        token,
        "agentscm_promise_report",
        json!({"operation_id":"mcp_oversized","promise_id":id,
            "promise_version":1,"promise_digest":digest,"repository_id":REPOSITORY,
            "summary":"x".repeat(8193)}),
    )
    .await;
    assert_eq!(oversized["isError"], true);
    assert!(
        !cli(&env, token, &["propose", "--request", path])
            .status
            .success()
    );
    std::fs::write(&input_file, b"{bad json").unwrap();
    assert!(
        !cli(&env, token, &["report", "--request", path])
            .status
            .success()
    );
    std::fs::write(&input_file, vec![b'x'; 64 * 1024 + 1]).unwrap();
    assert!(
        !cli(&env, token, &["report", "--request", path])
            .status
            .success()
    );
    std::fs::write(&input_file, report.to_string()).unwrap();

    let reader = generate_secret().unwrap();
    env.store
        .insert_agent_token(&NewAgentToken {
            owner: member_user.clone(),
            name: "promise-reader-cli".into(),
            repository: REPOSITORY.into(),
            digest: agent_token_digest(reader.expose()).unwrap(),
            expires_at: unix_now() + 3600,
            repository_id: None,
            read_only: true,
        })
        .unwrap();
    let readonly_page = parsed(&cli(&env, reader.expose(), &["list"]));
    let readonly_target = readonly_page["promises"]
        .as_array()
        .unwrap()
        .iter()
        .find(|view| view["current"]["ref"]["promise_id"] == id)
        .expect("target promise remains visible to the read-only token");
    assert_eq!(readonly_target["current"]["ref"]["digest"], digest);
    assert_eq!(
        parsed(&cli(&env, reader.expose(), &["show", id]))["current"]["ref"]["digest"],
        digest
    );
    assert_eq!(
        parsed(&cli(&env, reader.expose(), &["history", id]))["versions"][0]["ref"]["digest"],
        digest
    );
    assert!(
        !cli(&env, reader.expose(), &["report", "--request", path])
            .status
            .success()
    );
    std::fs::write(
        &input_file,
        proposal(REPOSITORY, "reader_proposal").to_string(),
    )
    .unwrap();
    assert!(
        !cli(&env, reader.expose(), &["propose", "--request", path])
            .status
            .success()
    );
    let (status, reader_tools) = mcp(&env, reader.expose(), "tools/list", json!({})).await;
    assert_eq!(status, 200, "{reader_tools}");
    let names: Vec<_> = reader_tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for name in [
        "agentscm_promises_list",
        "agentscm_promise_show",
        "agentscm_promise_history",
    ] {
        assert!(names.contains(&name));
    }
    for name in ["agentscm_promise_propose", "agentscm_promise_report"] {
        assert!(!names.contains(&name));
    }
    assert_eq!(
        mcp_tool(
            &env,
            reader.expose(),
            "agentscm_promise_show",
            json!({"promise_id":id})
        )
        .await["structuredContent"],
        show
    );
    let (status, _) = mcp(
        &env,
        reader.expose(),
        "tools/call",
        json!({"name":"agentscm_promise_report",
        "arguments":{"operation_id":"reader_report","promise_id":id,"promise_version":1,
        "promise_digest":digest,"repository_id":REPOSITORY,"summary":"Refused."}}),
    )
    .await;
    assert_eq!(status, 403);

    let help = cli(&env, token, &["--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for owner_action in ["create", "adopt", "amend", "retire", "transfer"] {
        assert!(
            !help
                .lines()
                .any(|line| line.split_whitespace().next() == Some(owner_action)),
            "owner action {owner_action} leaked into agent CLI"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_mcp_and_cli_rebind_hide_history_and_recover_exact_replays() {
    let initial = workspace_hub::Env::new().await;
    let storage = initial.stop().await;
    let env = reopen_with_forge_binding(storage, Some(123)).await;
    let (_, owner_cookie) = env.signin(101).await;
    let owner_session = env.session(&owner_cookie).await;
    let owner_csrf = owner_session["csrf_token"].as_str().unwrap();
    let admission = preflight(&env, &owner_cookie, owner_csrf, &[REPOSITORY]).await;
    let (status, created) = env.request(Method::POST, "/v1/hub/workspace/promises", Some(&owner_cookie), Some(owner_csrf), None,
        Some(json!({"operation_id":"create_admitted","expected_admissions_digest":admission["digest"],"definition":{
            "original_request":"Keep settings.","commitment":"Existing settings persist.",
            "criteria":[{"id":"settings","text":"Existing settings persist.","evidence_requirements":[]}],
            "constraints":[],"repositories":[{"repository_id":REPOSITORY,"relevance":"Settings source."}]
        }}))).await;
    assert_eq!(status, 200, "{created}");
    let promise_id = created["ref"]["promise_id"].as_str().unwrap().to_owned();
    let digest = created["ref"]["digest"].as_str().unwrap().to_owned();
    let member = json!({"request_id":"admitted_member","expected_generation":owner_session["workspace"]["generation"],
        "identity":env.identity(202),"roles":["reader","contributor"],"repository_ids":[REPOSITORY],"enabled":true});
    let (status, body) = env
        .request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(member),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (_, member_cookie) = env.signin(202).await;
    let user = env.session(&member_cookie).await["actor"]["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, secret, _) = mint_bound_agent_token(
        &*env.store,
        &user,
        "admitted-agent",
        REPOSITORY,
        123,
        3600,
        false,
    )
    .unwrap();
    let token = secret.expose().to_owned();
    assert_eq!(
        parsed(&cli(&env, &token, &["show", &promise_id]))["current"]["ref"]["digest"],
        digest
    );
    let proposed = proposal(REPOSITORY, "replay_admitted_proposal");
    let proposal_receipt = mcp_tool(&env, &token, "agentscm_promise_propose", {
        let mut value = proposed.clone();
        value.as_object_mut().unwrap().remove("kind");
        value
    })
    .await["structuredContent"]
        .clone();
    assert_eq!(proposal_receipt["replayed"], false);
    let reported = json!({"operation_id":"replay_admitted_report","promise_id":promise_id,
        "promise_version":1,"promise_digest":digest,"repository_id":REPOSITORY,
        "summary":"Caller report; no verified check."});
    let report_receipt = mcp_tool(&env, &token, "agentscm_promise_report", reported.clone()).await
        ["structuredContent"]
        .clone();
    assert_eq!(report_receipt["replayed"], false);

    let storage = env.stop().await;
    let env = reopen_with_forge_binding(storage, Some(456)).await;
    let (_, rebound_owner_cookie) = env.signin(101).await;
    let rebound_session = env.session(&rebound_owner_cookie).await;
    let rebound_csrf = rebound_session["csrf_token"].as_str().unwrap();
    let rebound_admission =
        preflight(&env, &rebound_owner_cookie, rebound_csrf, &[REPOSITORY]).await;
    let (status, rebound_created) = env.request(Method::POST, "/v1/hub/workspace/promises", Some(&rebound_owner_cookie), Some(rebound_csrf), None,
        Some(json!({"operation_id":"create_rebound","expected_admissions_digest":rebound_admission["digest"],"definition":{
            "original_request":"Keep new settings.","commitment":"New settings persist.",
            "criteria":[{"id":"new_settings","text":"New settings persist.","evidence_requirements":[]}],
            "constraints":[],"repositories":[{"repository_id":REPOSITORY,"relevance":"New settings source."}]
        }}))).await;
    assert_eq!(status, 200, "{rebound_created}");
    let rebound_id = rebound_created["ref"]["promise_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let rebound_digest = rebound_created["ref"]["digest"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, rebound_secret, _) = mint_bound_agent_token(
        &*env.store,
        &user,
        "rebound-agent",
        REPOSITORY,
        456,
        3600,
        false,
    )
    .unwrap();
    let rebound_token = rebound_secret.expose().to_owned();
    assert_eq!(
        parsed(&cli(&env, &rebound_token, &["show", &rebound_id]))["current"]["ref"]["digest"],
        rebound_digest
    );
    assert_eq!(
        parsed(&cli(&env, &rebound_token, &["list"]))["promises"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        !cli(&env, &rebound_token, &["show", &promise_id])
            .status
            .success()
    );
    assert!(!cli(&env, &token, &["list"]).status.success());
    assert!(!cli(&env, &token, &["show", &promise_id]).status.success());
    assert!(!cli(&env, &token, &["show", &rebound_id]).status.success());
    assert!(
        !cli(&env, &token, &["history", &promise_id])
            .status
            .success()
    );
    let hidden_show = mcp_tool(
        &env,
        &token,
        "agentscm_promise_show",
        json!({"promise_id":promise_id}),
    )
    .await;
    let unknown_show = mcp_tool(
        &env,
        &token,
        "agentscm_promise_show",
        json!({"promise_id":"prm_unknown"}),
    )
    .await;
    assert_eq!(hidden_show, unknown_show);
    let rebound_hidden_show = mcp_tool(
        &env,
        &rebound_token,
        "agentscm_promise_show",
        json!({"promise_id":promise_id}),
    )
    .await;
    assert_eq!(rebound_hidden_show, unknown_show);
    let old_agent_new_promise = mcp_tool(
        &env,
        &token,
        "agentscm_promise_show",
        json!({"promise_id":rebound_id}),
    )
    .await;
    assert_eq!(old_agent_new_promise, unknown_show);
    let hidden_history = mcp_tool(
        &env,
        &token,
        "agentscm_promise_history",
        json!({"promise_id":promise_id,"limit":1,"before_version":2}),
    )
    .await;
    let unknown_history = mcp_tool(
        &env,
        &token,
        "agentscm_promise_history",
        json!({"promise_id":"prm_unknown","limit":1,"before_version":2}),
    )
    .await;
    assert_eq!(hidden_history, unknown_history);
    let hidden_cursor = mcp_tool(
        &env,
        &token,
        "agentscm_promises_list",
        json!({"limit":1,"after":promise_id}),
    )
    .await;
    let unknown_cursor = mcp_tool(
        &env,
        &token,
        "agentscm_promises_list",
        json!({"limit":1,"after":"prm_unknown"}),
    )
    .await;
    assert_eq!(hidden_cursor, unknown_cursor);
    let mut mcp_proposal = proposed.clone();
    mcp_proposal.as_object_mut().unwrap().remove("kind");
    assert_eq!(
        mcp_tool(
            &env,
            &token,
            "agentscm_promise_propose",
            mcp_proposal.clone()
        )
        .await["isError"],
        true
    );
    assert_eq!(
        mcp_tool(&env, &token, "agentscm_promise_report", reported.clone()).await["isError"],
        true
    );

    let storage = env.stop().await;
    let env = reopen_with_forge_binding(storage, Some(123)).await;
    assert_eq!(
        parsed(&cli(&env, &token, &["show", &promise_id]))["current"]["ref"]["digest"],
        digest
    );
    assert!(!cli(&env, &rebound_token, &["list"]).status.success());
    assert!(!cli(&env, &token, &["show", &rebound_id]).status.success());
    assert_eq!(
        mcp_tool(
            &env,
            &token,
            "agentscm_promise_history",
            json!({"promise_id":promise_id})
        )
        .await["isError"],
        false
    );
    let replay =
        mcp_tool(&env, &token, "agentscm_promise_propose", mcp_proposal).await["structuredContent"]
            .clone();
    assert_eq!(replay["input_id"], proposal_receipt["input_id"]);
    assert_eq!(replay["replayed"], true);
    let replay =
        mcp_tool(&env, &token, "agentscm_promise_report", reported).await["structuredContent"]
            .clone();
    assert_eq!(replay["input_id"], report_receipt["input_id"]);
    assert_eq!(replay["replayed"], true);

    let storage = env.stop().await;
    let env = reopen_with_forge_binding(storage, None).await;
    let (_, unbound_owner_cookie) = env.signin(101).await;
    let unbound_session = env.session(&unbound_owner_cookie).await;
    let unbound_csrf = unbound_session["csrf_token"].as_str().unwrap();
    let unbound_admission =
        preflight(&env, &unbound_owner_cookie, unbound_csrf, &[REPOSITORY]).await;
    let (status, unbound_created) = env.request(Method::POST, "/v1/hub/workspace/promises", Some(&unbound_owner_cookie), Some(unbound_csrf), None,
        Some(json!({"operation_id":"create_unbound","expected_admissions_digest":unbound_admission["digest"],"definition":{
            "original_request":"Keep unbound settings.","commitment":"Unbound settings persist.",
            "criteria":[{"id":"unbound_settings","text":"Unbound settings persist.","evidence_requirements":[]}],
            "constraints":[],"repositories":[{"repository_id":REPOSITORY,"relevance":"Unbound settings source."}]
        }}))).await;
    assert_eq!(status, 200, "{unbound_created}");
    let unbound_id = unbound_created["ref"]["promise_id"].as_str().unwrap();
    let (_, unbound_secret) =
        mint_agent_token(&*env.store, &user, "unbound-agent", REPOSITORY, 3600).unwrap();
    assert_eq!(
        parsed(&cli(&env, unbound_secret.expose(), &["show", unbound_id]))["current"]["ref"]["promise_id"],
        unbound_id
    );
    assert!(!cli(&env, &token, &["list"]).status.success());
    assert!(!cli(&env, &token, &["show", unbound_id]).status.success());
    assert_eq!(
        mcp_tool(
            &env,
            &token,
            "agentscm_promise_show",
            json!({"promise_id":unbound_id})
        )
        .await["isError"],
        true
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_or_ungranted_corrupt_current_promise_matches_absent_before_decode() {
    let initial = workspace_hub::Env::new().await;
    let storage = initial.stop().await;
    let env = reopen_with_forge_binding(storage, Some(123)).await;
    let (_, owner_cookie) = env.signin(101).await;
    let owner_session = env.session(&owner_cookie).await;
    let owner_csrf = owner_session["csrf_token"].as_str().unwrap();
    let admission = preflight(&env, &owner_cookie, owner_csrf, &[REPOSITORY]).await;
    let owner_user = owner_session["actor"]["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, created) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"operation_id":"create_corrupt_scope","expected_admissions_digest":admission["digest"],"definition":{
                "original_request":"Retain settings.","commitment":"Settings stay compatible.",
                "criteria":[{"id":"compatibility","text":"Settings stay compatible.","evidence_requirements":[]}],
                "constraints":[],"repositories":[{"repository_id":REPOSITORY,"relevance":"Settings source."}]
            }})),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    let promise_id = created["ref"]["promise_id"].as_str().unwrap().to_owned();
    let digest = created["ref"]["digest"].as_str().unwrap().to_owned();
    let (status, pointer_created) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"operation_id":"create_bad_row_pointer","expected_admissions_digest":admission["digest"],"definition":{
                "original_request":"Preserve a separate setting.","commitment":"The separate setting persists.",
                "criteria":[{"id":"separate_setting","text":"The separate setting persists.","evidence_requirements":[]}],
                "constraints":[],"repositories":[{"repository_id":REPOSITORY,"relevance":"Separate settings source."}]
            }})),
        )
        .await;
    assert_eq!(status, 200, "{pointer_created}");
    let pointer_id = pointer_created["ref"]["promise_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, report) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/inputs",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(
                json!({"kind":"report","operation_id":"report_before_corruption",
            "promise_id":promise_id,"promise_version":1,"promise_digest":digest,
            "repository_id":REPOSITORY,"summary":"Unverified observation."}),
            ),
        )
        .await;
    assert_eq!(status, 200, "{report}");
    let input_id = report["input_id"].as_str().unwrap().to_owned();
    let (status, member) = env
        .request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"request_id":"member_without_corrupt_scope",
                "expected_generation":owner_session["workspace"]["generation"],
                "identity":env.identity(202),"roles":["reader"],
                "repository_ids":["synthetic/other"],"enabled":true})),
        )
        .await;
    assert_eq!(status, 200, "{member}");
    let storage = env.stop().await;
    let database = rusqlite::Connection::open(storage.dir.path().join("hub.sqlite")).unwrap();
    let payload: String = database
        .query_row(
            "SELECT payload FROM hub_promise_versions WHERE promise_id=?1 AND version=1",
            [&promise_id],
            |row| row.get(0),
        )
        .unwrap();
    let mut corrupted: Value = serde_json::from_str(&payload).unwrap();
    corrupted["definition"]["commitment"] = json!("Tampered settings commitment.");
    // This isolated test deliberately corrupts one retained row; production keeps the trigger.
    database
        .execute_batch("DROP TRIGGER hub_promise_versions_no_update")
        .unwrap();
    assert_eq!(
        database
            .execute(
                "UPDATE hub_promise_versions SET payload=?1 WHERE promise_id=?2 AND version=1",
                rusqlite::params![corrupted.to_string(), promise_id],
            )
            .unwrap(),
        1
    );
    assert_eq!(
        database
            .execute(
                "UPDATE hub_promises SET current_digest=?1 WHERE id=?2",
                rusqlite::params!["0".repeat(64), pointer_id],
            )
            .unwrap(),
        1
    );
    drop(database);

    let env = reopen_with_forge_binding(storage, Some(456)).await;
    let (_, owner_cookie) = env.signin(101).await;
    let owner_session = env.session(&owner_cookie).await;
    let owner_csrf = owner_session["csrf_token"].as_str().unwrap();
    let path = |id: &str| format!("/v1/hub/workspace/promises/{id}");
    let (hidden_status, hidden_body) = env
        .request(
            Method::GET,
            &path(&promise_id),
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    let (missing_status, missing_body) = env
        .request(
            Method::GET,
            &path("prm_missing_corrupt"),
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!((hidden_status, hidden_body), (missing_status, missing_body));
    assert_eq!(hidden_status, 404);
    for suffix in ["/history?limit=1&before_version=2", "/reports?limit=1"] {
        let hidden = env
            .request(
                Method::GET,
                &format!("{}{}", path(&promise_id), suffix),
                Some(&owner_cookie),
                None,
                None,
                None,
            )
            .await;
        let missing = env
            .request(
                Method::GET,
                &format!("{}{}", path("prm_missing_corrupt"), suffix),
                Some(&owner_cookie),
                None,
                None,
                None,
            )
            .await;
        assert_eq!(hidden, missing, "{suffix}");
        assert_eq!(
            hidden.0,
            if suffix.starts_with("/history") {
                400
            } else {
                404
            },
            "{suffix}"
        );
    }
    let hidden_input = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/inputs/{input_id}"),
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    let missing_input = env
        .request(
            Method::GET,
            "/v1/hub/workspace/promises/inputs/prr_missing_corrupt",
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(hidden_input, missing_input);
    assert_eq!(hidden_input.0, 404);
    let (hidden_status, hidden_body) = env
        .request(
            Method::GET,
            &format!("{}/capabilities", path(&promise_id)),
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    let (missing_status, missing_body) = env
        .request(
            Method::GET,
            &format!("{}/capabilities", path("prm_missing_corrupt")),
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!((hidden_status, hidden_body), (missing_status, missing_body));
    assert_eq!(hidden_status, 404);
    let mut change_results = Vec::new();
    let mut report_results = Vec::new();
    for id in [&promise_id[..], "prm_missing_corrupt"] {
        let result = env
            .request(
                Method::POST,
                &format!("{}/changes", path(id)),
                Some(&owner_cookie),
                Some(owner_csrf),
                None,
                Some(
                    json!({"operation_id":"change_hidden_corrupt","expected_version":1,
                    "expected_digest":digest,"expected_admissions_digest":admission["digest"],
                    "reason":"Test hidden scope.","action":{"kind":"retire"}}),
                ),
            )
            .await;
        assert_eq!(result.0, 404, "{}", result.1);
        change_results.push(result);
        let result = env
            .request(
                Method::POST,
                "/v1/hub/workspace/promises/inputs",
                Some(&owner_cookie),
                Some(owner_csrf),
                None,
                Some(
                    json!({"kind":"report","operation_id":"report_hidden_corrupt",
                    "promise_id":id,"promise_version":1,"promise_digest":digest,
                    "repository_id":REPOSITORY,"summary":"Caller report."}),
                ),
            )
            .await;
        assert_eq!(result.0, 404, "{}", result.1);
        report_results.push(result);
    }
    assert_eq!(change_results[0], change_results[1]);
    assert_eq!(report_results[0], report_results[1]);
    let (_, b_secret, _) = mint_bound_agent_token(
        &*env.store,
        &owner_user,
        "current-b-scope",
        REPOSITORY,
        456,
        3600,
        false,
    )
    .unwrap();
    let token = b_secret.expose();
    assert_eq!(
        mcp_tool(
            &env,
            token,
            "agentscm_promise_show",
            json!({"promise_id":promise_id})
        )
        .await,
        mcp_tool(
            &env,
            token,
            "agentscm_promise_show",
            json!({"promise_id":"prm_missing_corrupt"})
        )
        .await
    );
    assert_eq!(
        mcp_tool(
            &env,
            token,
            "agentscm_promise_history",
            json!({"promise_id":promise_id,"limit":1,"before_version":2})
        )
        .await,
        mcp_tool(
            &env,
            token,
            "agentscm_promise_history",
            json!({"promise_id":"prm_missing_corrupt","limit":1,"before_version":2})
        )
        .await
    );
    let report = |id: &str| {
        json!({"operation_id":"mcp_hidden_corrupt","promise_id":id,
        "promise_version":1,"promise_digest":digest,"repository_id":REPOSITORY,
        "summary":"Caller report."})
    };
    assert_eq!(
        mcp_tool(&env, token, "agentscm_promise_report", report(&promise_id)).await,
        mcp_tool(
            &env,
            token,
            "agentscm_promise_report",
            report("prm_missing_corrupt")
        )
        .await
    );

    let storage = env.stop().await;
    let env = reopen_with_forge_binding(storage, Some(123)).await;
    let (_, owner_cookie) = env.signin(101).await;
    let hidden = env
        .request(
            Method::GET,
            &path(&promise_id),
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    let absent = env
        .request(
            Method::GET,
            &path("prm_missing_corrupt"),
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(hidden, absent);
    assert_eq!(hidden.0, 404);
    let (pointer_status, pointer_body) = env
        .request(
            Method::GET,
            &path(&pointer_id),
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(pointer_status, 503, "{pointer_body}");
    let (_, denied_cookie) = env.signin(202).await;
    let hidden = env
        .request(
            Method::GET,
            &path(&promise_id),
            Some(&denied_cookie),
            None,
            None,
            None,
        )
        .await;
    let absent = env
        .request(
            Method::GET,
            &path("prm_missing_corrupt"),
            Some(&denied_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(hidden, absent);
    assert_eq!(hidden.0, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_preflight_freezes_source_and_exact_receipt_replays_after_restoration() {
    let initial = workspace_hub::Env::new().await;
    let storage = initial.stop().await;
    let env = reopen_with_forge_binding(storage, Some(123)).await;
    let (_, owner_cookie) = env.signin(101).await;
    let owner_session = env.session(&owner_cookie).await;
    let owner_csrf = owner_session["csrf_token"].as_str().unwrap();

    for repository_ids in [
        json!([]),
        json!([REPOSITORY, REPOSITORY]),
        json!(["synthetic/z", REPOSITORY]),
        json!([
            "synthetic/a",
            "synthetic/b",
            "synthetic/c",
            "synthetic/d",
            "synthetic/e",
            "synthetic/f",
            "synthetic/g",
            "synthetic/h",
            "synthetic/i"
        ]),
        json!(["synthetic/bad?"]),
    ] {
        let (status, _) = env
            .request(
                Method::POST,
                "/v1/hub/workspace/promises/admission-preflight",
                Some(&owner_cookie),
                Some(owner_csrf),
                None,
                Some(json!({"repository_ids": repository_ids})),
            )
            .await;
        assert_eq!(status, 400);
    }
    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/admission-preflight",
            Some(&owner_cookie),
            None,
            None,
            Some(json!({"repository_ids": [REPOSITORY]})),
        )
        .await;
    assert_ne!(status, 200);
    let owner_user = owner_session["actor"]["user"]["id"].as_str().unwrap();
    let (_, agent) =
        mint_agent_token(&*env.store, owner_user, "preflight-agent", REPOSITORY, 3600).unwrap();
    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/admission-preflight",
            None,
            None,
            Some(agent.expose()),
            Some(json!({"repository_ids": [REPOSITORY]})),
        )
        .await;
    assert_ne!(status, 200);
    let admission_a = preflight(&env, &owner_cookie, owner_csrf, &[REPOSITORY]).await;
    assert_eq!(
        admission_a["admissions"][0]["forge"]["forge_repository_id"],
        123
    );
    let body = json!({"operation_id":"preflight_source_retry",
        "expected_admissions_digest":admission_a["digest"],
        "definition":{"original_request":"Retain settings.",
            "commitment":"Settings stay compatible.",
            "criteria":[{"id":"compatibility","text":"Settings stay compatible.","evidence_requirements":[]}],
            "constraints":[],"repositories":[{"repository_id":REPOSITORY,"relevance":"Settings source."}]}});

    // Simulate a failed delivery after the version insert but before its receipt insert.
    let interrupted = rusqlite::Connection::open(env.dir.path().join("hub.sqlite")).unwrap();
    interrupted
        .execute_batch(
            "CREATE TRIGGER fixture_promise_operation_abort
        BEFORE INSERT ON hub_promise_operations BEGIN
            SELECT RAISE(ABORT, 'fixture failed before receipt');
        END;",
        )
        .unwrap();
    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, 503);
    let count: i64 = interrupted
        .query_row("SELECT count(*) FROM hub_promises", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let count: i64 = interrupted.query_row("SELECT count(*) FROM hub_promise_operations WHERE operation_id='preflight_source_retry'", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 0);
    interrupted
        .execute_batch("DROP TRIGGER fixture_promise_operation_abort")
        .unwrap();
    drop(interrupted);

    let storage = env.stop().await;
    let env = reopen_with_forge_binding(storage, Some(456)).await;
    let (_, owner_cookie) = env.signin(101).await;
    let owner_csrf = env.session(&owner_cookie).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let admission_b = preflight(&env, &owner_cookie, &owner_csrf, &[REPOSITORY]).await;
    assert_ne!(admission_a["digest"], admission_b["digest"]);
    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(&owner_csrf),
            None,
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, 409);
    let retained = rusqlite::Connection::open(env.dir.path().join("hub.sqlite")).unwrap();
    let count: i64 = retained
        .query_row("SELECT count(*) FROM hub_promises", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    drop(retained);

    let storage = env.stop().await;
    let env = reopen_with_forge_binding(storage, Some(123)).await;
    let (_, owner_cookie) = env.signin(101).await;
    let owner_csrf = env.session(&owner_cookie).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, created) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(&owner_csrf),
            None,
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["replayed"], false);

    let storage = env.stop().await;
    let env = reopen_with_forge_binding(storage, Some(456)).await;
    let (_, owner_cookie) = env.signin(101).await;
    let owner_csrf = env.session(&owner_cookie).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(&owner_csrf),
            None,
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, 404);

    let storage = env.stop().await;
    let env = reopen_with_forge_binding(storage, Some(123)).await;
    let (_, owner_cookie) = env.signin(101).await;
    let owner_csrf = env.session(&owner_cookie).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, replay) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(&owner_csrf),
            None,
            Some(body),
        )
        .await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["ref"], created["ref"]);
}
