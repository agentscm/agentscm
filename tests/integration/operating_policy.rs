#![cfg(unix)]
//! Synthetic protocol safety cases; explicit ignored qualification invokes an installed native client.
use agentscm::authority::Authority;
use agentscm::hub::pipeline_rules::*;
use agentscm::operating_policy::*;
use agentscm::skill_workflow::action::{ActionProfile, CommandProfile};
use agentscm::skill_workflow::{self, RunState};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    _root: TempDir,
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
        _root: root,
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

const ACTOR: &str = "github:forge.invalid:7";
fn contract(f: &Fixture) -> OperatingContract {
    let profile = read_profile(&f.rule).unwrap();
    OperatingContract {
        schema_version:1,rule_id:f.rule.rule_id.clone(),original_intent:"After a ship-labelled merge, produce the installed release artifact; never publish or merge".into(),
        rule_digest:rule_digest(&f.rule).unwrap(),eligible_actors:BTreeSet::from([ACTOR.into()]),
        action:SupportedAction::ProductiveArtifacts,required_evidence:RequiredEvidence::CurrentProfileAndWorkflowDependencies,escalation:Escalation::RetainForOwner,max_dispatches:1,max_action_runtime_ms:profile.timeout_ms,max_artifact_bytes:profile.max_output_bytes,unresolved:vec![],
    }
}
fn context() -> EvaluationContext {
    EvaluationContext {
        actor: ACTOR.into(),
        authority_current: Some(true),
        bindings_current: Some(true),
        prerequisites_passed: Some(true),
        remaining_dispatches: 1,
        duplicate: false,
    }
}
fn cases(f: &Fixture) -> Vec<Scenario> {
    let mut cases = vec![Scenario {
        name: "matching event".into(),
        event: event(f),
        context: context(),
        expected: Decision::Run,
    }];
    let mut actor = context();
    actor.actor = "github:forge.invalid:999".into();
    cases.push(Scenario {
        name: "unlisted actor".into(),
        event: event(f),
        context: actor,
        expected: Decision::Deny,
    });
    let mut missing = context();
    missing.prerequisites_passed = None;
    cases.push(Scenario {
        name: "unknown prerequisite".into(),
        event: event(f),
        context: missing,
        expected: Decision::Unknown,
    });
    for (name, mut context, expected) in [
        ("revoked authority", context(), Decision::Deny),
        ("changed source binding", context(), Decision::Deny),
        ("failed prerequisite", context(), Decision::Deny),
        ("duplicate event", context(), Decision::Ignore),
        ("exhausted budget", context(), Decision::Deny),
    ] {
        match name {
            "revoked authority" => context.authority_current = Some(false),
            "changed source binding" => context.bindings_current = Some(false),
            "failed prerequisite" => context.prerequisites_passed = Some(false),
            "duplicate event" => context.duplicate = true,
            _ => context.remaining_dispatches = 0,
        }
        cases.push(Scenario {
            name: name.into(),
            event: event(f),
            context,
            expected,
        });
    }
    cases
}
fn activated(f: &Fixture, journal: &mut RuleJournal) -> PolicyView {
    journal.install(&f.rule, None).unwrap();
    let p = journal
        .review_operating_policy(&contract(f), ACTOR, None)
        .unwrap();
    assert_eq!(p.mode, "observe");
    let (digest, report) = journal
        .rehearse_operating_policy(&f.rule.rule_id, cases(f))
        .unwrap();
    assert!(report.passed);
    assert!(
        journal
            .set_operating_mode(
                &f.rule.rule_id,
                &p.digest,
                p.generation,
                "active",
                Some(&digest),
                ACTOR
            )
            .is_err()
    );
    assert!(
        journal
            .accept_operating_event(&f.rule.rule_id, &event(f), ACTOR)
            .unwrap()
            .is_empty()
    );
    journal
        .set_operating_mode(
            &f.rule.rule_id,
            &p.digest,
            p.generation,
            "active",
            Some(&digest),
            ACTOR,
        )
        .unwrap()
}

#[test]
fn shared_evaluator_keeps_unknowns_and_rejects_scope_authority_budget_and_duplicates() {
    let f = fixture();
    let rule = &f.rule;
    let c = contract(&f);
    let e = event(&f);
    assert_eq!(evaluate(&c, rule, &e, &context()).decision, Decision::Run);
    let mut inputs = context();
    inputs.remaining_dispatches = 0;
    assert_eq!(
        evaluate(&c, rule, &e, &inputs).reasons,
        ["dispatch_budget_exhausted"]
    );
    inputs = context();
    inputs.duplicate = true;
    assert_eq!(evaluate(&c, rule, &e, &inputs).decision, Decision::Ignore);
    for field in [0, 1, 2] {
        let mut inputs = context();
        match field {
            0 => inputs.authority_current = Some(false),
            1 => inputs.bindings_current = Some(false),
            _ => inputs.prerequisites_passed = Some(false),
        }
        assert_eq!(evaluate(&c, rule, &e, &inputs).decision, Decision::Deny);
    }
    let mut other = e;
    other.target = "unreviewed".into();
    assert_eq!(
        evaluate(&c, rule, &other, &context()).decision,
        Decision::Ignore
    );
    let mut ambiguous = c;
    ambiguous
        .unresolved
        .push("requested merge is unsupported".into());
    assert_eq!(
        evaluate(&ambiguous, rule, &event(&f), &context()).decision,
        Decision::Deny
    );
    let mut unknown = serde_json::to_value(&ambiguous).unwrap();
    unknown["shell"] = json!("arbitrary");
    assert!(serde_json::from_value::<OperatingContract>(unknown).is_err());
}

#[test]
fn exact_activation_observe_first_budget_reservation_and_existing_runtime_produce_one_artifact() {
    let f = fixture();
    let mut j = journal(&f);
    let p = activated(&f, &mut j);
    assert!(j.accept(&event(&f)).unwrap().is_empty());
    let mut change = f.rule.clone();
    change.generation += 1;
    assert!(j.install(&change, Some(1)).is_err());
    let mut e = event(&f);
    e.proposal_number += 1;
    let accepted = j
        .accept_operating_event(&f.rule.rule_id, &e, ACTOR)
        .unwrap();
    assert_eq!(accepted.len(), 1);
    assert!(
        j.accept_operating_event(&f.rule.rule_id, &e, ACTOR)
            .unwrap()
            .is_empty()
    );
    e.proposal_number += 1;
    assert!(
        j.accept_operating_event(&f.rule.rule_id, &e, ACTOR)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        j.operating_policy(&f.rule.rule_id)
            .unwrap()
            .reserved_dispatches,
        1
    );
    drop(j);
    let mut j = journal(&f);
    let claim = j.claim(&accepted[0].dispatch_id).unwrap();
    assert!(deliver(&f.data, &mut j, &claim, &f.work).is_err());
    j.recover(&claim.dispatch.dispatch_id, claim.generation)
        .unwrap();
    let claim = j.claim(&claim.dispatch.dispatch_id).unwrap();
    let count = AtomicUsize::new(0);
    let guard = || {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    };
    let run = deliver_with_operating_guard(
        &f.data,
        &mut j,
        &claim,
        &f.work,
        &agentscm::execution::Cancellation::default(),
        &guard,
    )
    .unwrap();
    assert_eq!(run.state, RunState::Succeeded);
    assert!(count.load(Ordering::SeqCst) > 1);
    let artifact =
        skill_workflow::runtime::artifact(&f.data, &run.run_id, "draft", "draft").unwrap();
    let value: Value = serde_json::from_slice(&artifact).unwrap();
    assert_eq!(value["inputs"]["app"], "initial application");
    assert_eq!(
        fs::read_to_string(f.source.join("app.txt")).unwrap(),
        "developer edits"
    );
    assert!(
        j.set_operating_mode(
            &f.rule.rule_id,
            &p.digest,
            p.generation - 1,
            "paused",
            None,
            ACTOR
        )
        .is_err()
    );
    let paused = j
        .set_operating_mode(
            &f.rule.rule_id,
            &p.digest,
            p.generation,
            "paused",
            None,
            ACTOR,
        )
        .unwrap();
    assert_eq!(paused.mode, "paused");
}

#[test]
fn revocation_at_action_boundary_blocks_effect_and_keeps_reservation() {
    let f = fixture();
    let mut j = journal(&f);
    let p = activated(&f, &mut j);
    let mut e = event(&f);
    e.proposal_number += 1;
    let accepted = j
        .accept_operating_event(&f.rule.rule_id, &e, ACTOR)
        .unwrap();
    let claim = j.claim(&accepted[0].dispatch_id).unwrap();
    let revoked = j
        .set_operating_mode(
            &f.rule.rule_id,
            &p.digest,
            p.generation,
            "revoked",
            None,
            ACTOR,
        )
        .unwrap();
    assert!(
        deliver_with_operating_guard(
            &f.data,
            &mut j,
            &claim,
            &f.work,
            &agentscm::execution::Cancellation::default(),
            &|| Ok(())
        )
        .is_err()
    );
    assert_eq!(
        j.dispatch(&claim.dispatch.dispatch_id).unwrap().state,
        "blocked"
    );
    assert_eq!(
        j.operating_policy(&f.rule.rule_id)
            .unwrap()
            .reserved_dispatches,
        1
    );
    assert!(
        j.set_operating_mode(
            &f.rule.rule_id,
            &revoked.digest,
            revoked.generation,
            "active",
            revoked.rehearsal.as_deref(),
            ACTOR
        )
        .is_err()
    );
    let mut amended = contract(&f);
    amended.original_intent.push_str(" Updated scope.");
    let v2 = j
        .review_operating_policy(&amended, ACTOR, Some(revoked.generation))
        .unwrap();
    assert_eq!(v2.mode, "observe");
    assert_eq!(v2.version, 2);
    assert!(
        j.set_operating_mode(
            &f.rule.rule_id,
            &v2.digest,
            v2.generation,
            "active",
            revoked.rehearsal.as_deref(),
            ACTOR
        )
        .is_err()
    );
}

