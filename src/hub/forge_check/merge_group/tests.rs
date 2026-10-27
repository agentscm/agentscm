//! Synthetic fixtures only; no network or store.
use super::*;
use crate::context::ChangeId;
use serde_json::json;

const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HEAD: &str = "1111111111111111111111111111111111111111";
const GROUP: &str = "2222222222222222222222222222222222222222";
const TREE: &str = "3333333333333333333333333333333333333333";
const OTHER: &str = "4444444444444444444444444444444444444444";
const REPO: u64 = 100;

fn subject() -> CandidateSubject {
    CandidateSubject {
        id: "candidate".into(),
        attachment: "attachment".into(),
        endpoint: "endpoint".into(),
        output: "output".into(),
        change: ChangeId {
            repository: "native".into(),
            id: "work".into(),
        },
        change_version: 1,
        context_digest: "context".into(),
        policy_epoch: 0,
        revision: "revision".into(),
        target: "main".into(),
        baseline_commit: BASE.into(),
        parent_commit: OTHER.into(),
        source_commit: HEAD.into(),
        source_fingerprint: "source".into(),
        comparison_fingerprint: "comparison".into(),
    }
}

fn live() -> LiveGroup {
    LiveGroup {
        head_sha: GROUP.into(),
        base_ref: "main".into(),
        base_sha: BASE.into(),
        head_tree: TREE.into(),
        head_parents: vec![BASE.into(), HEAD.into()],
        pr_open: true,
        queued_head_sha: HEAD.into(),
        pr_head_sha: HEAD.into(),
        pr_head_repo_id: REPO,
        pr_base_repo_id: REPO,
        stacked: false,
    }
}

fn qualified(tree: &str) -> NativeState {
    NativeState::Qualified {
        receipt: "receipt".into(),
        requirement_digest: "requirement".into(),
        tree_oid: tree.into(),
    }
}

fn payload(action: &str, head_ref: &str) -> Value {
    json!({"action": action, "reason": "invalidated", "merge_group": {
        "head_sha": GROUP, "head_ref": head_ref, "base_sha": BASE, "base_ref": "refs/heads/main"}})
}

#[test]
fn a_queue_branch_names_its_pull_request_and_head() {
    let name = format!("refs/heads/gh-readonly-queue/main/pr-12-{HEAD}");
    assert_eq!(
        queue_branch(&name),
        Some(QueueBranch {
            base: "main".into(),
            pr_number: 12,
            pr_head_sha: HEAD.into(),
        })
    );
    let nested = format!("gh-readonly-queue/release/v1/pr-3-{HEAD}");
    assert_eq!(queue_branch(&nested).unwrap().base, "release/v1");
    for bad in [
        format!("gh-readonly-queue/main/pr-0-{HEAD}"),
        format!("gh-readonly-queue/main/pr-+1-{HEAD}"),
        format!("gh-readonly-queue/main/pr-1-{}", &HEAD[1..]),
        format!("gh-readonly-queue//pr-1-{HEAD}"),
        format!("feature/main/pr-1-{HEAD}"),
        "gh-readonly-queue/main/pr-1".into(),
    ] {
        assert_eq!(queue_branch(&bad), None, "{bad}");
    }
}

#[test]
fn a_delivery_is_parsed_only_when_every_identity_is_well_formed() {
    let head_ref = format!("refs/heads/gh-readonly-queue/main/pr-12-{HEAD}");
    let event = group_event(&payload("checks_requested", &head_ref)).unwrap();
    assert_eq!(event.action, GroupAction::ChecksRequested);
    assert_eq!(
        event.head_ref,
        format!("gh-readonly-queue/main/pr-12-{HEAD}")
    );
    assert_eq!(
        (event.queue.pr_number, event.base_ref.as_str()),
        (12, "main")
    );
    assert_eq!(
        group_event(&payload("destroyed", &head_ref))
            .unwrap()
            .action,
        GroupAction::Destroyed("invalidated")
    );
    assert_eq!(group_event(&payload("enqueued", &head_ref)), None);
    // The head branch must belong to the same base the delivery names.
    let elsewhere = format!("refs/heads/gh-readonly-queue/dev/pr-12-{HEAD}");
    assert_eq!(group_event(&payload("checks_requested", &elsewhere)), None);
    let mut short = payload("checks_requested", &head_ref);
    short["merge_group"]["head_sha"] = json!("abc");
    assert_eq!(group_event(&short), None);
}

