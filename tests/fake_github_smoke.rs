//! Sanity checks for the in-process fake GitHub used by the hub sync tests.
#[path = "support/fake_github.rs"]
mod fake_github;

use agentscm::hub::github::{Api, CommitStatus, GhError, StatusState};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use fake_github::{Branch, FakeGithub, Pull, Repo, private_pem};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};

fn jwt(iss: Value, iat: i64, exp: i64, tamper: bool) -> String {
    let pem: String = private_pem()
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let pair = RsaKeyPair::from_der(&STANDARD.decode(pem).unwrap()).unwrap();
    let part = |v: Value| URL_SAFE_NO_PAD.encode(v.to_string());
    let input = format!(
        "{}.{}",
        part(json!({"alg": "RS256", "typ": "JWT"})),
        part(json!({"iss": iss, "iat": iat, "exp": exp}))
    );
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        input.as_bytes(),
        &mut signature,
    )
    .unwrap();
    if tamper {
        signature[0] ^= 1;
    }
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

async fn get(url: &str, auth: &str, etag: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .get(url)
        .header("authorization", auth);
    if let Some(etag) = etag {
        request = request.header("if-none-match", etag);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn app_jwt_accepted_and_rejected() {
    let fake = FakeGithub::start().await;
    fake.with(|s| {
        s.installations.push(fake_github::Installation {
            id: 7,
            account: "acme".into(),
            repos: vec![],
        })
    });
    let url = format!("{}/app/installations", fake.base);
    let t = now();
    let ok = jwt(
        json!(fake_github::APP_ID.to_string()),
        t - 30,
        t + 300,
        false,
    );
    let response = get(&url, &format!("Bearer {ok}"), None).await;
    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(body, json!([{"id": 7, "account": {"login": "acme"}}]));
    // numeric iss is accepted too
    let numeric = jwt(json!(fake_github::APP_ID), t - 30, t + 300, false);
    assert_eq!(
        get(&url, &format!("Bearer {numeric}"), None).await.status(),
        200
    );

    for (name, bad) in [
        (
            "tampered",
            jwt(json!(fake_github::APP_ID), t - 30, t + 300, true),
        ),
        (
            "expired",
            jwt(json!(fake_github::APP_ID), t - 700, t - 100, false),
        ),
        (
            "future iat",
            jwt(json!(fake_github::APP_ID), t + 600, t + 900, false),
        ),
        ("wrong iss", jwt(json!(1), t - 30, t + 300, false)),
    ] {
        assert_eq!(
            get(&url, &format!("Bearer {bad}"), None).await.status(),
            401,
            "{name}"
        );
    }
    assert_eq!(get(&url, "Bearer nope", None).await.status(), 401);
    let token = reqwest::Client::new()
        .post(format!("{}/app/installations/7/access_tokens", fake.base))
        .header("authorization", format!("Bearer {ok}"))
        .send()
        .await
        .unwrap();
    assert_eq!(token.status(), 201);
    let token: Value = serde_json::from_str(&token.text().await.unwrap()).unwrap();
    assert!(token["token"].as_str().unwrap().starts_with("ghs_"));
}

fn with_pulls(fake: &FakeGithub, count: u64) {
    fake.with(|s| {
        let mut repo = Repo::new(1, "Acme/Widgets");
        for n in 1..=count {
            repo.pulls
                .insert(n, Pull::new(n, &format!("feature/{n}"), &["a.rs"]));
        }
        s.repos.insert(1, repo);
        s.pats
            .insert("pat".into(), ("octocat".into(), 1, [1].into()));
    });
}

#[tokio::test]
async fn pulls_etag_returns_304() {
    let fake = FakeGithub::start().await;
    with_pulls(&fake, 2);
    let url = format!("{}/repos/acme/widgets/pulls?state=open", fake.base);
    let first = get(&url, "Bearer pat", None).await;
    assert_eq!(first.status(), 200);
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let body: Value = serde_json::from_str(&first.text().await.unwrap()).unwrap();
    assert_eq!(body[0]["number"], 2, "newest first");
    let second = get(&url, "Bearer pat", Some(&etag)).await;
    assert_eq!(second.status(), 304);
    assert!(second.text().await.unwrap().is_empty());
    // a change in the open set invalidates the etag
    fake.with(|s| {
        s.repos
            .get_mut(&1)
            .unwrap()
            .pulls
            .get_mut(&1)
            .unwrap()
            .state = "closed".into()
    });
    assert_eq!(get(&url, "Bearer pat", Some(&etag)).await.status(), 200);
    // no access to a private repo is a 404
    fake.with(|s| {
        s.pats
            .insert("other".into(), ("x".into(), 2, Default::default()))
    });
    assert_eq!(get(&url, "Bearer other", None).await.status(), 404);
}

#[tokio::test]
async fn pulls_paginate_with_link_header() {
    let fake = FakeGithub::start().await;
    with_pulls(&fake, 5);
    let mut url = format!(
        "{}/repos/acme/widgets/pulls?state=open&per_page=2",
        fake.base
    );
    let mut seen = vec![];
    for _ in 0..5 {
        let response = get(&url, "Bearer pat", None).await;
        let link = response
            .headers()
            .get("link")
            .map(|v| v.to_str().unwrap().to_string());
        let page: Vec<Value> = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        seen.extend(page.iter().map(|p| p["number"].as_u64().unwrap()));
        let Some(link) = link else { break };
        assert!(
            link.contains("rel=\"last\"") && link.contains("page=3>; rel=\"last\""),
            "{link}"
        );
        let next = link
            .split(", ")
            .find(|l| l.ends_with("rel=\"next\""))
            .unwrap();
        url = next[1..next.find('>').unwrap()].to_string();
    }
    assert_eq!(seen, vec![5, 4, 3, 2, 1]);
}

const CONTEXT: &str = "agentscm/workflow/main";

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

fn api(fake: &FakeGithub) -> Api {
    Api::new(&fake.base, &fake.base).unwrap()
}

#[tokio::test]
async fn commit_statuses_are_recorded_exactly_and_validated_first() {
    let fake = FakeGithub::start().await;
    with_pulls(&fake, 1);
    let api = api(&fake);
    for (state, name) in [
        (StatusState::Pending, "pending"),
        (StatusState::Success, "success"),
        (StatusState::Failure, "failure"),
        (StatusState::Error, "error"),
    ] {
        let status = CommitStatus::new(&sha('a'), state, CONTEXT, &format!("is {name}")).unwrap();
        assert!(matches!(
            api.post_commit_status("pat", "Acme/Widgets", &status).await,
            Ok(Ok(()))
        ));
    }
    let recorded = fake.with(|s| s.repos[&1].statuses.clone());
    let states: Vec<_> = recorded
        .iter()
        .map(|(at, v)| {
            (
                at.as_str(),
                v["state"].as_str().unwrap(),
                v["context"].as_str().unwrap(),
                v["description"].as_str().unwrap(),
            )
        })
        .collect();
    let a = sha('a');
    assert_eq!(
        states,
        vec![
            (a.as_str(), "pending", CONTEXT, "is pending"),
            (a.as_str(), "success", CONTEXT, "is success"),
            (a.as_str(), "failure", CONTEXT, "is failure"),
            (a.as_str(), "error", CONTEXT, "is error"),
        ]
    );
    for (bad_sha, context, description) in [
        ("abc", CONTEXT, "ok"),
        (&"g".repeat(40), CONTEXT, "ok"),
        (&sha('a'), CONTEXT, &"x".repeat(141)),
        (&sha('a'), "", "ok"),
        (&sha('a'), CONTEXT, "line\nbreak"),
    ] {
        assert!(CommitStatus::new(bad_sha, StatusState::Success, context, description).is_err());
    }
    assert!(CommitStatus::new(&sha('a'), StatusState::Success, CONTEXT, &"x".repeat(140)).is_ok());
    assert_eq!(fake.with(|s| s.repos[&1].statuses.len()), 4);
}

#[tokio::test]
async fn status_failures_are_classified() {
    let fake = FakeGithub::start().await;
    with_pulls(&fake, 1);
    let api = api(&fake);
    let status = CommitStatus::new(&sha('b'), StatusState::Success, CONTEXT, "ok").unwrap();
    // The reply is lost after the status is recorded: uncertain, and the status exists.
    fake.with(|s| s.lose_next_status_reply = true);
    assert!(matches!(
        api.post_commit_status("pat", "Acme/Widgets", &status).await,
        Err(GhError::Failed(_))
    ));
    assert_eq!(fake.with(|s| s.repos[&1].statuses.len()), 1);
    // A 5xx before recording is also uncertain.
    fake.with(|s| {
        s.fail.push_back(fake_github::Failure {
            path_contains: "/statuses/".into(),
            status: 503,
            headers: vec![],
        })
    });
    assert!(matches!(
        api.post_commit_status("pat", "Acme/Widgets", &status).await,
        Err(GhError::Failed(_))
    ));
    // Rate limits and bad credentials are definite non-effects.
    fake.with(|s| {
        s.fail.push_back(fake_github::Failure {
            path_contains: "/statuses/".into(),
            status: 429,
            headers: vec![("retry-after".into(), "120".into())],
        })
    });
    assert!(matches!(
        api.post_commit_status("pat", "Acme/Widgets", &status).await,
        Err(GhError::Limited(_))
    ));
    fake.with(|s| {
        s.fail.push_back(fake_github::Failure {
            path_contains: "/statuses/".into(),
            status: 401,
            headers: vec![],
        })
    });
    assert!(matches!(
        api.post_commit_status("pat", "Acme/Widgets", &status).await,
        Err(GhError::Auth(_))
    ));
    // A refused write is a definite refusal.
    fake.with(|s| {
        s.fail.push_back(fake_github::Failure {
            path_contains: "/statuses/".into(),
            status: 422,
            headers: vec![],
        })
    });
    assert!(matches!(
        api.post_commit_status("pat", "Acme/Widgets", &status).await,
        Ok(Err(422))
    ));
    assert_eq!(fake.with(|s| s.repos[&1].statuses.len()), 1);
}

#[tokio::test]
async fn pull_head_reads_ids_live_base_tip_tree_and_parents() {
    let fake = FakeGithub::start().await;
    with_pulls(&fake, 2);
    let (head, tree, parent, tip) = (sha('1'), sha('2'), sha('3'), sha('4'));
    fake.with(|s| {
        let repo = s.repos.get_mut(&1).unwrap();
        let pull = repo.pulls.get_mut(&1).unwrap();
        pull.head_sha = head.clone();
        pull.base_repo_id = Some(1);
        repo.commits
            .insert(head.clone(), (tree.clone(), vec![parent.clone()]));
        repo.branches.push(Branch {
            name: "main".into(),
            sha: tip.clone(),
            committed_at: "2026-01-01T00:00:00Z".into(),
        });
        // A fork pull request and a deleted fork.
        repo.pulls.get_mut(&2).unwrap().head_repo = Some(Some(77));
    });
    let api = api(&fake);
    let read = api
        .pull_head("pat", "Acme/Widgets", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            read.open,
            read.head_sha.as_str(),
            read.head_repo_id,
            read.base_repo_id
        ),
        (true, head.as_str(), 1, 1)
    );
    assert_eq!(
        (
            read.base_ref.as_str(),
            read.base_tip.as_str(),
            read.head_tree.as_str()
        ),
        ("main", tip.as_str(), tree.as_str())
    );
    assert_eq!(read.head_parents, vec![parent.clone()]);

    // The fork's head commit is unseeded: that is an error, not a default.
    assert!(matches!(
        api.pull_head("pat", "Acme/Widgets", 2).await,
        Err(GhError::Failed(_))
    ));
    let fork_head = fake.with(|s| s.repos[&1].pulls[&2].head_sha.clone());
    fake.with(|s| {
        s.repos
            .get_mut(&1)
            .unwrap()
            .commits
            .insert(fork_head, (tree.clone(), vec![]));
    });
    let fork = api
        .pull_head("pat", "Acme/Widgets", 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((fork.head_repo_id, fork.base_repo_id), (77, 1));
    assert!(fork.head_parents.is_empty());

    // A deleted head repository (null) is id 0, which matches no connected repository.
    fake.with(|s| {
        s.repos
            .get_mut(&1)
            .unwrap()
            .pulls
            .get_mut(&2)
            .unwrap()
            .head_repo = Some(None)
    });
    let gone = api
        .pull_head("pat", "Acme/Widgets", 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gone.head_repo_id, 0);

    // A pull that is not there, and a pull without repository objects.
    assert!(
        api.pull_head("pat", "Acme/Widgets", 9)
            .await
            .unwrap()
            .is_none()
    );
    fake.with(|s| s.omit_pull_repos = true);
    assert!(matches!(
        api.pull_head("pat", "Acme/Widgets", 1).await,
        Err(GhError::Failed(_))
    ));
    // A moved live base is read, not the pull's recorded base.
    fake.with(|s| {
        s.omit_pull_repos = false;
        s.repos.get_mut(&1).unwrap().branches[0].sha = sha('5');
    });
    let moved = api
        .pull_head("pat", "Acme/Widgets", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.base_tip, sha('5'));
}
