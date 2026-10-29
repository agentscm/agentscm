#![cfg(unix)]
//! Synthetic forge observations plus exact real local Git snapshots. No live service.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agentscm::authority::Authority;
use agentscm::hub::connection::ConnectedRepository;
use agentscm::hub::forge_api::ForgeApi;
use agentscm::hub::pipeline_rules::*;
use agentscm::skill_workflow::action::{ActionProfile, CommandProfile};
use agentscm::skill_workflow::{self, RunState};
use serde_json::{Value, json};
use tempfile::TempDir;

fn git(exe: &Path, root: &Path, args: &[&str]) -> String {
    let out = Command::new(exe)
        .args(["-C"])
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn git_executable() -> PathBuf {
    for p in [
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
        "/usr/bin/git",
    ] {
        if Path::new(p).exists() {
            let out = Command::new(p).arg("--version").output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            let version = text.split_whitespace().nth(2).unwrap_or("");
            let parts: Vec<u32> = version
                .split('.')
                .take(2)
                .filter_map(|v| v.parse().ok())
                .collect();
            if parts.len() == 2 && (parts[0] > 2 || parts[0] == 2 && parts[1] >= 45) {
                return p.into();
            }
        }
    }
    panic!("requires Git >=2.45, as the snapshot bridge does");
}
struct Fixture {
    root: TempDir,
    data: PathBuf,
    source: PathBuf,
    work: PathBuf,
    rule: PipelineRule,
    commit: String,
}
fn fixture() -> Fixture {
    fixture_provider("github")
}
fn fixture_provider(provider: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let source = root.path().join("source");
    let work = root.path().join("work");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&work).unwrap();
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
    let exe = git_executable();
    git(&exe, &source, &["init", "-q"]);
    git(
        &exe,
        &source,
        &[
            "remote",
            "add",
            "origin",
            "https://forge.invalid/acme/widgets.git",
        ],
    );
    fs::create_dir_all(source.join("skills/release")).unwrap();
    fs::write(
        source.join("skills/release/SKILL.md"),
        "# Synthetic release skill\n",
    )
    .unwrap();
    fs::write(source.join("app.txt"), "initial application").unwrap();
    fs::write(source.join("skills/release/draft.json"),serde_json::to_vec(&json!({"version":1,"inputs":{"app":{"kind":"source","path":"app.txt"},"merge":{"kind":"event","field":"accepted_commit"}},"output":{"name":"draft","media_type":"application/json","max_bytes":4096},"action":{"kind":"command","profile":"echo"}})).unwrap()).unwrap();
    git(&exe, &source, &["add", "."]);
    git(
        &exe,
        &source,
        &[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "Synthetic fixture",
        ],
    );
    let commit = git(&exe, &source, &["rev-parse", "HEAD"]);
    // A dirty checkout is preserved; capture must use the exact committed bytes.
    fs::write(source.join("app.txt"), "developer edits").unwrap();
    let profile = ActionProfile {
        version: 1,
        id: "local".into(),
        provider: "claude_code".into(),
        claude_executable: None,
        model: None,
        commands: BTreeMap::from([(
            "echo".into(),
            CommandProfile {
                executable: "/bin/cat".into(),
                args: vec![],
            },
        )]),
        timeout_ms: 1000,
        max_output_bytes: 4096,
    };
    let profile_path = root.path().join("profile.json");
    fs::write(&profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    fs::set_permissions(&profile_path, fs::Permissions::from_mode(0o600)).unwrap();
    Authority::open(&data).unwrap();
    let recipe:PipelineRecipe=serde_json::from_value(json!({"version":1,"name":"release","skill":{"entrypoint":"skills/release/SKILL.md","resources":["skills/release/draft.json"]},"steps":[{"id":"draft","needs":[],"required":true,"action":{"kind":"agent_action","resource":"draft.json"}}]})).unwrap();
    let mut rule = PipelineRule {
        repository: "acme/widgets".into(),
        provider: provider.into(),
        host: "forge.invalid".into(),
        provider_repository_id: 42,
        rule_id: "release".into(),
        generation: 1,
        enabled: true,
        trigger: MergeTrigger {
            target: "main".into(),
            labels_all: BTreeSet::from(["ship".into()]),
        },
        recipe_digest: recipe.digest().unwrap(),
        recipe,
        skill_digest: "initial".into(),
        profile_id: "local".into(),
        profile_digest: profile.digest().unwrap(),
        profile_path,
        authority_ceiling: "productive_local".into(),
        git_identity: agentscm::execution::executable::capture(&exe, &[&data, &source]).unwrap(),
        git: exe.canonicalize().unwrap(),
        git_repository: source.clone(),
    };
    let directory = data.join("pipeline-rules");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let mut j = RuleJournal::open(&directory.join("state.sqlite")).unwrap();
    j.bind_repository(&rule).unwrap();
    drop(j);
    rule.skill_digest = compile_source(&data, &rule, &commit)
        .unwrap()
        .skill_digest()
        .into();
    Fixture {
        root,
        data,
        source,
        work,
        rule,
        commit,
    }
}
fn event(f: &Fixture) -> ConfirmedMerge {
    ConfirmedMerge {
        provider: f.rule.provider.clone(),
        host: f.rule.host.clone(),
        repository: f.rule.repository.clone(),
        provider_repository_id: 42,
        proposal_number: 7,
        accepted_commit: f.commit.clone(),
        target: "main".into(),
        labels: BTreeSet::from(["ship".into()]),
        observed_at: 123,
        delivery_id: Some("hint".into()),
    }
}
fn journal(f: &Fixture) -> RuleJournal {
    RuleJournal::open(&f.data.join("pipeline-rules/state.sqlite")).unwrap()
}

#[test]
fn offline_accept_restart_claim_dispatch_and_redelivery_reuse_exact_source() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let accepted = j.accept(&event(&f)).unwrap();
    assert_eq!(accepted.len(), 1);
    drop(j);
    let mut j = journal(&f);
    assert_eq!(j.pending(8).unwrap(), accepted);
    let claim = j.claim(&accepted[0].dispatch_id).unwrap();
    assert!(j.claim(&accepted[0].dispatch_id).is_err());
    let run = deliver(&f.data, &mut j, &claim, &f.work).unwrap();
    assert_eq!(run.state, RunState::Succeeded);
    let bytes = skill_workflow::runtime::artifact(&f.data, &run.run_id, "draft", "draft").unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["inputs"]["app"], "initial application");
    assert_eq!(value["inputs"]["merge"], f.commit);
    assert_eq!(
        fs::read_to_string(f.source.join("app.txt")).unwrap(),
        "developer edits"
    );
    assert_eq!(j.dispatches(8).unwrap()[0].state, "complete");
    assert!(j.accept(&event(&f)).unwrap().is_empty());
}
#[test]
fn cas_edits_reenable_and_delivery_ids_do_not_replay_merge() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let first = j.accept(&event(&f)).unwrap();
    assert!(j.install(&f.rule, None).is_err());
    let mut edited = f.rule.clone();
    edited.generation = 2;
    edited.enabled = false;
    j.install(&edited, Some(1)).unwrap();
    let claim = j.claim(&first[0].dispatch_id).unwrap();
    assert!(deliver(&f.data, &mut j, &claim, &f.work).is_err());
    edited.generation = 3;
    edited.enabled = true;
    j.install(&edited, Some(2)).unwrap();
    let mut e = event(&f);
    e.delivery_id = Some("another-delivery".into());
    assert!(j.accept(&e).unwrap().is_empty());
    j.recover(&first[0].dispatch_id, claim.generation).unwrap();
    let next = j.claim(&first[0].dispatch_id).unwrap();
    assert!(j.finish(&claim, "complete", None).is_err());
    assert_eq!(
        deliver(&f.data, &mut j, &next, &f.work).unwrap().run_id,
        first[0].run_id
    );
}
#[test]
fn preview_explains_identity_branch_labels_and_disabled_without_dispatch() {
    let f = fixture();
    let mut r = f.rule.clone();
    r.enabled = false;
    let mut e = event(&f);
    e.host = "other.invalid".into();
    e.target = "dev".into();
    e.labels.clear();
    let p = preview(&r, &e);
    assert!(!p.matches);
    assert_eq!(p.reasons.len(), 4);
    let mut j = journal(&f);
    j.install(&r, None).unwrap();
    assert!(j.accept(&e).is_err());
    assert!(j.dispatches(8).unwrap().is_empty());
}
#[test]
fn changed_or_missing_operator_profile_blocks_frozen_dispatch() {
    for remove in [false, true] {
        let f = fixture();
        let mut j = journal(&f);
        j.install(&f.rule, None).unwrap();
        let d = j.accept(&event(&f)).unwrap().remove(0);
        if remove {
            fs::remove_file(&f.rule.profile_path).unwrap();
        } else {
            fs::write(&f.rule.profile_path, "{}").unwrap();
        }
        let claim = j.claim(&d.dispatch_id).unwrap();
        assert!(deliver(&f.data, &mut j, &claim, &f.work).is_err());
        assert_eq!(j.dispatches(8).unwrap()[0].state, "blocked");
    }
}
#[test]
fn crash_after_start_adopts_run_and_interrupted_action_never_replays() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let d = j.accept(&event(&f)).unwrap().remove(0);
    let claim = j.claim(&d.dispatch_id).unwrap();
    let plan = compile_source(&f.data, &f.rule, &f.commit).unwrap();
    skill_workflow::start(&f.data, &d.run_id, &plan).unwrap();
    // Simulate process loss after durable action reservation but before its observation.
    let db = rusqlite::Connection::open(f.data.join("skill-workflows/state.sqlite")).unwrap();
    db.execute(
        "UPDATE workflow_steps SET state='running' WHERE run_id=?1",
        [&d.run_id],
    )
    .unwrap();
    drop(db);
    j.recover(&d.dispatch_id, claim.generation).unwrap();
    let claim = j.claim(&d.dispatch_id).unwrap();
    let error = deliver(&f.data, &mut j, &claim, &f.work).unwrap_err();
    assert!(error.to_string().contains("interrupted"));
    let run = skill_workflow::runtime::status(&f.data, &d.run_id).unwrap();
    assert_eq!(run.run_id, d.run_id);
    assert_ne!(run.state, RunState::Succeeded);
    assert_eq!(j.dispatches(8).unwrap()[0].state, "blocked");
    assert!(skill_workflow::runtime::artifact(&f.data, &run.run_id, "draft", "draft").is_err());
}
#[test]
fn missing_local_commit_blocks_without_fetch_and_changed_skill_is_not_run() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let mut e = event(&f);
    e.accepted_commit = "f".repeat(40);
    let d = j.accept(&e).unwrap().remove(0);
    let claim = j.claim(&d.dispatch_id).unwrap();
    assert!(deliver(&f.data, &mut j, &claim, &f.work).is_err());
    fs::write(f.source.join("skills/release/SKILL.md"), "# changed skill").unwrap();
    git(&f.rule.git, &f.source, &["add", "."]);
    git(
        &f.rule.git,
        &f.source,
        &[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "Changed synthetic resource",
        ],
    );
    e.accepted_commit = git(&f.rule.git, &f.source, &["rev-parse", "HEAD"]);
    e.proposal_number = 8;
    let d = j.accept(&e).unwrap().remove(0);
    let claim = j.claim(&d.dispatch_id).unwrap();
    assert!(
        deliver(&f.data, &mut j, &claim, &f.work)
            .unwrap_err()
            .to_string()
            .contains("resources changed")
    );
}

