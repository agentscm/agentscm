//! The GitLab client's publication calls (via `ForgeApi`) against the in-process fake. Synthetic data only.
#[path = "support/fake_gitlab.rs"]
mod fake_gitlab;

use agentscm::hub::connection::ConnectedRepository;
use agentscm::hub::forge_api::ForgeApi;
use agentscm::hub::github::GhError;
use agentscm::hub::gitlab::Api;
use fake_gitlab::{Branch, FakeGitlab, Mr, Project, Token};

const TOKEN: &str = "t-pub";
const MARKER: &str = "<!-- agentscm-candidate:pub_1:abc -->";

fn repo() -> ConnectedRepository {
    ConnectedRepository {
        id: "rep_00000000000000000000000000000000".into(),
        connection: "conn".into(),
        host: "gitlab.example".into(),
        repository_id: 1,
        full_name: "group/sub/widgets".into(),
        node_id: None,
        installation_id: None,
        pulls_etag: None,
    }
}

async fn setup() -> (FakeGitlab, Api) {
    let fake = FakeGitlab::start().await;
    fake.with(|s| {
        let mut p = Project::new(1, "group/sub/widgets", "private");
        p.branches = vec![
            Branch::new("main", "2026-01-01T00:00:00Z"),
            Branch::new("agent/x", "2026-01-02T00:00:00Z"),
        ];
        s.projects.insert(1, p);
        s.tokens.insert(
            TOKEN.into(),
            Token {
                username: "bot".into(),
                user_id: 7,
                access: [(1, 30)].into(),
            },
        );
    });
    let api = Api::new(&format!("{}/api/v4", fake.base)).unwrap();
    (fake, api)
}

async fn create(api: &Api) -> Result<Result<serde_json::Value, u16>, GhError> {
    ForgeApi::Gitlab(api)
        .create_candidate_pull(
            TOKEN,
            &repo(),
            "title",
            &format!("body\n\n{MARKER}"),
            "agent/x",
            "main",
        )
        .await
}

#[tokio::test]
async fn reads_default_branch_and_branch_tips() {
    let (fake, api) = setup().await;
    let forge = ForgeApi::Gitlab(&api);
    assert_eq!(forge.default_branch(TOKEN, &repo()).await.unwrap(), "main");
    let tip = fake.with(|s| s.projects[&1].branches[1].sha.clone());
    assert_eq!(
        forge.ref_sha(TOKEN, &repo(), "agent/x").await.unwrap(),
        Some(tip)
    );
    assert_eq!(forge.ref_sha(TOKEN, &repo(), "gone").await.unwrap(), None);
    assert!(forge.ref_sha(TOKEN, &repo(), "bad..name").await.is_err());
    fake.with(|s| s.projects.get_mut(&1).unwrap().default_branch = None);
    assert!(forge.default_branch(TOKEN, &repo()).await.is_err());
}

#[tokio::test]
async fn create_records_the_body_and_is_found_again() {
    let (fake, api) = setup().await;
    let pull = create(&api).await.unwrap().unwrap();
    assert_eq!(pull["number"], 1);
    assert_eq!(pull["head"]["ref"], "agent/x");
    assert_eq!(pull["base"]["ref"], "main");
    assert_eq!(pull["source_project_id"], 1);
    assert!(pull["body"].as_str().unwrap().lines().any(|l| l == MARKER));
    let found = ForgeApi::Gitlab(&api)
        .find_open_pulls(TOKEN, &repo(), "agent/x", "main")
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["head"]["sha"], pull["head"]["sha"]);
    // Another target is a different question.
    let other = ForgeApi::Gitlab(&api)
        .find_open_pulls(TOKEN, &repo(), "agent/x", "release")
        .await
        .unwrap();
    assert!(other.is_empty());
    assert_eq!(fake.with(|s| s.mr_creates()), 1);
}

#[tokio::test]
async fn duplicate_is_uncertain_not_refused() {
    let (fake, api) = setup().await;
    create(&api).await.unwrap().unwrap();
    assert!(matches!(create(&api).await, Err(GhError::Failed(d)) if d.starts_with("409")));
    assert_eq!(fake.with(|s| s.mr_creates()), 2);
}