#[test]
fn only_the_branch_tip_keeps_a_group_current() {
    assert_eq!(lifecycle(GROUP, Some(GROUP)), None);
    assert_eq!(
        lifecycle(GROUP, Some(OTHER)),
        Some("merge group was rebuilt")
    );
    assert_eq!(lifecycle(GROUP, None), Some("merge group branch is gone"));
}

#[test]
fn success_needs_the_group_tree_to_be_the_qualified_one_on_the_candidate_base() {
    let ok = evaluate_group(REPO, &subject(), HEAD, &live(), &qualified(TREE));
    assert_eq!(ok, Verdict::Success);
    // A passing pull request head does not carry over to different group bytes.
    let other_bytes = evaluate_group(REPO, &subject(), HEAD, &live(), &qualified(OTHER));
    assert!(
        matches!(other_bytes, Verdict::Failure(_)),
        "{other_bytes:?}"
    );
    let unqualified = evaluate_group(REPO, &subject(), HEAD, &live(), &NativeState::NotQualified);
    assert!(matches!(unqualified, Verdict::Pending(_)));
}

#[test]
fn a_group_commit_must_be_built_on_the_candidate_baseline_and_the_queued_head() {
    let judge = |parents: Vec<String>| {
        let mut group = live();
        group.head_parents = parents;
        evaluate_group(REPO, &subject(), HEAD, &group, &qualified(TREE))
    };
    let not_built = Verdict::Failure("merge group commit is not built on the candidate baseline");
    assert_eq!(judge(vec![BASE.into()]), Verdict::Success);
    assert_eq!(judge(vec![OTHER.into(), HEAD.into()]), not_built);
    assert_eq!(judge(vec![BASE.into(), OTHER.into()]), not_built);
    assert_eq!(judge(vec![]), not_built);
    assert_eq!(
        judge(vec![BASE.into(), HEAD.into(), HEAD.into()]),
        not_built
    );
    assert_eq!(
        judge(vec![BASE.into(), "nope".into()]),
        Verdict::Failure("malformed commit identity")
    );
    // A stacked group keeps its pending verdict whatever its parents are.
    let mut stacked = live();
    stacked.base_sha = OTHER.into();
    stacked.stacked = true;
    stacked.head_parents = vec![];
    assert!(matches!(
        evaluate_group(REPO, &subject(), HEAD, &stacked, &qualified(TREE)),
        Verdict::Pending(_)
    ));
}

#[test]
fn a_group_stacked_on_other_queued_changes_is_pending_never_success() {
    let mut stacked = live();
    stacked.base_sha = OTHER.into();
    stacked.stacked = true;
    let verdict = evaluate_group(REPO, &subject(), HEAD, &stacked, &qualified(TREE));
    assert_eq!(
        verdict,
        Verdict::Pending("unsupported: no evidence covers this group's combined source")
    );
    stacked.stacked = false;
    let advanced = evaluate_group(REPO, &subject(), HEAD, &stacked, &qualified(TREE));
    assert_eq!(
        advanced,
        Verdict::Failure("target advanced since the candidate baseline")
    );
}

#[test]
fn queued_and_live_heads_must_both_be_the_published_candidate() {
    let mut moved = live();
    moved.pr_head_sha = OTHER.into();
    assert!(matches!(
        evaluate_group(REPO, &subject(), HEAD, &moved, &qualified(TREE)),
        Verdict::Failure("pull request head changed after it was queued")
    ));
    let mut other = live();
    other.queued_head_sha = OTHER.into();
    other.pr_head_sha = OTHER.into();
    assert!(matches!(
        evaluate_group(REPO, &subject(), HEAD, &other, &qualified(TREE)),
        Verdict::Failure("queued head is not the published candidate")
    ));
    let mut fork = live();
    fork.pr_head_repo_id = 7;
    assert!(matches!(
        evaluate_group(REPO, &subject(), HEAD, &fork, &qualified(TREE)),
        Verdict::Failure(_)
    ));
    let mut closed = live();
    closed.pr_open = false;
    assert!(matches!(
        evaluate_group(REPO, &subject(), HEAD, &closed, &qualified(TREE)),
        Verdict::Failure(_)
    ));
}

#[test]
fn a_stale_or_unreadable_native_state_never_succeeds() {
    let stale = evaluate_group(
        REPO,
        &subject(),
        HEAD,
        &live(),
        &NativeState::Stale("moved"),
    );
    assert_eq!(stale, Verdict::Failure("moved"));
    let gone = evaluate_group(
        REPO,
        &subject(),
        HEAD,
        &live(),
        &NativeState::Unavailable("gone"),
    );
    assert_eq!(gone, Verdict::Error("gone"));
}