async fn fake(
    provider: &str,
    pull: Value,
    tip: Option<String>,
) -> (String, tokio::task::JoinHandle<()>) {
    use axum::{Json, Router, routing::get};
    let pull = Arc::new(pull);
    let body = pull.clone();
    let tip = tip.clone();
    let app = if provider == "github" {
        Router::new()
            .route(
                "/repos/acme/widgets",
                get(|| async { Json(json!({"id":42,"full_name":"acme/widgets"})) }),
            )
            .route(
                "/repos/acme/widgets/pulls/7",
                get(move || {
                    let b = body.clone();
                    async move { Json((*b).clone()) }
                }),
            )
    } else {
        Router::new()
            .route(
                "/projects/{name}",
                get(|| async { Json(json!({"id":42,"path_with_namespace":"acme/widgets"})) }),
            )
            .route(
                "/projects/42/merge_requests/7",
                get(move || {
                    let b = body.clone();
                    async move { Json((*b).clone()) }
                }),
            )
            .route(
                "/projects/42/repository/branches/main",
                get(move || {
                    let tip = tip.clone();
                    async move { Json(json!({"name":"main","commit":{"id":tip,"committed_date":"2026-10-24T00:00:00Z"}})) }
                }),
            )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}
fn connected() -> ConnectedRepository {
    ConnectedRepository {
        id: "local".into(),
        connection: "local".into(),
        host: "forge.invalid".into(),
        repository_id: 42,
        full_name: "acme/widgets".into(),
        node_id: None,
        installation_id: None,
        pulls_etag: None,
    }
}
#[tokio::test]
async fn live_reads_confirm_github_and_gitlab_shapes_and_observation_labels() {
    let sha = "a".repeat(40);
    for provider in ["github", "gitlab"] {
        let pull = if provider == "github" {
            json!({"number":7,"merged":true,"merge_commit_sha":sha,"base":{"ref":"main","repo":{"id":42,"full_name":"acme/widgets"}},"labels":[{"name":"ship"}]})
        } else {
            json!({"id":70,"iid":7,"state":"merged","target_branch":"main","target_project_id":42,"source_branch":"feature","merge_commit_sha":null,"squash_commit_sha":sha,"labels":["ship"]})
        };
        let (url, task) = fake(provider, pull, None).await;
        let gh = agentscm::hub::github::Api::new(&url, &url).unwrap();
        let gl = agentscm::hub::gitlab::Api::new(&url).unwrap();
        let api = if provider == "github" {
            ForgeApi::Github(&gh)
        } else {
            ForgeApi::Gitlab(&gl)
        };
        let event = confirm_merge(api, "synthetic", &connected(), 7, 123, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.accepted_commit, sha);
        assert_eq!(event.labels, BTreeSet::from(["ship".into()]));
        assert_eq!(event.observed_at, 123);
        task.abort();
    }
}
#[tokio::test]
async fn nonmerged_and_unprovable_fast_forward_do_not_accept_events() {
    let sha = "a".repeat(40);
    for (merged, tip, expected) in [
        (false, Some(sha.clone()), false),
        (true, Some("b".repeat(40)), false),
        (true, Some(sha.clone()), true),
    ] {
        let pull = json!({"id":70,"iid":7,"state":if merged{"merged"}else{"opened"},"target_branch":"main","target_project_id":42,"source_branch":"feature","sha":sha,"merge_commit_sha":null,"squash_commit_sha":null,"labels":["ship"]});
        let (url, task) = fake("gitlab", pull, tip).await;
        let gl = agentscm::hub::gitlab::Api::new(&url).unwrap();
        let result =
            confirm_merge(ForgeApi::Gitlab(&gl), "synthetic", &connected(), 7, 1, None).await;
        if expected {
            assert!(result.unwrap().is_some());
        } else {
            assert!(result.is_err() || result.unwrap().is_none());
        }
        task.abort();
    }
}

#[test]
fn disabled_observation_cannot_be_replayed_by_reenable_or_filter_edit() {
    let f = fixture();
    let mut j = journal(&f);
    let mut r = f.rule.clone();
    r.enabled = false;
    j.install(&r, None).unwrap();
    assert!(j.accept(&event(&f)).unwrap().is_empty());
    r.enabled = true;
    r.generation = 2;
    j.install(&r, Some(1)).unwrap();
    assert!(j.accept(&event(&f)).unwrap().is_empty());
    assert!(j.pending(10).unwrap().is_empty());
}
#[test]
fn cli_installs_previews_and_picks_up_with_private_local_state() {
    let f = fixture();
    let r = &f.rule;
    let manifest = f.root.path().join("install.json");
    fs::write(&manifest,serde_json::to_vec(&json!({"repository":r.repository,"provider":r.provider,"host":r.host,"provider_repository_id":r.provider_repository_id,
        "rule_id":r.rule_id,"enabled":true,"trigger":r.trigger,"recipe":r.recipe,"profile_path":r.profile_path,"git":r.git,"git_repository":r.git_repository,"source_commit":f.commit})).unwrap()).unwrap();
    let call = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--data-dir")
            .arg(&f.data)
            .arg("--no-telemetry")
            .arg("pipeline")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let result = call(&["install", manifest.to_str().unwrap()]);
    assert_eq!(result["rule"]["generation"], 1);
    let event_path = f.root.path().join("preview.json");
    fs::write(&event_path, serde_json::to_vec(&event(&f)).unwrap()).unwrap();
    let preview = call(&[
        "preview",
        "acme/widgets",
        "release",
        event_path.to_str().unwrap(),
    ]);
    assert_eq!(preview["verified"], false);
    assert_eq!(preview["preview"]["matches"], true);
    let mut j = RuleJournal::open(&f.data.join("pipeline-rules/state.sqlite")).unwrap();
    let dispatch = j.accept(&event(&f)).unwrap().remove(0);
    drop(j);
    let pickup = call(&["pickup"]);
    assert_eq!(pickup["deliveries"][0]["run"]["state"], "succeeded");
    assert_eq!(pickup["deliveries"][0]["run"]["run_id"], dispatch.run_id);
    let status = call(&["status"]);
    assert_eq!(status["dispatches"][0]["state"], "complete");
    let watch = call(&["watch", "--once"]);
    assert_eq!(watch["pickup"]["deliveries"].as_array().unwrap().len(), 0);
}

#[test]
fn crash_after_workflow_start_before_dispatch_adopts_same_run() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let d = j.accept(&event(&f)).unwrap().remove(0);
    let old = j.claim(&d.dispatch_id).unwrap();
    let plan = compile_source(&f.data, &f.rule, &f.commit).unwrap();
    skill_workflow::start(&f.data, &d.run_id, &plan).unwrap();
    drop(j);
    let mut j = journal(&f);
    j.recover(&d.dispatch_id, old.generation).unwrap();
    let claim = j.claim(&d.dispatch_id).unwrap();
    let run = deliver(&f.data, &mut j, &claim, &f.work).unwrap();
    assert_eq!(run.run_id, d.run_id);
    assert_eq!(run.state, RunState::Succeeded);
    assert!(j.accept(&event(&f)).unwrap().is_empty());
}