#[tokio::test]
async fn refusals_are_definitive() {
    let (fake, api) = setup().await;
    for status in [400, 403, 404, 422] {
        fake.with(|s| s.refuse_next_mr = Some(status));
        assert_eq!(create(&api).await.unwrap().unwrap_err(), status);
    }
    assert!(fake.with(|s| s.projects[&1].mrs.is_empty()));
    // A server error is not a refusal.
    fake.with(|s| s.refuse_next_mr = Some(500));
    assert!(matches!(create(&api).await, Err(GhError::Failed(_))));
}

#[tokio::test]
async fn lost_and_malformed_replies_are_uncertain() {
    let (fake, api) = setup().await;
    fake.with(|s| s.lose_next_mr_reply = true);
    assert!(matches!(create(&api).await, Err(GhError::Failed(_))));
    // The MR exists, so a retry finds it instead of creating another.
    let found = ForgeApi::Gitlab(&api)
        .find_open_pulls(TOKEN, &repo(), "agent/x", "main")
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0]["body"].as_str().unwrap().contains(MARKER));

    fake.with(|s| {
        s.projects.get_mut(&1).unwrap().mrs.clear();
        s.malformed_next_mr_reply = true;
    });
    assert!(matches!(create(&api).await, Err(GhError::Failed(d)) if d.contains("unreadable")));
}

#[tokio::test]
async fn rate_limit_is_reported() {
    let (fake, api) = setup().await;
    fake.with(|s| {
        s.fail.push_back(fake_gitlab::Failure {
            path_contains: "merge_requests".into(),
            status: 429,
            headers: vec![("retry-after".into(), "120".into())],
        });
    });
    assert!(matches!(create(&api).await, Err(GhError::Limited(_))));
}

#[tokio::test]
async fn fork_merge_requests_are_not_candidates() {
    let (fake, api) = setup().await;
    fake.with(|s| {
        let mut fork = Mr::new(5, "agent/x", &[]);
        fork.source_project_id = Some(99);
        s.projects.get_mut(&1).unwrap().mrs.insert(5, fork);
    });
    let found = ForgeApi::Gitlab(&api)
        .find_open_pulls(TOKEN, &repo(), "agent/x", "main")
        .await
        .unwrap();
    assert!(found.is_empty());
}

#[tokio::test]
async fn truncated_search_is_an_error() {
    let (fake, api) = setup().await;
    fake.with(|s| {
        let p = s.projects.get_mut(&1).unwrap();
        // More pages than the client reads; distinct targets keep them all "opened".
        for i in 1..=3000 {
            let mut m = Mr::new(i, "agent/x", &[]);
            m.target_branch = "main".into();
            p.mrs.insert(i, m);
        }
    });
    let result = ForgeApi::Gitlab(&api)
        .find_open_pulls(TOKEN, &repo(), "agent/x", "main")
        .await;
    assert!(matches!(result, Err(GhError::Failed(d)) if d.contains("too many")));
}

#[tokio::test]
async fn git_backed_branches_are_read_from_the_repository() {
    let (fake, api) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let bare = dir.path().join("bare.git");
    let run = |args: &[&str]| {
        let out = std::process::Command::new("/usr/bin/git")
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_DIR", &bare)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {:?}", out.stderr);
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    run(&["init", "--bare", "--quiet", "-b", "main"]);
    let empty = run(&["hash-object", "-t", "tree", "-w", "--stdin"]);
    let commit = run(&["commit-tree", &empty, "-m", "init"]);
    run(&["update-ref", "refs/heads/main", &commit]);
    run(&["update-ref", "refs/heads/agent/x", &commit]);
    fake.with(|s| s.projects.get_mut(&1).unwrap().git = Some(bare.clone()));
    let forge = ForgeApi::Gitlab(&api);
    assert_eq!(
        forge.ref_sha(TOKEN, &repo(), "agent/x").await.unwrap(),
        Some(commit.clone())
    );
    assert_eq!(forge.ref_sha(TOKEN, &repo(), "other").await.unwrap(), None);
    let pull = create(&api).await.unwrap().unwrap();
    assert_eq!(pull["head"]["sha"], commit.as_str());
    // A source branch the repository lacks is refused.
    let missing = forge
        .create_candidate_pull(TOKEN, &repo(), "t", "b", "other", "main")
        .await
        .unwrap();
    assert_eq!(missing.unwrap_err(), 400);
}