#[test]
fn drafting_reservations_do_not_retry_lost_native_responses() {
    let f = fixture();
    let mut j = journal(&f);
    let digest = "a".repeat(64);
    assert!(
        j.reserve_operating_draft("request", &digest)
            .unwrap()
            .is_none()
    );
    assert!(j.reserve_operating_draft("request", &digest).is_err());
    assert!(
        j.reserve_operating_draft("request", &"b".repeat(64))
            .is_err()
    );
    let response = json!({"native_session_id":"actual-parser-evidence","contract":contract(&f)});
    j.finish_operating_draft("request", &digest, &response)
        .unwrap();
    assert_eq!(
        j.reserve_operating_draft("request", &digest).unwrap(),
        Some(response)
    );
}

#[test]
fn old_pipeline_journals_migrate_without_replaying_or_changing_rules() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    drop(j);
    let path = f.data.join("pipeline-rules/state.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    for table in [
        "operating_drafts",
        "operating_reservations",
        "operating_decisions",
        "operating_rehearsals",
        "operating_state",
        "operating_versions",
    ] {
        db.execute(&format!("DROP TABLE {table}"), []).unwrap();
    }
    db.pragma_update(None, "user_version", 1).unwrap();
    drop(db);
    let j = journal(&f);
    assert_eq!(j.rules().unwrap(), vec![f.rule.clone()]);
    assert!(j.pending(10).unwrap().is_empty());
}

#[test]
fn missing_and_changed_live_prerequisite_profiles_retain_unknown_and_denied_decisions() {
    for missing in [true, false] {
        let f = fixture();
        let mut j = journal(&f);
        activated(&f, &mut j);
        if missing {
            fs::remove_file(&f.rule.profile_path).unwrap();
        } else {
            let mut profile: ActionProfile =
                serde_json::from_slice(&fs::read(&f.rule.profile_path).unwrap()).unwrap();
            profile.timeout_ms += 1;
            fs::write(&f.rule.profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
        }
        let mut next = event(&f);
        next.proposal_number += 1;
        assert!(
            j.accept_operating_event(&f.rule.rule_id, &next, ACTOR)
                .unwrap()
                .is_empty()
        );
        let history = j.operating_history(&f.rule.rule_id).unwrap();
        assert_eq!(
            history["decisions"][0]["trace"]["decision"],
            if missing { "unknown" } else { "deny" }
        );
        assert_eq!(
            history["decisions"][0]["trace"]["reasons"][0],
            "prerequisites"
        );
        assert_eq!(
            j.inspect_operating_rule(&f.rule.rule_id).unwrap()["profile_current"],
            false
        );
        assert_eq!(
            j.operating_policy(&f.rule.rule_id)
                .unwrap()
                .reserved_dispatches,
            0
        );
    }
}

#[test]
fn false_rehearsal_results_and_corrupt_retained_versions_cannot_activate_or_reopen() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let p = j
        .review_operating_policy(&contract(&f), ACTOR, None)
        .unwrap();
    let mut corpus = cases(&f);
    corpus[0].expected = Decision::Deny;
    let (rehearsal, report) = j
        .rehearse_operating_policy(&f.rule.rule_id, corpus)
        .unwrap();
    assert!(!report.passed);
    j.accept_operating_event(&f.rule.rule_id, &event(&f), ACTOR)
        .unwrap();
    assert!(
        j.set_operating_mode(
            &f.rule.rule_id,
            &p.digest,
            p.generation,
            "active",
            Some(&rehearsal),
            ACTOR
        )
        .is_err()
    );
    assert!(
        j.rehearse_operating_policy(&f.rule.rule_id, cases(&f)[..3].to_vec())
            .is_err()
    );
    let mut substituted = cases(&f);
    let revoked = substituted
        .iter_mut()
        .find(|s| s.name == "revoked authority")
        .unwrap();
    revoked.context.authority_current = None;
    revoked.expected = Decision::Unknown;
    assert!(
        j.rehearse_operating_policy(&f.rule.rule_id, substituted)
            .is_err()
    );
    drop(j);
    let path = f.data.join("pipeline-rules/state.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE operating_versions SET body=replace(body,'After a ship-labelled','After every')",
        [],
    )
    .unwrap();
    drop(db);
    assert!(RuleJournal::open(&path).is_err());
}