#[test]
fn source_origin_must_match_verified_forge_identity_without_checkout_edits() {
    let f = fixture();
    git(
        &f.rule.git,
        &f.source,
        &[
            "remote",
            "set-url",
            "origin",
            "https://other.invalid/acme/widgets.git",
        ],
    );
    assert!(
        compile_source(&f.data, &f.rule, &f.commit)
            .unwrap_err()
            .to_string()
            .contains("origin")
    );
    assert_eq!(
        fs::read_to_string(f.source.join("app.txt")).unwrap(),
        "developer edits"
    );
}
#[test]
fn status_pagination_and_exact_lookup_retain_recovery_after_long_history() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let mut ids = Vec::new();
    for number in 1..=260 {
        let mut e = event(&f);
        e.proposal_number = number;
        let d = j.accept(&e).unwrap().remove(0);
        let claim = j.claim(&d.dispatch_id).unwrap();
        j.finish(&claim, "complete", None).unwrap();
        ids.push(d.dispatch_id);
    }
    let mut e = event(&f);
    e.proposal_number = 261;
    let d = j.accept(&e).unwrap().remove(0);
    let claim = j.claim(&d.dispatch_id).unwrap();
    j.finish(&claim, "blocked", Some("synthetic interruption"))
        .unwrap();
    let page = j.dispatches_page(256, None).unwrap();
    assert_eq!(page.len(), 256);
    assert_eq!(page[0].dispatch.dispatch_id, d.dispatch_id);
    let older = j
        .dispatches_page(256, Some(page.last().unwrap().sequence))
        .unwrap();
    assert_eq!(older.len(), 5);
    assert_eq!(j.dispatch(&ids[0]).unwrap().state, "complete");
    assert_eq!(
        j.dispatch(&d.dispatch_id).unwrap().claim_generation,
        claim.generation
    );
    j.recover(&d.dispatch_id, claim.generation).unwrap();
    assert_eq!(j.pending(8).unwrap()[0].dispatch_id, d.dispatch_id);
}
#[test]
fn disabling_rule_during_action_stops_the_owned_process_and_retains_reason() {
    active_rule_revocation(false);
}
#[test]
fn changing_profile_during_action_stops_the_owned_process_and_retains_reason() {
    active_rule_revocation(true);
}
fn active_rule_revocation(profile_change: bool) {
    let mut f = fixture();
    let marker = f.root.path().join("started");
    let script = f.root.path().join("slow-action");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n/usr/bin/touch '{}'\n/bin/cat >/dev/null\n/bin/sleep 30\nprintf '{{}}'\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let mut profile: ActionProfile =
        serde_json::from_slice(&fs::read(&f.rule.profile_path).unwrap()).unwrap();
    profile.commands.get_mut("echo").unwrap().executable = script;
    profile.timeout_ms = 10000;
    f.rule.profile_digest = profile.digest().unwrap();
    fs::write(&f.rule.profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let d = j.accept(&event(&f)).unwrap().remove(0);
    let claim = j.claim(&d.dispatch_id).unwrap();
    drop(j);
    let data = f.data.clone();
    let work = f.work.clone();
    let journal_path = f.data.join("pipeline-rules/state.sqlite");
    let path = journal_path.clone();
    let worker = std::thread::spawn(move || {
        let mut j = RuleJournal::open(&path).unwrap();
        deliver(&data, &mut j, &claim, &work)
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !marker.exists() && !worker.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !marker.exists() {
        panic!("action did not dispatch: {:?}", worker.join().unwrap());
    }
    let mut j = RuleJournal::open(&journal_path).unwrap();
    if profile_change {
        profile.timeout_ms += 1;
        fs::write(&f.rule.profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    } else {
        let mut disabled = f.rule.clone();
        disabled.enabled = false;
        disabled.generation = 2;
        j.install(&disabled, Some(1)).unwrap();
    }
    assert!(worker.join().unwrap().is_err());
    let view = j.dispatch(&d.dispatch_id).unwrap();
    assert_eq!(view.state, "blocked");
    let reason = view.reason.unwrap();
    assert!(reason.contains(if profile_change {
        "profile revoked or changed"
    } else {
        "disabled"
    }));
    assert!(skill_workflow::runtime::artifact(&f.data, &d.run_id, "draft", "draft").is_err());
}

#[test]
fn typed_backup_verify_restore_retains_pending_claimed_and_root_identity() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let pending = j.accept(&event(&f)).unwrap().remove(0);
    let mut e = event(&f);
    e.proposal_number = 8;
    let claimed = j.accept(&e).unwrap().remove(0);
    let claim = j.claim(&claimed.dispatch_id).unwrap();
    drop(j);
    let backup = f.root.path().join("backup");
    let manifest = agentscm::local_recovery::backup(&f.data, &backup, &[], &[], true).unwrap();
    assert!(
        manifest
            .entries
            .iter()
            .any(|v| v.path.ends_with("pipeline-rules/state.sqlite"))
    );
    assert!(
        !manifest
            .entries
            .iter()
            .any(|v| v.path.ends_with("state.sqlite-wal") || v.path.ends_with("state.sqlite-shm"))
    );
    agentscm::local_recovery::verify(&backup).unwrap();
    let restored = f.root.path().join("restored");
    agentscm::local_recovery::restore(&backup, &restored).unwrap();
    assert!(RuleJournal::open(&restored.join("data/pipeline-rules/state.sqlite")).is_err());
    agentscm::local_recovery::activate(&restored, true, true).unwrap();
    let mut j = RuleJournal::open(&restored.join("data/pipeline-rules/state.sqlite")).unwrap();
    assert_eq!(j.pending(8).unwrap(), vec![pending]);
    assert_eq!(j.dispatch(&claimed.dispatch_id).unwrap().state, "claimed");
    j.recover(&claimed.dispatch_id, claim.generation).unwrap();
    assert_eq!(j.pending(8).unwrap().len(), 2);
    assert!(j.assert_repository("gitlab", &f.rule.host, 42).is_err());
}

#[test]
fn typed_pipeline_recovery_rejects_corruption_schema_foreign_keys_and_payload_drift() {
    for failure in [
        "corrupt",
        "schema",
        "foreign_key",
        "rule_payload",
        "dispatch_payload",
        "native_binding",
    ] {
        let f = fixture();
        let mut j = journal(&f);
        j.install(&f.rule, None).unwrap();
        let d = j.accept(&event(&f)).unwrap().remove(0);
        drop(j);
        let path = f.data.join("pipeline-rules/state.sqlite");
        if failure == "corrupt" {
            fs::write(&path, b"not a SQLite database").unwrap();
        } else {
            let db = rusqlite::Connection::open(&path).unwrap();
            match failure {
                "schema" => db.pragma_update(None, "user_version", 999).unwrap(),
                "foreign_key" => {
                    db.execute_batch("PRAGMA foreign_keys=OFF; DELETE FROM pipeline_events;")
                        .unwrap();
                }
                "rule_payload" => {
                    db.execute("UPDATE pipeline_rules SET generation=generation+1", [])
                        .unwrap();
                }
                "dispatch_payload" => {
                    db.execute(
                        "UPDATE pipeline_dispatches SET run_id='wrong-run' WHERE dispatch_id=?1",
                        [d.dispatch_id],
                    )
                    .unwrap();
                }
                "native_binding" => {
                    db.execute(
                        "UPDATE pipeline_binding SET native_repository='another-root'",
                        [],
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
        }
        assert!(RuleJournal::open(&path).is_err(), "{failure}");
        assert!(
            agentscm::local_recovery::backup(
                &f.data,
                &f.root.path().join("backup"),
                &[],
                &[],
                true
            )
            .is_err(),
            "{failure}"
        );
    }
}

#[test]
fn copied_backup_revalidates_pipeline_payload_after_matching_file_hashes() {
    use sha2::{Digest, Sha256};
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    j.accept(&event(&f)).unwrap();
    drop(j);
    let backup = f.root.path().join("backup");
    agentscm::local_recovery::backup(&f.data, &backup, &[], &[], true).unwrap();
    let payload = backup.join("payload/data/pipeline-rules/state.sqlite");
    {
        let db = rusqlite::Connection::open(&payload).unwrap();
        db.execute(
            "UPDATE pipeline_dispatches SET run_id='payload-mismatch'",
            [],
        )
        .unwrap();
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    }
    let manifest_path = backup.join("recovery-manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let bytes = fs::read(&payload).unwrap();
    for entry in manifest["entries"].as_array_mut().unwrap() {
        if entry["path"]
            .as_str()
            .unwrap()
            .ends_with("pipeline-rules/state.sqlite")
        {
            entry["sha256"] = json!(format!("{:x}", Sha256::digest(&bytes)));
            entry["bytes"] = json!(bytes.len());
        }
    }
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(agentscm::local_recovery::verify(&backup).is_err());
    assert!(
        agentscm::local_recovery::restore(&backup, &f.root.path().join("restore-bad")).is_err()
    );
}

#[test]
fn singleton_binding_rejects_second_repository_before_capture_and_survives_rename() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let mut other = f.rule.clone();
    other.rule_id = "other".into();
    other.provider_repository_id = 43;
    other.repository = "acme/private".into();
    assert!(j.bind_repository(&other).is_err());
    assert!(j.install(&other, None).is_err());
    assert!(compile_source(&f.data, &other, &f.commit).is_err());
    let mut e = event(&f);
    e.provider_repository_id = 43;
    e.repository = other.repository.clone();
    assert!(j.accept(&e).is_err());
    let mut renamed = f.rule.clone();
    renamed.repository = "acme/renamed".into();
    renamed.generation = 2;
    j.bind_repository(&renamed).unwrap();
    j.install(&renamed, Some(1)).unwrap();
    assert_eq!(j.rules().unwrap().len(), 1);
    assert_eq!(j.rule("acme/renamed", "release").unwrap().generation, 2);
    drop(j);
    assert!(
        journal(&f)
            .assert_repository("github", "forge.invalid", 42)
            .is_ok()
    );
}

#[test]
fn changed_or_candidate_git_controller_blocks_before_executing_it() {
    let f = fixture();
    let controller = f.root.path().join("operator-git");
    fs::copy(&f.rule.git, &controller).unwrap();
    fs::set_permissions(&controller, fs::Permissions::from_mode(0o700)).unwrap();
    let mut rule = f.rule.clone();
    rule.git = controller.canonicalize().unwrap();
    rule.git_identity =
        agentscm::execution::executable::capture(&rule.git, &[&f.data, &f.source]).unwrap();
    let mut j = journal(&f);
    j.install(&rule, None).unwrap();
    let d = j.accept(&event(&f)).unwrap().remove(0);
    let marker = f.root.path().join("git-was-executed");
    fs::write(
        &controller,
        format!("#!/bin/sh\n/usr/bin/touch '{}'\n", marker.display()),
    )
    .unwrap();
    let claim = j.claim(&d.dispatch_id).unwrap();
    assert!(deliver(&f.data, &mut j, &claim, &f.work).is_err());
    assert!(!marker.exists());
    let candidate = f.source.join("candidate-git");
    fs::copy(&controller, &candidate).unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(agentscm::execution::executable::capture(&candidate, &[&f.data, &f.source]).is_err());
    assert!(!marker.exists());
}

#[derive(Default)]
struct PollFakeState {
    provider: String,
    pulls: BTreeMap<i64, Value>,
    listing: Vec<i64>,
    requests: Vec<String>,
    failure: Option<(String, u16)>,
    bad_query: bool,
    listing_entries: Option<Vec<Value>>,
    target_tip: Option<String>,
}
fn raw_poll_pull(provider: &str, number: i64, sha: &str, merged: bool) -> Value {
    if provider == "github" {
        json!({"number":number,"merged":merged,"merge_commit_sha":sha,"base":{"ref":"main","repo":{"id":42,"full_name":"acme/widgets"}},"labels":[{"name":"ship"}],"updated_at":"2026-01-01T00:00:00Z"})
    } else {
        json!({"id":number*10,"iid":number,"state":if merged{"merged"}else{"closed"},"target_branch":"main","target_project_id":42,"source_branch":"feature","merge_commit_sha":sha,"labels":["ship"],"updated_at":"2026-01-01T00:00:00Z"})
    }
}
async fn poll_fake(
    provider: &str,
    sha: &str,
) -> (
    String,
    Arc<std::sync::Mutex<PollFakeState>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{
        Json, Router,
        extract::State,
        http::{StatusCode, Uri, header::HeaderValue},
        response::IntoResponse,
        routing::get,
    };
    let state = Arc::new(std::sync::Mutex::new(PollFakeState {
        provider: provider.into(),
        pulls: BTreeMap::from([(7, raw_poll_pull(provider, 7, sha, true))]),
        listing: vec![7],
        ..Default::default()
    }));
    let app = Router::new()
        .fallback(get(
            |State(state): State<Arc<std::sync::Mutex<PollFakeState>>>, uri: Uri| async move {
                let mut s = state.lock().unwrap();
                let path = uri.path();
                let kind = if path.ends_with("/pulls") || path.ends_with("/merge_requests") {
                    "list"
                } else if path.contains("/pulls/") || path.contains("/merge_requests/") {
                    "pull"
                } else if path.contains("/repository/branches/") {
                    "ref"
                } else {
                    "repo"
                };
                s.requests.push(kind.into());
                if let Some((key, status)) = &s.failure
                    && (key == kind
                        || key == &format!("{kind}:{}", path.rsplit('/').next().unwrap_or("")))
                {
                    let mut response = (
                        StatusCode::from_u16(*status).unwrap(),
                        Json(json!({"message":"synthetic refusal"})),
                    )
                        .into_response();
                    if *status == 429 {
                        let reset = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            + 600;
                        response
                            .headers_mut()
                            .insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
                        response.headers_mut().insert(
                            "x-ratelimit-reset",
                            HeaderValue::from_str(&reset.to_string()).unwrap(),
                        );
                        response
                            .headers_mut()
                            .insert("retry-after", HeaderValue::from_static("600"));
                    }
                    return response;
                }
                if kind=="ref" {return Json(json!({"name":"main","commit":{"id":s.target_tip,"committed_date":"2026-10-24T00:00:00Z"}})).into_response();}
                if kind == "repo" {
                    return Json(if s.provider == "github" {
                        json!({"id":42,"full_name":"acme/widgets"})
                    } else {
                        json!({"id":42,"path_with_namespace":"acme/widgets"})
                    })
                    .into_response();
                }
                if kind == "list" {
                    let url = reqwest::Url::parse(&format!("http://fake.invalid{uri}")).unwrap();
                    let query: BTreeMap<String, String> = url
                        .query_pairs()
                        .map(|(a, b)| (a.into(), b.into()))
                        .collect();
                    let expected = if s.provider == "github" {
                        vec![
                            ("state", "closed"),
                            ("sort", "updated"),
                            ("direction", "desc"),
                            ("per_page", "100"),
                            ("page", "1"),
                        ]
                    } else {
                        vec![
                            ("state", "merged"),
                            ("order_by", "updated_at"),
                            ("sort", "desc"),
                            ("per_page", "100"),
                            ("page", "1"),
                        ]
                    };
                    if query.len() != expected.len()
                        || expected
                            .iter()
                            .any(|(k, v)| query.get(*k).map(String::as_str) != Some(*v))
                    {
                        s.bad_query = true;
                        return StatusCode::BAD_REQUEST.into_response();
                    }
                    if let Some(entries)=&s.listing_entries {return Json(entries.clone()).into_response();}
                    return Json(
                        s.listing
                            .iter()
                            .map(|number| s.pulls[number].clone())
                            .collect::<Vec<_>>(),
                    )
                    .into_response();
                }
                let number = path.rsplit('/').next().unwrap().parse::<i64>().unwrap();
                Json(s.pulls[&number].clone()).into_response()
            },
        ))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, state, task)
}
fn observation_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn github_and_gitlab_poll_queries_skip_only_confirmed_semantic_events() {
    for provider in ["github", "gitlab"] {
        let f = fixture_provider(provider);
        let rule = f.rule.clone();
        // Each provider uses its own dedicated native root.
        let mut j = journal(&f);
        j.install(&rule, None).unwrap();
        let (url, state, task) = poll_fake(provider, &f.commit).await;
        let gh = agentscm::hub::github::Api::new(&url, &url).unwrap();
        let gl = agentscm::hub::gitlab::Api::new(&url).unwrap();
        let api = if provider == "github" {
            ForgeApi::Github(&gh)
        } else {
            ForgeApi::Gitlab(&gl)
        };
        let first = poll_merges(api, "synthetic", &connected(), &mut j, observation_now())
            .await
            .unwrap();
        assert!(first.blocked.is_none());
        assert!(first.checkpoint_advanced);
        assert_eq!(j.pending(8).unwrap().len(), 1);
        assert_eq!(state.lock().unwrap().requests, vec!["repo", "list", "pull"]);
        state.lock().unwrap().requests.clear();
        let second = poll_merges(
            api,
            "synthetic",
            &connected(),
            &mut j,
            observation_now() + 1,
        )
        .await
        .unwrap();
        assert_eq!(second.skipped_confirmed, 1);
        assert!(second.observations.is_empty());
        assert_eq!(state.lock().unwrap().requests, vec!["repo", "list"]);
        // A listing with the known SHA but a different number cannot authorize work.
        {
            let mut s = state.lock().unwrap();
            s.requests.clear();
            s.listing.push(8);
            s.pulls
                .insert(8, raw_poll_pull(provider, 8, &f.commit, false));
        }
        let late = poll_merges(
            api,
            "synthetic",
            &connected(),
            &mut j,
            observation_now() + 2,
        )
        .await
        .unwrap();
        assert!(late.blocked.is_none());
        assert_eq!(j.pending(8).unwrap().len(), 1);
        assert_eq!(state.lock().unwrap().requests, vec!["repo", "list", "pull"]);
        state.lock().unwrap().requests.clear();
        let repeated_unmerged = poll_merges(
            api,
            "synthetic",
            &connected(),
            &mut j,
            observation_now() + 3,
        )
        .await
        .unwrap();
        assert!(repeated_unmerged.blocked.is_none());
        assert_eq!(j.pending(8).unwrap().len(), 1);
        assert_eq!(state.lock().unwrap().requests, vec!["repo", "list", "pull"]);
        // Older/same updated_at is not a cutoff: closed-unmerged may later merge.
        {
            let mut s = state.lock().unwrap();
            s.requests.clear();
            s.pulls
                .insert(8, raw_poll_pull(provider, 8, &f.commit, true));
        }
        poll_merges(
            api,
            "synthetic",
            &connected(),
            &mut j,
            observation_now() + 3,
        )
        .await
        .unwrap();
        assert_eq!(j.pending(8).unwrap().len(), 2);
        assert!(!state.lock().unwrap().bad_query);
        task.abort();
    }
}

#[tokio::test]
async fn polling_reports_typed_failures_without_consuming_successful_checkpoint() {
    for provider in ["github", "gitlab"] {
        for (stage, status, kind) in [
            ("repo", 401, "authentication"),
            ("list", 429, "rate_limited"),
            ("pull", 503, "unavailable"),
        ] {
            let f = fixture_provider(provider);
            let rule = f.rule.clone();
            let mut j = journal(&f);
            j.install(&rule, None).unwrap();
            let (url, state, task) = poll_fake(provider, &f.commit).await;
            let gh = agentscm::hub::github::Api::new(&url, &url).unwrap();
            let gl = agentscm::hub::gitlab::Api::new(&url).unwrap();
            let api = if provider == "github" {
                ForgeApi::Github(&gh)
            } else {
                ForgeApi::Gitlab(&gl)
            };
            poll_merges(api, "synthetic", &connected(), &mut j, observation_now())
                .await
                .unwrap();
            let before = j.polls().unwrap();
            {
                let mut s = state.lock().unwrap();
                s.failure = Some((stage.into(), status));
                s.listing.push(8);
                s.pulls
                    .insert(8, raw_poll_pull(provider, 8, &f.commit, true));
                s.requests.clear();
            }
            let failed = poll_merges(
                api,
                "synthetic",
                &connected(),
                &mut j,
                observation_now() + 1000,
            )
            .await
            .unwrap();
            let blocked = failed.blocked.unwrap();
            assert_eq!(blocked.kind, kind);
            assert!(!failed.checkpoint_advanced);
            assert_eq!(j.polls().unwrap(), before);
            if status == 429 {
                assert!(blocked.retry_at.is_some());
            }
            assert_eq!(j.pending(8).unwrap().len(), 1);
            task.abort();
        }
    }
}

#[tokio::test]
async fn partial_poll_retains_confirmed_rows_but_not_the_successful_checkpoint() {
    for provider in ["github", "gitlab"] {
        let f = fixture_provider(provider);
        let mut j = journal(&f);
        j.install(&f.rule, None).unwrap();
        let (url, state, task) = poll_fake(provider, &f.commit).await;
        let gh = agentscm::hub::github::Api::new(&url, &url).unwrap();
        let gl = agentscm::hub::gitlab::Api::new(&url).unwrap();
        let api = if provider == "github" {
            ForgeApi::Github(&gh)
        } else {
            ForgeApi::Gitlab(&gl)
        };
        poll_merges(api, "synthetic", &connected(), &mut j, observation_now())
            .await
            .unwrap();
        let before = j.polls().unwrap();
        {
            let mut s = state.lock().unwrap();
            s.listing.extend([8, 9]);
            s.pulls
                .insert(8, raw_poll_pull(provider, 8, &f.commit, true));
            s.pulls
                .insert(9, raw_poll_pull(provider, 9, &f.commit, true));
            s.failure = Some(("pull:9".into(), 503));
        }
        let partial = poll_merges(
            api,
            "synthetic",
            &connected(),
            &mut j,
            observation_now() + 1000,
        )
        .await
        .unwrap();
        assert_eq!(partial.blocked.unwrap().kind, "unavailable");
        assert_eq!(partial.observations.len(), 1);
        assert!(!partial.checkpoint_advanced);
        assert_eq!(j.pending(8).unwrap().len(), 2);
        assert_eq!(j.polls().unwrap(), before);
        {
            let mut s = state.lock().unwrap();
            s.failure = None;
            s.requests.clear();
        }
        let resumed = poll_merges(
            api,
            "synthetic",
            &connected(),
            &mut j,
            observation_now() + 2000,
        )
        .await
        .unwrap();
        assert!(resumed.blocked.is_none());
        assert_eq!(resumed.skipped_confirmed, 2);
        assert_eq!(state.lock().unwrap().requests, vec!["repo", "list", "pull"]);
        assert_eq!(j.pending(8).unwrap().len(), 3);
        task.abort();
    }
}

#[test]
fn preflight_executable_restore_recovers_the_same_pipeline_run() {
    let mut f = fixture();
    let executable = f.root.path().join("operator-command");
    fs::write(&executable, "#!/bin/sh\n/bin/cat\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let mut profile: ActionProfile =
        serde_json::from_slice(&fs::read(&f.rule.profile_path).unwrap()).unwrap();
    profile.commands.get_mut("echo").unwrap().executable = executable.clone();
    f.rule.profile_digest = profile.digest().unwrap();
    fs::write(&f.rule.profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let d = j.accept(&event(&f)).unwrap().remove(0);
    let plan = compile_source(&f.data, &f.rule, &f.commit).unwrap();
    skill_workflow::start(&f.data, &d.run_id, &plan).unwrap();
    let saved = f.root.path().join("saved-command");
    fs::rename(&executable, &saved).unwrap();
    let claim = j.claim(&d.dispatch_id).unwrap();
    assert!(deliver(&f.data, &mut j, &claim, &f.work).is_err());
    assert_eq!(j.dispatch(&d.dispatch_id).unwrap().state, "blocked");
    assert_eq!(
        skill_workflow::status(&f.data, &d.run_id).unwrap().steps[0].state,
        skill_workflow::StepState::Pending
    );
    fs::rename(&saved, &executable).unwrap();
    j.recover(&d.dispatch_id, claim.generation).unwrap();
    let claim = j.claim(&d.dispatch_id).unwrap();
    let run = deliver(&f.data, &mut j, &claim, &f.work).unwrap();
    assert_eq!(run.run_id, d.run_id);
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(j.dispatch(&d.dispatch_id).unwrap().state, "complete");
}

#[test]
fn terminal_failed_and_cancelled_pipeline_runs_stay_blocked_without_effect_replay() {
    for cancel in [false, true] {
        let mut f = fixture();
        let executable = f.root.path().join("operator-command");
        let marker = f.root.path().join("attempts");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\n/bin/cat >/dev/null\n{}\nexit 1\n",
                marker.display(),
                if cancel { "/bin/sleep 30" } else { "" }
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut profile: ActionProfile =
            serde_json::from_slice(&fs::read(&f.rule.profile_path).unwrap()).unwrap();
        profile.commands.get_mut("echo").unwrap().executable = executable;
        profile.timeout_ms = 10000;
        f.rule.profile_digest = profile.digest().unwrap();
        fs::write(&f.rule.profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
        let mut j = journal(&f);
        j.install(&f.rule, None).unwrap();
        let d = j.accept(&event(&f)).unwrap().remove(0);
        let claim = j.claim(&d.dispatch_id).unwrap();
        if cancel {
            let token = agentscm::execution::Cancellation::default();
            let triggered = token.clone();
            let marker = marker.clone();
            let canceller = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !marker.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                triggered.cancel();
            });
            deliver_with_cancellation(&f.data, &mut j, &claim, &f.work, &token).unwrap();
            canceller.join().unwrap();
        } else {
            assert_eq!(
                deliver(&f.data, &mut j, &claim, &f.work).unwrap().state,
                RunState::Failed
            );
        }
        let blocked = j.dispatch(&d.dispatch_id).unwrap();
        assert_eq!(blocked.state, "blocked");
        assert!(
            blocked
                .reason
                .unwrap()
                .contains(if cancel { "cancelled" } else { "failed" })
        );
        assert_eq!(fs::read(&marker).unwrap(), b"x");
        j.recover(&d.dispatch_id, claim.generation).unwrap();
        let retry = j.claim(&d.dispatch_id).unwrap();
        let run = deliver(&f.data, &mut j, &retry, &f.work).unwrap();
        assert_ne!(run.state, RunState::Succeeded);
        assert_eq!(run.run_id, d.run_id);
        assert_eq!(fs::read(&marker).unwrap(), b"x");
        assert_eq!(j.dispatch(&d.dispatch_id).unwrap().state, "blocked");
    }
}

#[test]
fn watch_cli_help_declares_the_omitted_interval_as_three_hundred_seconds() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(["pipeline", "watch", "--help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8(output.stdout).unwrap();
    let option = help
        .lines()
        .skip_while(|line| !line.contains("--interval-seconds"))
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        option.contains("[default: 300]"),
        "unexpected interval default: {option}"
    );
}

#[tokio::test]
async fn direct_and_polled_merge_reads_require_their_own_target_repository_identity() {
    for provider in ["github", "gitlab"] {
        let f = fixture_provider(provider);
        let mut j = journal(&f);
        j.install(&f.rule, None).unwrap();
        for mismatch in ["id", "missing", "name"] {
            if provider == "gitlab" && mismatch == "name" {
                continue;
            }
            let (url, state, task) = poll_fake(provider, &f.commit).await;
            {
                let mut s = state.lock().unwrap();
                let pull = s.pulls.get_mut(&7).unwrap();
                if provider == "github" {
                    match mismatch {
                        "id" => pull["base"]["repo"]["id"] = json!(43),
                        "name" => pull["base"]["repo"]["full_name"] = json!("other/widgets"),
                        _ => {
                            pull["base"].as_object_mut().unwrap().remove("repo");
                        }
                    }
                } else if mismatch == "id" {
                    pull["target_project_id"] = json!(43);
                } else {
                    pull.as_object_mut().unwrap().remove("target_project_id");
                }
            }
            let gh = agentscm::hub::github::Api::new(&url, &url).unwrap();
            let gl = agentscm::hub::gitlab::Api::new(&url).unwrap();
            let api = if provider == "github" {
                ForgeApi::Github(&gh)
            } else {
                ForgeApi::Gitlab(&gl)
            };
            assert!(
                confirm_merge(api, "synthetic", &connected(), 7, observation_now(), None)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("target repository")
            );
            state.lock().unwrap().requests.clear();
            let report = poll_merges(api, "synthetic", &connected(), &mut j, observation_now())
                .await
                .unwrap();
            assert_eq!(report.blocked.unwrap().kind, "unverifiable");
            assert!(!report.checkpoint_advanced);
            assert!(j.polls().unwrap().is_empty());
            assert!(j.pending(8).unwrap().is_empty());
            assert_eq!(state.lock().unwrap().requests, vec!["repo", "list", "pull"]);
            task.abort();
        }
    }
}

#[tokio::test]
async fn unverifiable_items_do_not_starve_later_valid_merges_or_advance_checkpoint() {
    for provider in ["github", "gitlab"] {
        let f = fixture_provider(provider);
        let mut j = journal(&f);
        j.install(&f.rule, None).unwrap();
        j.record_poll(
            &format!("{provider}://forge.invalid/acme/widgets#42"),
            123,
            0,
        )
        .unwrap();
        let before = j.polls().unwrap();
        let (url, state, task) = poll_fake(provider, &f.commit).await;
        let mut bad = raw_poll_pull(provider, 7, &f.commit, true);
        let good = raw_poll_pull(provider, 8, &f.commit, true);
        if provider == "github" {
            bad["base"]["repo"]["id"] = json!(43);
        } else {
            bad["merge_commit_sha"] = Value::Null;
            bad["squash_commit_sha"] = Value::Null;
            bad["sha"] = json!("a".repeat(40));
        }
        {
            let mut s = state.lock().unwrap();
            s.pulls.insert(7, bad.clone());
            s.pulls.insert(8, good.clone());
            s.target_tip = Some("b".repeat(40));
            s.listing_entries = Some(vec![
                bad.clone(),
                json!({"malformed":"no proposal identity"}),
                good.clone(),
            ]);
        }
        let gh = agentscm::hub::github::Api::new(&url, &url).unwrap();
        let gl = agentscm::hub::gitlab::Api::new(&url).unwrap();
        let api = if provider == "github" {
            ForgeApi::Github(&gh)
        } else {
            ForgeApi::Gitlab(&gl)
        };
        let report = poll_merges(api, "synthetic", &connected(), &mut j, observation_now())
            .await
            .unwrap();
        assert!(report.blocked.is_some());
        assert!(!report.checkpoint_advanced);
        assert_eq!(report.observations.len(), 3);
        assert_eq!(report.observations[0]["proposal"], 7);
        assert_eq!(report.observations[0]["accepted"], json!([]));
        assert!(report.observations[0]["blocked"].is_string());
        assert_eq!(report.observations[1]["listing_index"], 1);
        assert!(report.observations[1].get("proposal").is_none());
        assert_eq!(report.observations[1]["accepted"], json!([]));
        assert_eq!(report.observations[2]["proposal"], 8);
        assert_eq!(
            report.observations[2]["accepted"].as_array().unwrap().len(),
            1
        );
        let blocked = report.blocked.unwrap();
        assert_eq!(blocked.kind, "unverifiable");
        assert!(blocked.reason.contains(if provider == "github" {
            "target repository"
        } else {
            "fast-forward target may have advanced"
        }));
        assert_eq!(j.polls().unwrap(), before);
        let pending = j.pending(8).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].event.proposal_number, 8);
        let db = rusqlite::Connection::open(f.data.join("pipeline-rules/state.sqlite")).unwrap();
        let events: i64 = db
            .query_row("SELECT count(*) FROM pipeline_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(events, 1);
        drop(db);
        assert_eq!(
            state.lock().unwrap().requests,
            if provider == "github" {
                vec!["repo", "list", "pull", "pull"]
            } else {
                vec!["repo", "list", "pull", "ref", "pull"]
            }
        );
        // A later transport failure supersedes the first per-proposal reason and
        // stops immediately, while the previously accepted item stays retained.
        {
            let mut s = state.lock().unwrap();
            let failed = raw_poll_pull(provider, 9, &f.commit, true);
            s.pulls.insert(9, failed.clone());
            s.listing_entries = Some(vec![bad, good, failed]);
            s.failure = Some(("pull:9".into(), 503));
        }
        let failed = poll_merges(api, "synthetic", &connected(), &mut j, observation_now())
            .await
            .unwrap();
        assert!(failed.blocked.is_some());
        assert!(!failed.checkpoint_advanced);
        assert_eq!(failed.blocked.unwrap().kind, "unavailable");
        assert_eq!(j.polls().unwrap(), before);
        assert_eq!(j.pending(8).unwrap().len(), 1);
        task.abort();
    }
}