// ---- merge request heads and commit statuses ----

use agentscm::hub::github::{CommitStatus, StatusState};
use agentscm::hub::gitlab::{ListedStatus, MAX_STATUS_PAGES, MAX_TREE_PAGES};
use fake_gitlab::{FakeCommit, TreeEntry};
use std::collections::HashMap;

const CONTEXT: &str = "agentscm/workflow/main";

fn status(sha: &str, state: StatusState) -> CommitStatus {
    CommitStatus::new(sha, state, CONTEXT, "synthetic").unwrap()
}

async fn head(api: &Api, iid: i64) -> Result<Option<agentscm::hub::github::PullHead>, GhError> {
    ForgeApi::Gitlab(api)
        .pull_head(TOKEN, &repo(), iid, &mut HashMap::new())
        .await
}

fn entry(name: &str, kind: &str, mode: &str, n: usize) -> TreeEntry {
    TreeEntry {
        id: format!("{n:040x}"),
        name: name.into(),
        kind: kind.into(),
        mode: mode.into(),
    }
}

#[tokio::test]
async fn pull_head_decodes_the_merge_request() {
    let (fake, api) = setup().await;
    let parent = "a".repeat(40);
    let (sha, base_tip) = fake.with(|s| {
        let p = s.projects.get_mut(&1).unwrap();
        let mut mr = Mr::new(5, "agent/x", &[]);
        mr.source_project_id = Some(1);
        mr.sha = p.branches[1].sha.clone();
        let sha = mr.sha.clone();
        let mut fork = Mr::new(8, "agent/f", &[]);
        fork.source_project_id = Some(2);
        p.mrs.insert(8, fork);
        p.commits.insert(
            sha.clone(),
            FakeCommit {
                parent_ids: vec!["a".repeat(40)],
                tree: vec![entry("f.txt", "blob", "100644", 1)],
            },
        );
        p.mrs.insert(5, mr);
        let mut locked = Mr::new(6, "agent/y", &[]);
        locked.state = "locked".into();
        p.mrs.insert(6, locked);
        let mut merged = Mr::new(7, "agent/z", &[]);
        merged.state = "merged".into();
        merged.merge_commit_sha = Some("b".repeat(40));
        p.mrs.insert(7, merged);
        (sha, p.branches[0].sha.clone())
    });
    let pull = head(&api, 5).await.unwrap().unwrap();
    assert!(pull.open && !pull.merged);
    assert_eq!(pull.head_sha, sha);
    assert_eq!((pull.head_repo_id, pull.base_repo_id), (1, 1));
    assert_eq!((pull.base_ref.as_str(), pull.base_tip), ("main", base_tip));
    assert_eq!(pull.head_parents, vec![parent]);
    assert_eq!(pull.head_tree.len(), 40);

    // A fork's merge request is answered from the merge request alone.
    let before = fake.with(|s| s.requests.len());
    let fork = head(&api, 8).await.unwrap().unwrap();
    assert!(fork.open && fork.head_repo_id == 2 && fork.head_tree.is_empty());
    let paths = fake.with(|s| {
        s.requests[before..]
            .iter()
            .map(|r| r.path_and_query.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(paths.len(), 1, "{paths:?}");

    let err = head(&api, 6).await.unwrap_err();
    assert!(matches!(err, GhError::Failed(d) if d.contains("locked")));
    let merged = head(&api, 7).await.unwrap().unwrap();
    assert!(!merged.open && merged.merged);
    assert_eq!(merged.merge_commit, Some("b".repeat(40)));
    assert!(head(&api, 99).await.unwrap().is_none());

    // A head commit the forge cannot show is an error, not an empty tree.
    fake.with(|s| s.projects.get_mut(&1).unwrap().commits.clear());
    assert!(head(&api, 5).await.is_err());
}

#[tokio::test]
async fn root_tree_oid_matches_git() {
    let (fake, api) = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let (bare, work) = (dir.path().join("bare.git"), dir.path().join("work"));
    let run = |args: &[&str]| {
        let out = std::process::Command::new("/usr/bin/git")
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_DIR", &bare)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {:?}", out.stderr);
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    run(&["init", "--bare", "--quiet", "-b", "main"]);
    std::fs::create_dir_all(work.join("sub")).unwrap();
    std::fs::create_dir_all(work.join("sub-dir")).unwrap();
    // "sub.txt" sorts before the directory "sub", which sorts before "sub0"; "sub-dir" is before both.
    for path in ["a.txt", "sub.txt", "sub0", "sub/b.txt", "sub-dir/c.txt"] {
        std::fs::write(work.join(path), format!("{path}\n")).unwrap();
    }
    let wt = work.to_str().unwrap();
    run(&["--work-tree", wt, "add", "-A"]);
    run(&["--work-tree", wt, "commit", "--quiet", "-m", "one"]);
    let tip = run(&["rev-parse", "HEAD"]);
    run(&["update-ref", "refs/heads/agent/x", &tip]);
    let tree = run(&["rev-parse", &format!("{tip}^{{tree}}")]);
    fake.with(|s| {
        let p = s.projects.get_mut(&1).unwrap();
        p.git = Some(bare.clone());
        let mut mr = Mr::new(1, "agent/x", &[]);
        mr.sha = tip.clone();
        p.mrs.insert(1, mr);
    });
    let pull = head(&api, 1).await.unwrap().unwrap();
    assert_eq!(pull.head_tree, tree);
    assert!(pull.head_parents.is_empty());
}

#[tokio::test]
async fn oversized_or_malformed_tree_listing_fails_closed() {
    let (fake, api) = setup().await;
    let sha = fake.with(|s| {
        let p = s.projects.get_mut(&1).unwrap();
        let mut mr = Mr::new(1, "agent/x", &[]);
        mr.sha = p.branches[1].sha.clone();
        let sha = mr.sha.clone();
        p.mrs.insert(1, mr);
        let tree = (0..MAX_TREE_PAGES as usize * 100 + 1)
            .map(|n| entry(&format!("f{n}"), "blob", "100644", n))
            .collect();
        p.commits.insert(
            sha.clone(),
            FakeCommit {
                parent_ids: vec![],
                tree,
            },
        );
        sha
    });
    assert!(matches!(head(&api, 1).await, Err(GhError::Failed(d)) if d.contains("too large")));
    for bad in [
        entry("f", "blob", "100666", 1),
        entry("f", "tree", "100644", 1),
        entry("a/b", "blob", "100644", 1),
        TreeEntry {
            id: "xyz".into(),
            ..entry("f", "blob", "100644", 1)
        },
    ] {
        fake.with(|s| {
            let c = s
                .projects
                .get_mut(&1)
                .unwrap()
                .commits
                .get_mut(&sha)
                .unwrap();
            c.tree = vec![bad];
        });
        assert!(head(&api, 1).await.is_err());
    }
    // A duplicate name is not a tree.
    fake.with(|s| {
        let c = s
            .projects
            .get_mut(&1)
            .unwrap()
            .commits
            .get_mut(&sha)
            .unwrap();
        c.tree = vec![
            entry("f", "blob", "100644", 1),
            entry("f", "blob", "100644", 2),
        ];
    });
    assert!(head(&api, 1).await.is_err());
}

async fn post(api: &Api, sha: &str, state: StatusState) -> Result<Result<(), u16>, GhError> {
    ForgeApi::Gitlab(api)
        .post_commit_status(TOKEN, &repo(), &status(sha, state), "agent/x")
        .await
}

async fn listed(api: &Api, sha: &str) -> Result<Option<ListedStatus>, GhError> {
    ForgeApi::Gitlab(api)
        .commit_statuses(TOKEN, &repo(), sha, CONTEXT, "agent/x")
        .await
}

#[tokio::test]
async fn status_post_and_read_back() {
    let (fake, api) = setup().await;
    let sha = "c".repeat(40);
    assert_eq!(listed(&api, &sha).await.unwrap(), None);
    post(&api, &sha, StatusState::Pending)
        .await
        .unwrap()
        .unwrap();
    // Only a status still pending refuses its own state again.
    assert_eq!(
        post(&api, &sha, StatusState::Pending).await.unwrap(),
        Err(400)
    );
    post(&api, &sha, StatusState::Error).await.unwrap().unwrap();
    let latest = listed(&api, &sha).await.unwrap().unwrap();
    assert_eq!(
        (latest.state.as_str(), latest.description.as_str()),
        ("failed", "synthetic")
    );
    // Other refs and other commits are not this status.
    let other = ForgeApi::Gitlab(&api)
        .commit_statuses(TOKEN, &repo(), &sha, CONTEXT, "agent/other")
        .await
        .unwrap();
    assert_eq!(other, None);
    // A finished status is not reused: reposting its state makes a new status.
    post(&api, &sha, StatusState::Failure)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fake.with(|s| s.projects[&1].statuses.len()), 3);
}

#[tokio::test]
async fn status_post_failures_are_classified() {
    let (fake, api) = setup().await;
    let sha = "d".repeat(40);
    fake.with(|s| s.refuse_next_status = Some(403));
    assert_eq!(
        post(&api, &sha, StatusState::Success).await.unwrap(),
        Err(403)
    );
    assert_eq!(fake.with(|s| s.projects[&1].statuses.len()), 0);
    fake.with(|s| s.limit_next_status = true);
    assert!(matches!(
        post(&api, &sha, StatusState::Success).await,
        Err(GhError::Limited(_))
    ));
    fake.with(|s| s.lose_next_status_reply = true);
    assert!(matches!(
        post(&api, &sha, StatusState::Success).await,
        Err(GhError::Failed(_))
    ));
    // The lost reply stored the status; the list shows it.
    assert_eq!(listed(&api, &sha).await.unwrap().unwrap().state, "success");
    // Unknown project, as the fake's 404, is a refusal.
    fake.with(|s| {
        s.projects.get_mut(&1).unwrap().visibility = "private".into();
        s.tokens.get_mut(TOKEN).unwrap().access.clear();
    });
    assert_eq!(
        post(&api, &sha, StatusState::Pending).await.unwrap(),
        Err(404)
    );
    assert!(
        ForgeApi::Gitlab(&api)
            .commit_statuses(TOKEN, &repo(), "xyz", CONTEXT, "agent/x")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn status_list_is_bounded() {
    let (fake, api) = setup().await;
    let sha = "e".repeat(40);
    fake.with(|s| {
        let p = s.projects.get_mut(&1).unwrap();
        for n in 0..MAX_STATUS_PAGES as i64 * 100 + 1 {
            p.statuses.push((
                sha.clone(),
                serde_json::json!({"id": n + 1, "state": "success", "name": CONTEXT,
                    "description": "", "ref": "agent/x"}),
            ));
        }
    });
    assert!(matches!(listed(&api, &sha).await, Err(GhError::Failed(d)) if d.contains("too many")));
}

#[tokio::test]
async fn a_head_lagging_the_source_branch_is_an_error() {
    let (fake, api) = setup().await;
    let tip = fake.with(|s| {
        let p = s.projects.get_mut(&1).unwrap();
        let mut mr = Mr::new(1, "agent/x", &[]);
        mr.sha = p.branches[1].sha.clone();
        p.commits.insert(
            mr.sha.clone(),
            FakeCommit {
                parent_ids: vec![],
                tree: vec![entry("f.txt", "blob", "100644", 1)],
            },
        );
        p.mrs.insert(1, mr);
        p.branches[1].sha.clone()
    });
    assert_eq!(head(&api, 1).await.unwrap().unwrap().head_sha, tip);
    // The source branch moved on; the merge request still reports the old head.
    fake.with(|s| s.projects.get_mut(&1).unwrap().branches[1].sha = "c".repeat(40));
    assert!(matches!(head(&api, 1).await, Err(GhError::Failed(d)) if d.contains("lags")));
    // A missing source branch is not a match either.
    fake.with(|s| s.projects.get_mut(&1).unwrap().branches.truncate(1));
    assert!(matches!(head(&api, 1).await, Err(GhError::Failed(d)) if d.contains("lags")));
}