#[test]
fn declared_workflow_prerequisite_failure_prevents_dependent_productive_action() {
    let mut f = fixture();
    let prerequisite=serde_json::from_value(json!({"id":"source-check","needs":[],"required":true,"action":{"kind":"source_paths","expectations":[{"path":"app.txt","sha256":"0".repeat(64)}]}})).unwrap();
    f.rule.recipe.steps[0].needs = vec!["source-check".into()];
    f.rule.recipe.steps.insert(0, prerequisite);
    f.rule.recipe_digest = f.rule.recipe.digest().unwrap();
    f.rule.skill_digest = compile_source(&f.data, &f.rule, &f.commit)
        .unwrap()
        .skill_digest()
        .into();
    let mut j = journal(&f);
    activated(&f, &mut j);
    let mut next = event(&f);
    next.proposal_number += 1;
    let dispatch = j
        .accept_operating_event(&f.rule.rule_id, &next, ACTOR)
        .unwrap()
        .remove(0);
    let claim = j.claim(&dispatch.dispatch_id).unwrap();
    let run = deliver_with_operating_guard(
        &f.data,
        &mut j,
        &claim,
        &f.work,
        &agentscm::execution::Cancellation::default(),
        &|| Ok(()),
    )
    .unwrap();
    assert_ne!(run.state, RunState::Succeeded);
    assert!(skill_workflow::runtime::artifact(&f.data, &run.run_id, "draft", "draft").is_err());
    let db = rusqlite::Connection::open(f.data.join("skill-workflows/state.sqlite")).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM workflow_action_attempts", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn revocation_during_execution_cancels_the_child_and_recovery_does_not_repeat_it() {
    let mut f = fixture();
    let executable = f._root.path().join("operator-command");
    let marker = f._root.path().join("attempts");
    let pid_file = f._root.path().join("running-pid");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nprintf x >> '{}'\n/bin/cat >/dev/null\n/bin/sleep 30\nprintf '{{}}'\n",
            pid_file.display(), marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let mut profile = read_profile(&f.rule).unwrap();
    profile.commands.get_mut("echo").unwrap().executable = executable;
    profile.timeout_ms = 10000;
    f.rule.profile_digest = profile.digest().unwrap();
    fs::write(&f.rule.profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let mut j = journal(&f);
    let p = activated(&f, &mut j);
    let mut next = event(&f);
    next.proposal_number += 1;
    let dispatch = j
        .accept_operating_event(&f.rule.rule_id, &next, ACTOR)
        .unwrap()
        .remove(0);
    let claim = j.claim(&dispatch.dispatch_id).unwrap();
    let db_path = f.data.join("pipeline-rules/state.sqlite");
    let seen = marker.clone();
    let id = f.rule.rule_id.clone();
    let canceller = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !seen.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(seen.exists());
        let mut journal = RuleJournal::open(&db_path).unwrap();
        journal
            .set_operating_mode(&id, &p.digest, p.generation, "revoked", None, ACTOR)
            .unwrap();
    });
    let result = deliver_with_operating_guard(
        &f.data,
        &mut j,
        &claim,
        &f.work,
        &agentscm::execution::Cancellation::default(),
        &|| Ok(()),
    );
    canceller.join().unwrap();
    match result {
        Ok(run) => assert_ne!(run.state, RunState::Succeeded),
        Err(agentscm::Error::Conflict(reason)) => {
            assert!(reason.contains("operating action refused: authority"))
        }
        Err(error) => panic!("unexpected cancellation failure: {error}"),
    }
    let pid: i32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    // kill(0) checks existence only; it sends no signal and uses no pointer.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "revoked native child still exists"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert_eq!(j.dispatch(&dispatch.dispatch_id).unwrap().state, "blocked");
    assert_eq!(fs::read(&marker).unwrap(), b"x");
    j.recover(&dispatch.dispatch_id, claim.generation).unwrap();
    let retry = j.claim(&dispatch.dispatch_id).unwrap();
    assert!(
        deliver_with_operating_guard(
            &f.data,
            &mut j,
            &retry,
            &f.work,
            &agentscm::execution::Cancellation::default(),
            &|| Ok(())
        )
        .is_err()
    );
    assert_eq!(fs::read(&marker).unwrap(), b"x");
    assert!(
        !j.operating_history(&f.rule.rule_id).unwrap()["decisions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

struct LocalHub {
    url: String,
    token: PathBuf,
    token_id: String,
    store: std::sync::Arc<dyn agentscm::hub::store::Store>,
    runtime: tokio::runtime::Runtime,
    _directory: TempDir,
}
impl LocalHub {
    fn start(f: &Fixture) -> Self {
        use agentscm::hub::{HubState, router, store};
        let directory = tempfile::tempdir().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            directory.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let objects =
            agentscm::backend::LocalBackend::open(directory.path().join("objects")).unwrap();
        let owner = store
            .upsert_user(
                &store::ForgeIdentity {
                    provider: "github".into(),
                    host: f.rule.host.clone(),
                    user_id: 7,
                },
                "synthetic",
            )
            .unwrap();
        let (token_id, secret, _) = store::mint_bound_agent_token(
            &*store,
            &owner.id,
            "operating-test",
            &f.rule.repository,
            f.rule.provider_repository_id,
            3600,
            false,
        )
        .unwrap();
        let token = directory.path().join("agent.token");
        fs::write(&token, secret.expose()).unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let app = router(HubState::new(store.clone(), std::sync::Arc::new(objects)));
        let address = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            address
        });
        Self {
            url: format!("http://{address}"),
            token,
            token_id,
            store,
            runtime,
            _directory: directory,
        }
    }
    fn cli(&self, f: &Fixture, args: &[&str], native: bool) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        if !native {
            command
                .env_clear()
                .env("HOME", self._directory.path())
                .env("XDG_CONFIG_HOME", self._directory.path());
        }
        command
            .current_dir(&f.source)
            .arg("--data-dir")
            .arg(&f.data)
            .arg("operating-policy")
            .args(args)
            .env("AGENTSCM_HUB_URL", &self.url)
            .env("AGENTSCM_HUB_TOKEN_FILE", &self.token)
            .output()
            .unwrap()
    }
    fn success(&self, f: &Fixture, args: &[&str], native: bool) -> Value {
        let out = self.cli(f, args, native);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn actual_hub_cli_run_keeps_one_work_and_revoked_pending_run_creates_none() {
    for revoke in [false, true] {
        let f = fixture();
        let hub = LocalHub::start(&f);
        let mut j = journal(&f);
        let p = activated(&f, &mut j);
        let mut next = event(&f);
        next.proposal_number += 1;
        let dispatch = j
            .accept_operating_event(&f.rule.rule_id, &next, ACTOR)
            .unwrap()
            .remove(0);
        if revoke {
            j.set_operating_mode(
                &f.rule.rule_id,
                &p.digest,
                p.generation,
                "revoked",
                None,
                ACTOR,
            )
            .unwrap();
            assert!(
                !hub.cli(&f, &["run", &dispatch.dispatch_id], false)
                    .status
                    .success()
            );
        } else {
            let result = hub.success(&f, &["run", &dispatch.dispatch_id], false);
            assert_eq!(result["checkpoint_recorded"], true, "{result}");
            assert_eq!(result["execution"]["run"]["state"], "succeeded", "{result}");
            let replay = hub.success(&f, &["run", &dispatch.dispatch_id], false);
            assert_eq!(replay["replayed"], true);
            let binding = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
            assert_eq!(binding.work.state, agentscm::hub::work::WorkState::Finished);
        }
        let page = hub
            .store
            .work_search(
                &hub.token_id,
                None,
                None,
                100,
                agentscm::hub::store::unix_now(),
            )
            .unwrap();
        assert_eq!(page.items.len(), usize::from(!revoke));
    }
}

#[test]
#[ignore = "explicit existing-subscription qualification: two native drafts and one real Claude productive action"]
fn live_native_drafting_and_confirmed_event_run_existing_productive_executor() {
    use agentscm::hub::{connection::ConnectedRepository, forge_api::ForgeApi};
    let executable = std::env::var_os("AGENTSCM_TEST_NATIVE_CLAUDE")
        .map(PathBuf::from)
        .expect("set AGENTSCM_TEST_NATIVE_CLAUDE to installed Claude Code executable; no fallback");
    assert!(executable.is_absolute());
    let mut f = fixture();
    let mut profile = read_profile(&f.rule).unwrap();
    profile.commands.clear();
    profile.claude_executable = Some(executable.clone());
    profile.model = Some(
        std::env::var("AGENTSCM_TEST_NATIVE_CLAUDE_MODEL")
            .expect("set an explicitly authorized installed model; no fallback"),
    );
    profile.timeout_ms = 120000;
    profile.max_output_bytes = 8192;
    f.rule.profile_digest = profile.digest().unwrap();
    fs::write(&f.rule.profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    fs::write(f.source.join("skills/release/prompt.txt"),"Write a concise release briefing from the supplied app and merge inputs. Include the exact accepted commit and the application description. Explain that this briefing is retained locally for owner review. Do not publish or claim tests ran. Return plain text only.").unwrap();
    fs::write(f.source.join("skills/release/draft.json"),serde_json::to_vec(&json!({"version":1,"inputs":{"app":{"kind":"source","path":"app.txt"},"merge":{"kind":"event","field":"accepted_commit"}},"output":{"name":"draft","media_type":"text/plain","max_bytes":8192},"action":{"kind":"prompt","prompt":"prompt.txt","required_tools":[]}})).unwrap()).unwrap();
    f.rule
        .recipe
        .skill
        .resources
        .push("skills/release/prompt.txt".into());
    f.rule.recipe_digest = f.rule.recipe.digest().unwrap();
    git(&f.rule.git, &f.source, &["add", "skills"]);
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
            "Synthetic productive prompt",
        ],
    );
    f.commit = git(&f.rule.git, &f.source, &["rev-parse", "HEAD"]);
    f.rule.skill_digest = compile_source(&f.data, &f.rule, &f.commit)
        .unwrap()
        .skill_digest()
        .into();
    let hub = LocalHub::start(&f);
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let intent = f._root.path().join("intent.txt");
    fs::write(&intent,"After a ship-labelled merge to main, produce the installed local release briefing once for owner review. Do not publish, merge or alter source.").unwrap();
    let draft = hub.success(
        &f,
        &[
            "draft",
            "release",
            "--intent",
            intent.to_str().unwrap(),
            "--client",
            "claude",
            "--executable",
            executable.to_str().unwrap(),
            "--request-id",
            "qualification-supported",
        ],
        true,
    );
    assert!(!draft["native_session_id"].as_str().unwrap().is_empty());
    let reviewed: OperatingContract = serde_json::from_value(draft["contract"].clone()).unwrap();
    assert!(
        reviewed.unresolved.is_empty(),
        "native client found unsupported intent; owner inspection required"
    );
    let input = f._root.path().join("contract.json");
    fs::write(&input, serde_json::to_vec(&reviewed).unwrap()).unwrap();
    hub.success(&f, &["review", input.to_str().unwrap()], false);
    let (rehearsal, report) = j.rehearse_operating_policy("release", cases(&f)).unwrap();
    assert!(report.passed);
    let (forge_url,server)=hub.runtime.block_on(async {
        use axum::{Json,Router,routing::get};
        let commit=f.commit.clone();
        let app=Router::new().route("/repos/acme/widgets",get(||async{Json(json!({"id":42,"full_name":"acme/widgets"}))})).route("/repos/acme/widgets/pulls/{number}",get(move |axum::extract::Path(number):axum::extract::Path<i64>|{let commit=commit.clone();async move{Json(json!({"number":number,"merged":true,"merge_commit_sha":commit,"base":{"ref":"main","repo":{"id":42,"full_name":"acme/widgets"}},"labels":[{"name":"ship"}]}))}}));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("http://{}",listener.local_addr().unwrap());
        let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});(url,server)
    });
    let api = agentscm::hub::github::Api::new(&forge_url, &forge_url).unwrap();
    let connected = ConnectedRepository {
        id: "synthetic".into(),
        connection: "synthetic".into(),
        host: f.rule.host.clone(),
        repository_id: 42,
        full_name: f.rule.repository.clone(),
        node_id: None,
        installation_id: None,
        pulls_etag: None,
    };
    let observe = hub
        .runtime
        .block_on(confirm_merge(
            ForgeApi::Github(&api),
            "synthetic",
            &connected,
            7,
            123,
            None,
        ))
        .unwrap()
        .unwrap();
    assert!(
        j.accept_operating_event("release", &observe, ACTOR)
            .unwrap()
            .is_empty()
    );
    let p = j.operating_policy("release").unwrap();
    hub.success(
        &f,
        &[
            "activate",
            "release",
            "--digest",
            &p.digest,
            "--rehearsal",
            &rehearsal,
            "--expected-generation",
            &p.generation.to_string(),
        ],
        false,
    );
    let confirmed = hub
        .runtime
        .block_on(confirm_merge(
            ForgeApi::Github(&api),
            "synthetic",
            &connected,
            8,
            124,
            None,
        ))
        .unwrap()
        .unwrap();
    let dispatch = j
        .accept_operating_event("release", &confirmed, ACTOR)
        .unwrap()
        .remove(0);
    assert!(
        j.accept_operating_event("release", &confirmed, ACTOR)
            .unwrap()
            .is_empty()
    );
    let result = hub.success(&f, &["run", &dispatch.dispatch_id], true);
    assert_eq!(result["execution"]["run"]["state"], "succeeded", "{result}");
    assert_eq!(result["checkpoint_recorded"], true, "{result}");
    let artifact =
        skill_workflow::runtime::artifact(&f.data, &dispatch.run_id, "draft", "draft").unwrap();
    let text = String::from_utf8(artifact).unwrap();
    assert!(text.contains(&f.commit));
    assert!(text.to_lowercase().contains("initial application"));
    assert_eq!(
        fs::read_to_string(f.source.join("app.txt")).unwrap(),
        "developer edits"
    );
    hub.success(&f, &["run", &dispatch.dispatch_id], true);
    assert_eq!(
        hub.store
            .work_search(
                &hub.token_id,
                None,
                None,
                100,
                agentscm::hub::store::unix_now()
            )
            .unwrap()
            .items
            .len(),
        1
    );
    fs::write(&intent,"After a ship-labelled merge to main, produce the installed local release briefing and also publish the package to a registry.").unwrap();
    let unsupported = hub.success(
        &f,
        &[
            "draft",
            "release",
            "--intent",
            intent.to_str().unwrap(),
            "--client",
            "claude",
            "--executable",
            executable.to_str().unwrap(),
            "--request-id",
            "qualification-unsupported",
        ],
        true,
    );
    assert!(
        !unsupported["contract"]["unresolved"]
            .as_array()
            .unwrap()
            .is_empty(),
        "unsupported publishing must remain explicit"
    );
    server.abort();
}