#[test]
fn the_group_binding_changes_with_the_group_and_differs_from_a_head_binding() {
    let digest = |live: &LiveGroup| {
        group_binding_digest(
            REPO,
            12,
            live,
            &subject(),
            "native",
            "requirement",
            "receipt",
        )
    };
    let mut rebuilt = live();
    rebuilt.head_sha = OTHER.into();
    assert_ne!(digest(&live()), digest(&rebuilt));
    assert_eq!(digest(&live()), digest(&live()));
    assert_eq!(digest(&live()).len(), 64);
}

#[test]
fn ruleset_requirement_reports_the_weakest_source() {
    let rule = |checks: Value| {
        json!([{"type": "required_status_checks",
        "parameters": {"required_status_checks": checks, "strict_required_status_checks_policy": false}}])
    };
    let ctx = "agentscm/workflow/main";
    assert_eq!(
        ruleset_requirement(
            &rule(json!([{"context": ctx, "integration_id": 9}])),
            ctx,
            Some(9)
        ),
        "required_from_this_app"
    );
    assert_eq!(
        ruleset_requirement(
            &rule(json!([{"context": ctx, "integration_id": 8}])),
            ctx,
            Some(9)
        ),
        "required_from_another_source"
    );
    assert_eq!(
        ruleset_requirement(&rule(json!([{"context": ctx}])), ctx, Some(9)),
        "required_any_source"
    );
    let mixed = json!([
        {"type": "required_status_checks", "parameters": {"required_status_checks": [{"context": ctx, "integration_id": 9}]}},
        {"type": "required_status_checks", "parameters": {"required_status_checks": [{"context": ctx}]}}
    ]);
    assert_eq!(
        ruleset_requirement(&mixed, ctx, Some(9)),
        "required_any_source"
    );
    assert_eq!(
        ruleset_requirement(&rule(json!([{"context": "other"}])), ctx, Some(9)),
        "not_required"
    );
    assert_eq!(ruleset_requirement(&json!({}), ctx, Some(9)), "unknown");
    // A personal token has no App identity, so an App-pinned rule is someone else's source.
    assert_eq!(
        ruleset_requirement(
            &rule(json!([{"context": ctx, "integration_id": 9}])),
            ctx,
            None
        ),
        "required_from_another_source"
    );
}

#[test]
fn status_source_names_who_posted_the_newest_status_of_the_context() {
    let ctx = "agentscm/workflow/main";
    let list = json!([
        {"context": "other", "creator": {"login": "someone"}},
        {"context": ctx, "creator": {"login": "imposter"}},
        {"context": ctx, "creator": {"login": "agentscm[bot]"}}
    ]);
    assert_eq!(
        status_source(&list, true, ctx, Some("agentscm[bot]")),
        "another_account"
    );
    assert_eq!(
        status_source(&json!([list[2].clone()]), true, ctx, Some("AgentSCM[bot]")),
        "hub"
    );
    assert_eq!(status_source(&json!([]), true, ctx, Some("x")), "absent");
    assert_eq!(status_source(&json!([]), false, ctx, Some("x")), "unknown");
    assert_eq!(status_source(&list, true, ctx, None), "unknown");
}

#[test]
fn delivery_access_needs_an_app_with_the_permission_and_the_event() {
    let installation = |permission: Option<&str>, events: Value| json!({"permissions": {"merge_queues": permission}, "events": events});
    let full = installation(Some("read"), json!(["pull_request", "merge_group"]));
    assert_eq!(delivery_access("app", Some(&full)), "subscribed");
    let no_event = installation(Some("write"), json!(["pull_request"]));
    assert_eq!(
        delivery_access("app", Some(&no_event)),
        "event_not_subscribed"
    );
    let no_permission = installation(None, json!(["merge_group"]));
    assert_eq!(
        delivery_access("app", Some(&no_permission)),
        "permission_missing"
    );
    assert_eq!(delivery_access("app", None), "unknown");
    // GitHub delivers merge_group only to Apps, never to repository webhooks.
    assert_eq!(
        delivery_access("pat", Some(&full)),
        "personal_token_unsupported"
    );
}

#[test]
fn only_a_verified_merge_commit_is_a_landed_outcome() {
    assert_eq!(outcome(None), "pending");
    assert_eq!(outcome(Some(LANDED)), "landed");
    assert_eq!(outcome(Some("merged")), "merged_reported");
    for reason in ["invalidated", "dequeued", "merge group was rebuilt"] {
        assert_eq!(outcome(Some(reason)), "not_landed");
    }
    for reason in [MERGED_ELSEWHERE, "merge group branch is gone", "destroyed"] {
        assert_eq!(outcome(Some(reason)), "unknown");
    }
}
