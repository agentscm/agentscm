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
