//! Sanity checks for the in-process fake GitHub used by the hub sync tests.
#[path = "support/fake_github.rs"]
mod fake_github;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use fake_github::{FakeGithub, Pull, Repo, private_pem};
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