#[test]
#[ignore = "explicit HTTPS forge read plus existing-subscription native draft and productive action"]
fn live_https_observe_execute_runs_once_through_the_production_cli() {
    use agentscm::hub::{connection::ConnectedRepository, forge_api::ForgeApi};
    let config_path = std::env::var_os("AGENTSCM_TEST_OPERATING_LIVE_CONFIG")
        .map(PathBuf::from)
        .expect("set explicit live qualification config path");
    let config: Value = serde_json::from_slice(&fs::read(config_path).unwrap()).unwrap();
    let connection_path = PathBuf::from(config["connection"].as_str().unwrap());
    let connection: Value = serde_json::from_slice(&fs::read(&connection_path).unwrap()).unwrap();
    let executable = std::env::var_os("AGENTSCM_TEST_NATIVE_CLAUDE")
        .map(PathBuf::from)
        .expect("set installed Claude Code executable; no fallback");
    let observe = config["observe_proposal"].as_i64().unwrap();
    let execute = config["execute_proposal"].as_i64().unwrap();
    assert_ne!(observe, execute);
    let mut f = fixture();
    f.source = PathBuf::from(config["source_checkout"].as_str().unwrap())
        .canonicalize()
        .unwrap();
    f.rule.git_repository = f.source.clone();
    f.rule.git_identity =
        agentscm::execution::executable::capture(&f.rule.git, &[&f.data, &f.source]).unwrap();
    f.rule.provider = connection["provider"].as_str().unwrap().into();
    assert_eq!(
        f.rule.provider, "github",
        "first external qualification is GitHub"
    );
    f.rule.host = connection["host"].as_str().unwrap().into();
    f.rule.repository = connection["repository"].as_str().unwrap().into();
    f.rule.provider_repository_id = connection["provider_repository_id"].as_i64().unwrap();
    let mut profile: ActionProfile =
        serde_json::from_slice(&fs::read(&f.rule.profile_path).unwrap()).unwrap();
    profile.commands.clear();
    profile.claude_executable = Some(executable.clone());
    profile.model = Some(
        std::env::var("AGENTSCM_TEST_NATIVE_CLAUDE_MODEL")
            .expect("set an explicitly authorized installed model; no fallback"),
    );
    profile.timeout_ms = 120000;
    profile.max_output_bytes = 32768;
    f.rule.profile_digest = profile.digest().unwrap();
    fs::write(&f.rule.profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let hub = LocalHub::start(&f);
    let api = agentscm::hub::github::Api::new(
        connection["api_url"].as_str().unwrap(),
        connection["web_url"].as_str().unwrap(),
    )
    .unwrap();
    let connected = ConnectedRepository {
        id: "operator-local".into(),
        connection: "operator-local".into(),
        host: f.rule.host.clone(),
        repository_id: f.rule.provider_repository_id,
        full_name: f.rule.repository.clone(),
        node_id: None,
        installation_id: None,
        pulls_etag: None,
    };
    let token = std::env::var(connection["token_env"].as_str().unwrap())
        .expect("connection credential environment variable is required");
    let confirmed = hub
        .runtime
        .block_on(confirm_merge(
            ForgeApi::Github(&api),
            &token,
            &connected,
            execute,
            agentscm::hub::store::unix_now() * 1000,
            None,
        ))
        .unwrap()
        .expect("selected external proposal must be merged");
    f.commit = confirmed.accepted_commit.clone();
    f.rule.trigger = MergeTrigger {
        target: confirmed.target.clone(),
        labels_all: BTreeSet::new(),
    };
    f.rule.recipe=serde_json::from_value(json!({"version":1,"name":"release-briefing","skill":{"entrypoint":"examples/productive-workflows/release-notes/v1/SKILL.md","resources":["examples/productive-workflows/release-notes/v1/explain.json","examples/productive-workflows/release-notes/v1/explain.md"]},"steps":[{"id":"explain","needs":[],"required":true,"action":{"kind":"agent_action","resource":"explain.json"}}]})).unwrap();
    f.rule.recipe_digest = f.rule.recipe.digest().unwrap();
    // Recreate only the disposable, still-empty local journal binding; never change source.
    fs::remove_file(f.data.join("pipeline-rules/state.sqlite")).unwrap();
    let mut j = journal(&f);
    j.bind_repository(&f.rule).unwrap();
    f.rule.skill_digest = compile_source(&f.data, &f.rule, &f.commit)
        .unwrap()
        .skill_digest()
        .into();
    j.install(&f.rule, None).unwrap();
    let intent = f._root.path().join("intent.txt");
    fs::write(&intent,"After a confirmed merge to the installed target, run the installed local release briefing workflow once for owner review. Do not publish, merge or alter source.").unwrap();
    let draft = hub.success(
        &f,
        &[
            "draft",
            "release",
            "--intent",
            intent.to_str().unwrap(),
            "--client",
            "claude",
            "--executable",
            executable.to_str().unwrap(),
            "--request-id",
            "https-qualification",
        ],
        true,
    );
    let reviewed: OperatingContract = serde_json::from_value(draft["contract"].clone()).unwrap();
    assert!(reviewed.unresolved.is_empty());
    let input = f._root.path().join("contract.json");
    fs::write(&input, serde_json::to_vec(&reviewed).unwrap()).unwrap();
    hub.success(&f, &["review", input.to_str().unwrap()], true);
    let mut corpus = cases(&f);
    for case in &mut corpus {
        case.event.provider_repository_id = f.rule.provider_repository_id;
        case.event.repository = f.rule.repository.clone();
        case.event.target = f.rule.trigger.target.clone();
        if case.context.actor == ACTOR {
            case.context.actor = format!("github:{}:7", f.rule.host);
        }
    }
    let (rehearsal, report) = j.rehearse_operating_policy("release", corpus).unwrap();
    assert!(report.passed);
    let before = hub.success(
        &f,
        &[
            "observe",
            "release",
            "--connection",
            connection_path.to_str().unwrap(),
            "--proposal",
            &observe.to_string(),
            "--execute",
        ],
        true,
    );
    assert!(before["executions"].as_array().unwrap().is_empty());
    let p = j.operating_policy("release").unwrap();
    hub.success(
        &f,
        &[
            "activate",
            "release",
            "--digest",
            &p.digest,
            "--rehearsal",
            &rehearsal,
            "--expected-generation",
            &p.generation.to_string(),
        ],
        true,
    );
    let result = hub.success(
        &f,
        &[
            "observe",
            "release",
            "--connection",
            connection_path.to_str().unwrap(),
            "--proposal",
            &execute.to_string(),
            "--execute",
        ],
        true,
    );
    assert_eq!(
        result["executions"][0]["execution"]["run"]["state"], "succeeded",
        "{result}"
    );
    assert_eq!(
        result["executions"][0]["checkpoint_recorded"], true,
        "{result}"
    );
    let run_id = result["accepted"][0]["run_id"].as_str().unwrap();
    let artifact =
        skill_workflow::runtime::artifact(&f.data, run_id, "explain", "explanation").unwrap();
    let explanation = String::from_utf8(artifact).unwrap().to_lowercase();
    for required in [
        "filter", "all", "open", "closed", "default", "search", "query", "empty",
    ] {
        assert!(
            explanation.contains(required),
            "release explanation omitted source behavior: {required}"
        );
    }
    assert!(
        ["preserv", "retain", "keep", "unchanged"]
            .iter()
            .any(|word| explanation.contains(word)),
        "release explanation must describe preservation of the search query"
    );
    let duplicate = hub.success(
        &f,
        &[
            "observe",
            "release",
            "--connection",
            connection_path.to_str().unwrap(),
            "--proposal",
            &execute.to_string(),
            "--execute",
        ],
        true,
    );
    assert!(duplicate["executions"].as_array().unwrap().is_empty());
    assert_eq!(
        hub.store
            .work_search(
                &hub.token_id,
                None,
                None,
                100,
                agentscm::hub::store::unix_now()
            )
            .unwrap()
            .items
            .len(),
        1
    );
}

#[test]
fn explicit_cli_recovery_preserves_work_and_does_not_replay_failed_effect() {
    use agentscm::hub::work::{CheckpointKind, WorkCheckpoint};
    for outcome in [
        "uncommitted",
        "committed",
        "superseded",
        "compacted",
        "blocked",
    ] {
        let needs_reconciliation = matches!(outcome, "compacted" | "blocked");
        let prior_commit = outcome != "uncommitted";
        let mut f = fixture();
        let executable = f._root.path().join("fail-once");
        let marker = f._root.path().join("attempts");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\n/bin/cat >/dev/null\nexit 1\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut profile = read_profile(&f.rule).unwrap();
        profile.commands.get_mut("echo").unwrap().executable = executable;
        f.rule.profile_digest = profile.digest().unwrap();
        fs::write(&f.rule.profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
        let hub = LocalHub::start(&f);
        let mut j = journal(&f);
        activated(&f, &mut j);
        let mut next = event(&f);
        next.proposal_number += 1;
        let dispatch = j
            .accept_operating_event(&f.rule.rule_id, &next, ACTOR)
            .unwrap()
            .remove(0);
        let first = hub.success(&f, &["run", &dispatch.dispatch_id], false);
        assert_eq!(first["checkpoint_recorded"], true);
        let binding = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
        assert_eq!(binding.work.state, agentscm::hub::work::WorkState::Blocked);
        let current = j.dispatch(&dispatch.dispatch_id).unwrap();
        if outcome == "uncommitted" {
            // The legacy CLI must not requeue policy Work before its Blocker is reconciled.
            let legacy = Command::new(env!("CARGO_BIN_EXE_agentscm"))
                .env_clear()
                .env("HOME", f._root.path())
                .env("XDG_CONFIG_HOME", f._root.path())
                .current_dir(&f.source)
                .arg("--data-dir")
                .arg(&f.data)
                .args([
                    "pipeline",
                    "recover",
                    &dispatch.dispatch_id,
                    "--expected-generation",
                    &current.claim_generation.to_string(),
                ])
                .output()
                .unwrap();
            assert!(!legacy.status.success());
            assert!(String::from_utf8_lossy(&legacy.stderr).contains("policy-owned"));
            assert!(String::from_utf8_lossy(&legacy.stderr).contains("operating-policy recover"));
            assert_eq!(j.dispatch(&dispatch.dispatch_id).unwrap(), current);
            assert_eq!(
                j.operating_work(&dispatch.dispatch_id).unwrap().unwrap(),
                binding
            );
            assert_eq!(
                hub.store
                    .work_get(
                        &hub.token_id,
                        &binding.work.id,
                        agentscm::hub::store::unix_now()
                    )
                    .unwrap()
                    .unwrap()
                    .updated_seq,
                binding.work.updated_seq
            );
            assert_eq!(fs::read(&marker).unwrap(), b"x");
            // Continue below through the actual operating recovery and retained action guard.
        }
        assert!(
            !hub.cli(
                &f,
                &[
                    "recover",
                    &dispatch.dispatch_id,
                    "--expected-generation",
                    &(current.claim_generation + 1).to_string()
                ],
                false
            )
            .status
            .success()
        );
        let request = WorkCheckpoint {
            work_id: binding.work.id.clone(),
            operation_id: format!("reserved-recovery-{}", dispatch.dispatch_id),
            generation: binding.work.generation,
            session: binding.work.lead_session.clone(),
            expected_updated_seq: Some(binding.work.updated_seq),
            kind: CheckpointKind::Progress,
            summary: "Explicit recovery progress".into(),
            next_action: None,
            scope: None,
            branch: None,
            worktree: None,
            resolve_blocker: true,
            source_refs: vec![],
            depends_on: vec![],
            related_work: vec![],
        };
        let mut reserved = binding.clone();
        reserved.recovery = Some(OperatingRecovery {
            dispatch_generation: current.claim_generation,
            request: request.clone(),
            rejected_at_seq: None,
        });
        j.bind_operating_work(&dispatch.dispatch_id, Some(&binding), &reserved)
            .unwrap();
        if outcome == "uncommitted" {
            for invalid_sequence in [0, binding.work.updated_seq - 1, binding.work.updated_seq] {
                let mut invalid = reserved.clone();
                invalid.recovery.as_mut().unwrap().rejected_at_seq = Some(invalid_sequence);
                assert!(
                    j.bind_operating_work(&dispatch.dispatch_id, Some(&reserved), &invalid)
                        .is_err()
                );
            }
            let unchanged = hub.cli(
                &f,
                &[
                    "recover",
                    &dispatch.dispatch_id,
                    "--expected-generation",
                    &current.claim_generation.to_string(),
                    "--reconcile-operation",
                    &request.operation_id,
                    "--observed-seq",
                    &binding.work.updated_seq.to_string(),
                ],
                false,
            );
            assert!(!unchanged.status.success());
            assert_eq!(
                j.operating_work(&dispatch.dispatch_id).unwrap().unwrap(),
                reserved
            );
        }
        let mut committed_request = request.clone();
        if !prior_commit {
            committed_request.operation_id = "owner-progress-before-recovery".into();
        }
        let advanced = hub
            .store
            .work_checkpoint(
                &hub.token_id,
                &committed_request,
                agentscm::hub::store::unix_now() - if outcome == "compacted" { 200_000 } else { 0 },
            )
            .unwrap();
        if matches!(outcome, "superseded" | "blocked") {
            let mut newer = request.clone();
            newer.operation_id = "owner-checkpoint-after-recovery".into();
            newer.expected_updated_seq = Some(advanced.item.updated_seq);
            if outcome == "blocked" {
                newer.kind = CheckpointKind::Blocker;
                newer.resolve_blocker = false;
            }
            hub.store
                .work_checkpoint(&hub.token_id, &newer, agentscm::hub::store::unix_now())
                .unwrap();
        }
        if outcome == "compacted" {
            let now = agentscm::hub::store::unix_now();
            let compacted = hub
                .store
                .work_compact(
                    &agentscm::hub::work::WorkRepositoryScope {
                        provider: f.rule.provider.clone(),
                        host: f.rule.host.clone(),
                        repository: f.rule.repository.clone(),
                        repository_id: Some(f.rule.provider_repository_id),
                    },
                    now - 86_400,
                    true,
                    now,
                )
                .unwrap();
            assert_eq!(compacted.deleted_operations, 1);
        }
        let observed_sequence = hub
            .store
            .work_get(
                &hub.token_id,
                &binding.work.id,
                agentscm::hub::store::unix_now(),
            )
            .unwrap()
            .unwrap()
            .updated_seq;
        let recovery_args = [
            "recover",
            &dispatch.dispatch_id,
            "--expected-generation",
            &current.claim_generation.to_string(),
        ];
        if !prior_commit {
            let rejected = hub.cli(&f, &recovery_args, false);
            assert!(!rejected.status.success());
            assert!(String::from_utf8_lossy(&rejected.stderr).contains("without committing"));
            let retained = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
            assert_eq!(
                retained.recovery.unwrap().rejected_at_seq,
                Some(advanced.item.updated_seq)
            );
            assert_eq!(j.dispatch(&dispatch.dispatch_id).unwrap().state, "blocked");
        }
        if needs_reconciliation {
            let ambiguous = hub.cli(&f, &recovery_args, false);
            assert!(!ambiguous.status.success());
            let uncertain = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
            assert_eq!(uncertain.recovery.as_ref().unwrap().request, request);
            assert_eq!(uncertain.recovery.as_ref().unwrap().rejected_at_seq, None);
            assert!(uncertain.recovery_reconciliations.is_empty());
            assert_eq!(j.dispatch(&dispatch.dispatch_id).unwrap().state, "blocked");
            for (operation, sequence) in [
                ("wrong-retained-operation", observed_sequence),
                (request.operation_id.as_str(), observed_sequence - 1),
            ] {
                let mut args = recovery_args.to_vec();
                let observed = sequence.to_string();
                args.extend([
                    "--reconcile-operation",
                    operation,
                    "--observed-seq",
                    &observed,
                ]);
                let refused = hub.cli(&f, &args, false);
                assert!(!refused.status.success());
                assert_eq!(
                    j.operating_work(&dispatch.dispatch_id).unwrap().unwrap(),
                    uncertain
                );
            }
            let mut args = recovery_args.to_vec();
            let observed = observed_sequence.to_string();
            args.extend([
                "--reconcile-operation",
                &request.operation_id,
                "--observed-seq",
                &observed,
            ]);
            let reconciled = hub.success(&f, &args, false);
            assert_eq!(
                reconciled["work"]["recovery_reconciliations"][0]["outcome"],
                "unknown"
            );
        } else {
            hub.success(&f, &recovery_args, false);
        }
        let recovered = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
        if needs_reconciliation {
            assert!(recovered.work.updated_seq > observed_sequence);
            assert_eq!(recovered.recovery_reconciliations.len(), 1);
            let historical = &recovered.recovery_reconciliations[0];
            assert_eq!(historical.recovery, reserved.recovery.clone().unwrap());
            assert_eq!(historical.observed_seq, observed_sequence);
            assert_eq!(historical.recovery.rejected_at_seq, None);
            assert_ne!(
                recovered.recovery.as_ref().unwrap().request.operation_id,
                request.operation_id
            );
            let mut erased = recovered.clone();
            erased.recovery_reconciliations.clear();
            assert!(
                j.bind_operating_work(&dispatch.dispatch_id, Some(&recovered), &erased)
                    .is_err()
            );
            let status = hub.success(&f, &["status", &f.rule.rule_id], false);
            assert_eq!(
                status["work_bindings"][0]["binding"]["recovery_reconciliations"][0]["recovery"]["request"]
                    ["operation_id"],
                request.operation_id
            );
        } else if prior_commit {
            assert_eq!(recovered.work.updated_seq, observed_sequence);
            assert_eq!(recovered.recovery.as_ref().unwrap().request, request);
        } else {
            assert!(recovered.work.updated_seq > advanced.item.updated_seq);
            assert_ne!(
                recovered.recovery.as_ref().unwrap().request.operation_id,
                request.operation_id
            );
        }
        assert_eq!(j.dispatch(&dispatch.dispatch_id).unwrap().state, "pending");
        // A repeated pending recovery is read-only, with no additional checkpoint.
        let repeated = hub.success(&f, &recovery_args, false);
        assert_eq!(repeated["replayed"], true);
        assert_eq!(
            hub.store
                .work_get(
                    &hub.token_id,
                    &binding.work.id,
                    agentscm::hub::store::unix_now()
                )
                .unwrap()
                .unwrap()
                .updated_seq,
            recovered.work.updated_seq
        );
        assert_eq!(
            j.operating_work(&dispatch.dispatch_id)
                .unwrap()
                .unwrap()
                .work
                .id,
            binding.work.id
        );
        let replay = hub.success(&f, &["run", &dispatch.dispatch_id], false);
        assert_eq!(replay["checkpoint_recorded"], true);
        assert_eq!(fs::read(marker).unwrap(), b"x");
        assert_eq!(
            hub.store
                .work_search(
                    &hub.token_id,
                    None,
                    None,
                    100,
                    agentscm::hub::store::unix_now()
                )
                .unwrap()
                .items
                .len(),
            1
        );
        let latest = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
        assert_eq!(
            latest.recovery_reconciliations,
            recovered.recovery_reconciliations
        );
        let mut altered = latest.clone();
        altered.work.generation += 1;
        assert!(
            j.bind_operating_work(&dispatch.dispatch_id, Some(&latest), &altered)
                .is_err()
        );
        altered = latest.clone();
        altered.identity_commitment = "f".repeat(64);
        assert!(
            j.bind_operating_work(&dispatch.dispatch_id, Some(&latest), &altered)
                .is_err()
        );
    }
}

#[test]
fn typed_backup_preserves_both_pipeline_versions_and_migrates_only_on_open() {
    for version in [1, 2] {
        let f = fixture();
        let mut j = journal(&f);
        let retained_policy = if version == 2 {
            Some(activated(&f, &mut j))
        } else {
            j.install(&f.rule, None).unwrap();
            None
        };
        let mut next = event(&f);
        next.proposal_number += 1;
        let dispatch = if version == 2 {
            j.accept_operating_event(&f.rule.rule_id, &next, ACTOR)
                .unwrap()
                .remove(0)
        } else {
            j.accept(&next).unwrap().remove(0)
        };
        let expected = retained_policy.map(|_| j.operating_policy(&f.rule.rule_id).unwrap());
        drop(j);
        let path = f.data.join("pipeline-rules/state.sqlite");
        if version == 1 {
            let db = rusqlite::Connection::open(&path).unwrap();
            for table in [
                "operating_drafts",
                "operating_reservations",
                "operating_decisions",
                "operating_rehearsals",
                "operating_state",
                "operating_versions",
            ] {
                db.execute(&format!("DROP TABLE {table}"), []).unwrap();
            }
            db.pragma_update(None, "user_version", 1).unwrap();
        }
        let backup = f._root.path().join("backup");
        let manifest = agentscm::local_recovery::backup(&f.data, &backup, &[], &[], true).unwrap();
        assert_eq!(
            manifest.version, 1,
            "recovery container version is independent of SQLite schema"
        );
        agentscm::local_recovery::verify(&backup).unwrap();
        let read_version = |path: &Path| -> i64 {
            rusqlite::Connection::open(path)
                .unwrap()
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            read_version(&path),
            version,
            "backup must not migrate source state"
        );
        assert_eq!(
            read_version(&backup.join("payload/data/pipeline-rules/state.sqlite")),
            version
        );
        let restored = f._root.path().join("restored");
        agentscm::local_recovery::restore(&backup, &restored).unwrap();
        let restored_path = restored.join("data/pipeline-rules/state.sqlite");
        assert!(RuleJournal::open(&restored_path).is_err());
        agentscm::local_recovery::activate(&restored, true, true).unwrap();
        assert_eq!(
            read_version(&restored_path),
            version,
            "activation validates without migrating"
        );
        let restored_journal = RuleJournal::open(&restored_path).unwrap();
        assert_eq!(restored_journal.pending(10).unwrap(), vec![dispatch]);
        if let Some(policy) = expected {
            assert_eq!(
                restored_journal.operating_policy(&f.rule.rule_id).unwrap(),
                policy
            );
            assert!(
                !restored_journal.operating_history(&f.rule.rule_id).unwrap()["decisions"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(read_version(&restored_path), 2);
    }
}

#[test]
fn typed_backup_rejects_future_or_mistagged_pipeline_versions_and_operating_corruption() {
    for failure in [
        "future_version",
        "v2_tables_tagged_v1",
        "missing_operating_table",
        "changed_contract",
    ] {
        let f = fixture();
        let mut j = journal(&f);
        activated(&f, &mut j);
        drop(j);
        let path = f.data.join("pipeline-rules/state.sqlite");
        let db = rusqlite::Connection::open(&path).unwrap();
        match failure {
            "future_version" => db.pragma_update(None, "user_version", 3).unwrap(),
            "v2_tables_tagged_v1" => db.pragma_update(None, "user_version", 1).unwrap(),
            "missing_operating_table" => {
                db.execute("DROP TABLE operating_drafts", []).unwrap();
            }
            "changed_contract" => {
                db.execute("UPDATE operating_versions SET body=replace(body,'After a ship-labelled','After every')",[]).unwrap();
            }
            _ => unreachable!(),
        }
        drop(db);
        assert!(
            agentscm::local_recovery::backup(
                &f.data,
                &f._root.path().join("backup"),
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
fn legacy_pickup_skips_owned_dispatches_and_legacy_observe_does_not_consume_them() {
    let f = fixture();
    let hub = LocalHub::start(&f);
    let mut j = journal(&f);
    activated(&f, &mut j);
    let mut legacy = f.rule.clone();
    legacy.rule_id = "legacy".into();
    j.install(&legacy, None).unwrap();
    let mut next = event(&f);
    next.proposal_number += 1;
    let decisions = j.operating_history(&f.rule.rule_id).unwrap()["decisions"]
        .as_array()
        .unwrap()
        .len();
    let legacy_dispatch = j.accept(&next).unwrap().remove(0);
    assert_eq!(legacy_dispatch.rule.rule_id, "legacy");
    assert_eq!(
        j.operating_history(&f.rule.rule_id).unwrap()["decisions"]
            .as_array()
            .unwrap()
            .len(),
        decisions
    );
    let owned = j
        .accept_operating_event(&f.rule.rule_id, &next, ACTOR)
        .unwrap()
        .remove(0);
    assert_eq!(j.pending_legacy(32).unwrap(), vec![legacy_dispatch.clone()]);
    assert!(j.claim_legacy(&owned.dispatch_id).unwrap().is_none());
    let out = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .current_dir(&f.source)
        .arg("--data-dir")
        .arg(&f.data)
        .args(["pipeline", "pickup"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["deliveries"].as_array().unwrap().len(), 1);
    assert_eq!(
        j.dispatch(&legacy_dispatch.dispatch_id).unwrap().state,
        "complete"
    );
    let pending = j.dispatch(&owned.dispatch_id).unwrap();
    assert_eq!(pending.state, "pending");
    assert_eq!(pending.claim_generation, 0);
    assert!(j.operating_work(&owned.dispatch_id).unwrap().is_none());
    hub.success(&f, &["run", &owned.dispatch_id], false);
    assert_eq!(j.dispatch(&owned.dispatch_id).unwrap().state, "complete");
}

#[tokio::test]
async fn legacy_poll_continues_mixed_rules_and_leaves_owned_event_available() {
    use agentscm::hub::{connection::ConnectedRepository, forge_api::ForgeApi};
    use axum::{Json, Router, routing::get};
    let f = fixture();
    let mut j = journal(&f);
    activated(&f, &mut j);
    let mut legacy = f.rule.clone();
    legacy.rule_id = "legacy".into();
    j.install(&legacy, None).unwrap();
    let mut owned_first = event(&f);
    owned_first.proposal_number = 8;
    assert_eq!(
        j.accept_operating_event(&f.rule.rule_id, &owned_first, ACTOR)
            .unwrap()
            .len(),
        1
    );
    let commit = f.commit.clone();
    let detail = commit.clone();
    let app=Router::new().route("/repos/acme/widgets",get(||async{Json(json!({"id":42,"full_name":"acme/widgets"}))}))
        .route("/repos/acme/widgets/pulls",get(move||{let sha=commit.clone();async move{Json(json!([{"number":8,"merge_commit_sha":sha}]))}}))
        .route("/repos/acme/widgets/pulls/8",get(move||{let sha=detail.clone();async move{Json(json!({"number":8,"merged":true,"merge_commit_sha":sha,"base":{"ref":"main","repo":{"id":42,"full_name":"acme/widgets"}},"labels":[{"name":"ship"}]}))}}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = agentscm::hub::github::Api::new(&url, &url).unwrap();
    let connected = ConnectedRepository {
        id: "local".into(),
        connection: "local".into(),
        host: f.rule.host.clone(),
        repository_id: 42,
        full_name: f.rule.repository.clone(),
        node_id: None,
        installation_id: None,
        pulls_etag: None,
    };
    let report = poll_merges(ForgeApi::Github(&api), "synthetic", &connected, &mut j, 123)
        .await
        .unwrap();
    assert!(report.blocked.is_none(), "{report:?}");
    assert!(report.checkpoint_advanced);
    assert_eq!(j.pending_legacy(10).unwrap().len(), 1);
    let mut next = event(&f);
    next.proposal_number = 8;
    assert!(
        j.accept_operating_event(&f.rule.rule_id, &next, ACTOR)
            .unwrap()
            .is_empty()
    );
    server.abort();
}

#[test]
fn activation_requires_a_successful_matching_observation_and_retries_do_not_fill_history() {
    for failure in ["target", "actor", "profile"] {
        let f = fixture();
        let mut j = journal(&f);
        j.install(&f.rule, None).unwrap();
        let p = j
            .review_operating_policy(&contract(&f), ACTOR, None)
            .unwrap();
        let (rehearsal, _) = j
            .rehearse_operating_policy(&f.rule.rule_id, cases(&f))
            .unwrap();
        let mut next = event(&f);
        let mut actor = ACTOR;
        let profile = fs::read(&f.rule.profile_path).unwrap();
        match failure {
            "target" => next.target = "other".into(),
            "actor" => actor = "github:forge.invalid:999",
            _ => fs::remove_file(&f.rule.profile_path).unwrap(),
        }
        assert!(
            j.accept_operating_event(&f.rule.rule_id, &next, actor)
                .unwrap()
                .is_empty()
        );
        assert!(
            j.set_operating_mode(
                &f.rule.rule_id,
                &p.digest,
                p.generation,
                "active",
                Some(&rehearsal),
                ACTOR
            )
            .is_err()
        );
        fs::write(&f.rule.profile_path, profile).unwrap();
        fs::set_permissions(&f.rule.profile_path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut matching = event(&f);
        matching.proposal_number += 1;
        for _ in 0..25 {
            assert!(
                j.accept_operating_event(&f.rule.rule_id, &matching, ACTOR)
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(
            j.operating_history(&f.rule.rule_id).unwrap()["decisions"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        j.set_operating_mode(
            &f.rule.rule_id,
            &p.digest,
            p.generation,
            "active",
            Some(&rehearsal),
            ACTOR,
        )
        .unwrap();
    }
}

#[test]
fn native_draft_cannot_expand_dispatch_budget_or_clear_known_unresolved_clauses() {
    let f = fixture();
    let proposed = contract(&f);
    let mut draft = proposed.clone();
    draft.max_dispatches += 1;
    assert!(validate_draft(&draft, &proposed, &f.rule).is_err());
    let mut proposed = proposed;
    proposed.unresolved.push("publishing is unsupported".into());
    let mut draft = proposed.clone();
    draft.unresolved.clear();
    assert!(validate_draft(&draft, &proposed, &f.rule).is_err());
    for field in ["action", "required_evidence", "escalation"] {
        let mut value = serde_json::to_value(&proposed).unwrap();
        value[field] = json!("expanded_authority");
        assert!(serde_json::from_value::<OperatingContract>(value).is_err());
    }
}

#[test]
fn private_local_owner_can_narrow_offline_but_revocation_cannot_be_paused_away() {
    let f = fixture();
    let mut j = journal(&f);
    let p = activated(&f, &mut j);
    let invoke = |operation: &str, generation: i64| {
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .env_clear()
            .env("HOME", f._root.path())
            .current_dir(&f.source)
            .arg("--data-dir")
            .arg(&f.data)
            .args([
                "operating-policy",
                operation,
                &f.rule.rule_id,
                "--digest",
                &p.digest,
                "--expected-generation",
                &generation.to_string(),
            ])
            .output()
            .unwrap()
    };
    let paused = invoke("pause", p.generation);
    assert!(
        paused.status.success(),
        "{}",
        String::from_utf8_lossy(&paused.stderr)
    );
    let current = j.operating_policy(&f.rule.rule_id).unwrap();
    assert_eq!(current.mode, "paused");
    assert!(invoke("revoke", current.generation).status.success());
    let revoked = j.operating_policy(&f.rule.rule_id).unwrap();
    assert_eq!(revoked.mode, "revoked");
    assert!(!invoke("pause", revoked.generation).status.success());
    assert!(
        j.set_operating_mode(
            &f.rule.rule_id,
            &revoked.digest,
            revoked.generation,
            "active",
            revoked.rehearsal.as_deref(),
            ACTOR
        )
        .is_err()
    );
    assert!(
        j.narrow_operating_mode(
            &f.rule.rule_id,
            &revoked.digest,
            revoked.generation,
            "active"
        )
        .is_err()
    );
}

#[test]
fn definite_draft_preflight_errors_do_not_reserve_requests() {
    let f = fixture();
    let hub = LocalHub::start(&f);
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let intent = f._root.path().join("intent.txt");
    fs::write(&intent, "Produce the installed local release artifact.").unwrap();
    let out = hub.cli(
        &f,
        &[
            "draft",
            "release",
            "--intent",
            intent.to_str().unwrap(),
            "--client",
            "claude",
            "--executable",
            "/missing/native/client",
            "--request-id",
            "not-launched",
        ],
        false,
    );
    assert!(!out.status.success());
    let db = rusqlite::Connection::open(f.data.join("pipeline-rules/state.sqlite")).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM operating_drafts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn recovery_replays_the_exact_checkpoint_after_lost_response_and_can_requeue_unbound_legacy_refusal()
 {
    use agentscm::hub::work::{CheckpointKind, WorkCheckpoint};
    let f = fixture();
    let hub = LocalHub::start(&f);
    let mut j = journal(&f);
    activated(&f, &mut j);
    let mut next = event(&f);
    next.proposal_number += 1;
    let dispatch = j
        .accept_operating_event(&f.rule.rule_id, &next, ACTOR)
        .unwrap()
        .remove(0);
    let claim = j.claim(&dispatch.dispatch_id).unwrap();
    assert!(deliver(&f.data, &mut j, &claim, &f.work).is_err());
    hub.success(
        &f,
        &[
            "recover",
            &dispatch.dispatch_id,
            "--expected-generation",
            &claim.generation.to_string(),
        ],
        false,
    );
    // Run through the actual Hub path, retaining its real Work binding and artifact.
    let profile = fs::read(&f.rule.profile_path).unwrap();
    let result = hub.success(&f, &["run", &dispatch.dispatch_id], false);
    assert_eq!(result["checkpoint_recorded"], true);
    // A successful dispatch can reconcile a lost completion checkpoint without rerunning it.
    let binding = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
    let retained = j.dispatch(&dispatch.dispatch_id).unwrap();
    let request = WorkCheckpoint {
        work_id: binding.work.id.clone(),
        operation_id: format!(
            "operating-recovery-{}-{}",
            dispatch.dispatch_id, retained.claim_generation
        ),
        generation: binding.work.generation,
        session: binding.work.lead_session.clone(),
        expected_updated_seq: Some(binding.work.updated_seq),
        kind: CheckpointKind::Finish,
        summary: "Reconciled retained successful workflow".into(),
        next_action: Some("Inspect retained workflow artifacts and attempts".into()),
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: true,
        source_refs: vec![],
        depends_on: vec![],
        related_work: vec![],
    };
    let mut reserved = binding.clone();
    reserved.recovery = Some(OperatingRecovery {
        dispatch_generation: retained.claim_generation,
        request: request.clone(),
        rejected_at_seq: None,
    });
    j.bind_operating_work(&dispatch.dispatch_id, Some(&binding), &reserved)
        .unwrap();
    let committed = hub
        .store
        .work_checkpoint(&hub.token_id, &request, agentscm::hub::store::unix_now())
        .unwrap();
    // Keep the old local binding to model a lost successful HTTP response.
    hub.success(
        &f,
        &[
            "recover",
            &dispatch.dispatch_id,
            "--expected-generation",
            &retained.claim_generation.to_string(),
        ],
        false,
    );
    assert_eq!(
        j.operating_work(&dispatch.dispatch_id)
            .unwrap()
            .unwrap()
            .work
            .updated_seq,
        committed.item.updated_seq
    );
    let successful_workflow = skill_workflow::runtime::status(&f.data, &dispatch.run_id).unwrap();
    let before_compaction = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
    let mut old_finish = request.clone();
    old_finish.operation_id = "compacted-finish-recovery".into();
    old_finish.expected_updated_seq = Some(before_compaction.work.updated_seq);
    let mut uncertain = before_compaction.clone();
    uncertain.recovery = Some(OperatingRecovery {
        dispatch_generation: retained.claim_generation,
        request: old_finish.clone(),
        rejected_at_seq: None,
    });
    j.bind_operating_work(&dispatch.dispatch_id, Some(&before_compaction), &uncertain)
        .unwrap();
    let now = agentscm::hub::store::unix_now();
    let old_commit = hub
        .store
        .work_checkpoint(&hub.token_id, &old_finish, now - 200_000)
        .unwrap();
    let compacted = hub
        .store
        .work_compact(
            &agentscm::hub::work::WorkRepositoryScope {
                provider: f.rule.provider.clone(),
                host: f.rule.host.clone(),
                repository: f.rule.repository.clone(),
                repository_id: Some(f.rule.provider_repository_id),
            },
            now - 86_400,
            true,
            now,
        )
        .unwrap();
    assert_eq!(compacted.deleted_operations, 1);
    let normal = [
        "recover",
        &dispatch.dispatch_id,
        "--expected-generation",
        &retained.claim_generation.to_string(),
    ];
    assert!(!hub.cli(&f, &normal, false).status.success());
    assert_eq!(
        j.operating_work(&dispatch.dispatch_id)
            .unwrap()
            .unwrap()
            .recovery
            .unwrap()
            .request,
        old_finish
    );
    let mut explicit = normal.to_vec();
    let observed = old_commit.item.updated_seq.to_string();
    explicit.extend([
        "--reconcile-operation",
        &old_finish.operation_id,
        "--observed-seq",
        &observed,
    ]);
    hub.success(&f, &explicit, false);
    let reconciled = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
    assert_eq!(
        reconciled.work.state,
        agentscm::hub::work::WorkState::Finished
    );
    assert!(
        reconciled
            .work
            .current
            .as_ref()
            .unwrap()
            .source_refs
            .iter()
            .any(
                |source| matches!(source, agentscm::hub::work::SourceRef::Commit { oid, path: None }
            if oid == &dispatch.event.accepted_commit)
            )
    );

    assert_eq!(
        reconciled.recovery_reconciliations[0].recovery.request,
        old_finish
    );
    assert_eq!(
        reconciled.recovery_reconciliations[0]
            .recovery
            .rejected_at_seq,
        None
    );
    assert_eq!(
        skill_workflow::runtime::status(&f.data, &dispatch.run_id).unwrap(),
        successful_workflow
    );
    assert_eq!(fs::read(&f.rule.profile_path).unwrap(), profile);
    assert_eq!(
        hub.store
            .work_search(
                &hub.token_id,
                None,
                None,
                100,
                agentscm::hub::store::unix_now()
            )
            .unwrap()
            .items
            .len(),
        1
    );
}

#[test]
fn completed_draft_replay_works_offline_without_the_original_executable() {
    use agentscm::continuation::NativeClient;
    use agentscm::operating_policy::draft_request_digest;
    let f = fixture();
    let mut j = journal(&f);
    let intent = "Run the installed local artifact workflow after a matching merge.";
    let input = f._root.path().join("intent.txt");
    fs::write(&input, intent).unwrap();
    let missing = f._root.path().join("missing-native-client");
    let digest =
        draft_request_digest(&f.rule.rule_id, intent, NativeClient::Claude, &missing, 120).unwrap();
    assert!(
        j.reserve_operating_draft("retained-draft", &digest)
            .unwrap()
            .is_none()
    );
    let response = json!({"native_session_id":"retained-parser-evidence","contract":contract(&f)});
    j.finish_operating_draft("retained-draft", &digest, &response)
        .unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .env_clear()
            .env("HOME", f._root.path())
            .env("XDG_CONFIG_HOME", f._root.path())
            .current_dir(&f.source)
            .arg("--data-dir")
            .arg(&f.data)
            .args(["operating-policy", "draft", &f.rule.rule_id, "--intent"])
            .arg(&input)
            .args(["--client", "claude", "--executable"])
            .arg(&missing)
            .args(["--request-id", "retained-draft"])
            .output()
            .unwrap()
    };
    let replay = run();
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.stdout).unwrap(),
        response
    );
    fs::write(&input, "A different owner request").unwrap();
    let changed = run();
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("draft request identity"));
}

#[test]
fn policy_attachment_refuses_unfinished_legacy_dispatches_until_completion() {
    let f = fixture();
    let mut j = journal(&f);
    j.install(&f.rule, None).unwrap();
    let dispatch = j.accept(&event(&f)).unwrap().remove(0);
    for state in ["pending", "claimed", "blocked"] {
        assert_eq!(j.dispatch(&dispatch.dispatch_id).unwrap().state, state);
        assert!(
            matches!(j.review_operating_policy(&contract(&f), ACTOR, None),
            Err(agentscm::Error::Conflict(reason)) if reason.contains("legacy dispatches remain unfinished"))
        );
        assert!(j.operating_policy(&f.rule.rule_id).is_err());
        if state == "pending" {
            j.claim(&dispatch.dispatch_id).unwrap();
        } else if state == "claimed" {
            let claim = j.dispatch(&dispatch.dispatch_id).unwrap();
            j.finish(
                &agentscm::hub::pipeline_rules::DispatchClaim {
                    dispatch: claim.dispatch,
                    generation: claim.claim_generation,
                },
                "blocked",
                Some("owner inspection required"),
            )
            .unwrap();
        }
    }
    let retained = j.dispatch(&dispatch.dispatch_id).unwrap();
    j.recover(&dispatch.dispatch_id, retained.claim_generation)
        .unwrap();
    let claim = j.claim(&dispatch.dispatch_id).unwrap();
    let run = deliver(&f.data, &mut j, &claim, &f.work).unwrap();
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(j.dispatch(&dispatch.dispatch_id).unwrap().state, "complete");
    j.review_operating_policy(&contract(&f), ACTOR, None)
        .unwrap();
}

#[tokio::test]
async fn all_owned_legacy_poll_makes_no_provider_requests() {
    use agentscm::hub::{connection::ConnectedRepository, forge_api::ForgeApi};
    let f = fixture();
    let mut j = journal(&f);
    activated(&f, &mut j);
    // An unserved listener catches even one attempted repository/list/detail request.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let api = agentscm::hub::github::Api::new(&url, &url).unwrap();
    let connected = ConnectedRepository {
        id: "local".into(),
        connection: "local".into(),
        host: f.rule.host.clone(),
        repository_id: 42,
        full_name: f.rule.repository.clone(),
        node_id: None,
        installation_id: None,
        pulls_etag: None,
    };
    let report = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        poll_merges(ForgeApi::Github(&api), "synthetic", &connected, &mut j, 123),
    )
    .await
    .expect("all-owned poll attempted provider I/O")
    .unwrap();
    assert!(report.blocked.is_none());
    assert!(!report.checkpoint_advanced);
    assert!(report.observations.is_empty());
    assert!(report.coverage.contains("no legacy-eligible rules"));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn reconciliation_distinguishes_absent_workflow_from_pending_or_incomplete_runtime() {
    use agentscm::hub::work::{
        CheckpointKind, WorkCheckpoint, WorkRepositoryScope, WorkRole, WorkStart,
    };
    for runtime_state in ["absent", "missing", "pending", "incomplete"] {
        let f = fixture();
        let hub = LocalHub::start(&f);
        let mut j = journal(&f);
        activated(&f, &mut j);
        let mut next = event(&f);
        next.proposal_number += 1;
        let dispatch = j
            .accept_operating_event(&f.rule.rule_id, &next, ACTOR)
            .unwrap()
            .remove(0);
        // Model the real Run boundary: Hub Work is bound before claiming and
        // runtime::start. No command or effect is needed to reach this state.
        let work = hub
            .store
            .work_start(
                &hub.token_id,
                &WorkStart {
                    work_id: None,
                    source: None,
                    request_key: Some(dispatch.run_id.clone()),
                    title: "Workflow admission interrupted".into(),
                    session: format!("operating-{}", dispatch.dispatch_id),
                    role: WorkRole::Lead,
                    scope: vec![],
                    branch: None,
                    worktree: None,
                    next_action: None,
                    depends_on: vec![],
                    related_work: vec![],
                    takeover_generation: None,
                    preflight: None,
                },
                agentscm::hub::store::unix_now(),
            )
            .unwrap()
            .item;
        let scope = WorkRepositoryScope {
            provider: f.rule.provider.clone(),
            host: f.rule.host.clone(),
            repository: f.rule.repository.clone(),
            repository_id: Some(f.rule.provider_repository_id),
        };
        let identity_commitment = agentscm::store::digest(
            &serde_json::to_vec(&json!({
                "domain":"agentscm/managed-work-identity/v1", "hub":hub.url, "scope":scope,
                "token_id":hub.token_id,
                "owner":{"provider":f.rule.provider,"host":f.rule.host,"user_id":7}
            }))
            .unwrap(),
        );
        let binding = OperatingWork {
            identity_commitment,
            work: work.clone(),
            recovery: None,
            recovery_reconciliations: vec![],
        };
        j.bind_operating_work(&dispatch.dispatch_id, None, &binding)
            .unwrap();
        let claim = j.claim(&dispatch.dispatch_id).unwrap();
        if matches!(runtime_state, "missing" | "pending") {
            let plan = compile_source(&f.data, &f.rule, &f.commit).unwrap();
            let run = if runtime_state == "pending" {
                dispatch.run_id.as_str()
            } else {
                "unrelated-pending-run"
            };
            let retained = skill_workflow::runtime::start_compiled(&f.data, run, &plan).unwrap();
            assert_eq!(retained.state, RunState::Pending);
        } else if runtime_state == "incomplete" {
            fs::create_dir(f.data.join("skill-workflows")).unwrap();
        }
        let request = WorkCheckpoint {
            work_id: work.id.clone(),
            operation_id: "pre-workflow-recovery".into(),
            generation: work.generation,
            session: work.lead_session.clone(),
            expected_updated_seq: Some(work.updated_seq),
            kind: CheckpointKind::Progress,
            summary: "Retained administrative recovery".into(),
            next_action: None,
            scope: None,
            branch: None,
            worktree: None,
            resolve_blocker: true,
            source_refs: vec![],
            depends_on: vec![],
            related_work: vec![],
        };
        let mut reserved = binding.clone();
        reserved.recovery = Some(OperatingRecovery {
            dispatch_generation: claim.generation,
            request: request.clone(),
            rejected_at_seq: None,
        });
        j.bind_operating_work(&dispatch.dispatch_id, Some(&binding), &reserved)
            .unwrap();
        let committed = hub
            .store
            .work_checkpoint(&hub.token_id, &request, agentscm::hub::store::unix_now())
            .unwrap();
        let mut blocker = request.clone();
        blocker.operation_id = "owner-blocker-after-pre-workflow-recovery".into();
        blocker.expected_updated_seq = Some(committed.item.updated_seq);
        blocker.kind = CheckpointKind::Blocker;
        blocker.resolve_blocker = false;
        let current = hub
            .store
            .work_checkpoint(&hub.token_id, &blocker, agentscm::hub::store::unix_now())
            .unwrap();
        let args = [
            "recover",
            &dispatch.dispatch_id,
            "--expected-generation",
            &claim.generation.to_string(),
        ];
        assert!(!hub.cli(&f, &args, false).status.success());
        let before = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
        let mut explicit = args.to_vec();
        let sequence = current.item.updated_seq.to_string();
        explicit.extend([
            "--reconcile-operation",
            &request.operation_id,
            "--observed-seq",
            &sequence,
        ]);
        if matches!(runtime_state, "absent" | "missing") {
            hub.success(&f, &explicit, false);
            let after = j.operating_work(&dispatch.dispatch_id).unwrap().unwrap();
            assert_eq!(after.recovery_reconciliations.len(), 1);
            assert_eq!(after.recovery_reconciliations[0].recovery.request, request);
            assert_eq!(
                after.recovery_reconciliations[0].outcome,
                RecoveryOutcome::Unknown
            );
            assert_eq!(j.dispatch(&dispatch.dispatch_id).unwrap().state, "pending");
            if runtime_state == "absent" {
                assert!(!f.data.join("skill-workflows").exists());
            } else {
                assert!(matches!(
                    skill_workflow::runtime::status(&f.data, &dispatch.run_id),
                    Err(agentscm::Error::NotFound(_))
                ));
            }
        } else {
            let refused = hub.cli(&f, &explicit, false);
            assert!(!refused.status.success());
            if runtime_state == "pending" {
                assert!(
                    String::from_utf8_lossy(&refused.stderr).contains("not in a terminal state")
                );
                assert_eq!(
                    skill_workflow::runtime::status(&f.data, &dispatch.run_id)
                        .unwrap()
                        .state,
                    RunState::Pending
                );
            }
            assert_eq!(
                j.operating_work(&dispatch.dispatch_id).unwrap().unwrap(),
                before
            );
            assert_eq!(
                hub.store
                    .work_get(&hub.token_id, &work.id, agentscm::hub::store::unix_now())
                    .unwrap()
                    .unwrap()
                    .updated_seq,
                current.item.updated_seq
            );
            assert_eq!(j.dispatch(&dispatch.dispatch_id).unwrap().state, "claimed");
        }
    }
}
