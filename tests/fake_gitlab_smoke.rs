//! Sanity checks for the in-process fake GitLab used by the hub sync tests.
#[path = "support/fake_gitlab.rs"]
mod fake_gitlab;

use fake_gitlab::{Branch, FakeGitlab, Mr, Project, Token};
use serde_json::{Value, json};

async fn call(
    method: reqwest::Method,
    url: String,
    token: Option<&str>,
    body: Option<Value>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new().request(method, url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    request.send().await.unwrap()
}

async fn get(url: String, token: &str) -> reqwest::Response {
    call(reqwest::Method::GET, url, Some(token), None).await
}

trait Json {
    async fn val<T: serde::de::DeserializeOwned>(self) -> T;
}

impl Json for reqwest::Response {
    async fn val<T: serde::de::DeserializeOwned>(self) -> T {
        serde_json::from_str(&self.text().await.unwrap()).unwrap()
    }
}

fn header(r: &reqwest::Response, name: &str) -> String {
    r.headers()[name].to_str().unwrap().to_string()
}

fn token(name: &str, id: i64, access: &[(i64, u32)]) -> Token {
    Token {
        username: name.into(),
        user_id: id,
        access: access.iter().copied().collect(),
    }
}

#[tokio::test]
async fn auth_and_access_rules() {
    let fake = FakeGitlab::start().await;
    fake.with(|s| {
        s.projects
            .insert(1, Project::new(1, "group/sub/secret", "private"));
        s.projects
            .insert(2, Project::new(2, "group/open", "internal"));
        s.tokens
            .insert("t-member".into(), token("alice", 10, &[(1, 30)]));
        s.tokens.insert("t-other".into(), token("bob", 11, &[]));
    });
    let api = format!("{}/api/v4", fake.base);
    assert!(fake.base.ends_with("/gitlab"));

    assert_eq!(
        call(reqwest::Method::GET, format!("{api}/user"), None, None)
            .await
            .status(),
        401
    );
    assert_eq!(get(format!("{api}/user"), "nope").await.status(), 401);
    let me: Value = get(format!("{api}/user"), "t-member").await.val().await;
    assert_eq!(
        (me["id"].as_i64(), me["username"].as_str()),
        (Some(10), Some("alice"))
    );

    // Private: member by id and by url-encoded path; stranger gets 404.
    let p: Value = get(format!("{api}/projects/1"), "t-member")
        .await
        .val()
        .await;
    assert_eq!(p["permissions"]["project_access"]["access_level"], 30);
    assert!(p["permissions"]["group_access"].is_null());
    let by_path = get(format!("{api}/projects/group%2Fsub%2Fsecret"), "t-member").await;
    assert_eq!(by_path.status(), 200);
    assert_eq!(
        get(format!("{api}/projects/1"), "t-other").await.status(),
        404
    );
    assert_eq!(
        get(format!("{api}/projects/1/merge_requests"), "t-other")
            .await
            .status(),
        404
    );

    // Internal: readable by any valid token, with no project access entry.
    let p: Value = get(format!("{api}/projects/2"), "t-other")
        .await
        .val()
        .await;
    assert!(p["permissions"]["project_access"].is_null());
    assert_eq!(get(format!("{api}/nothing"), "t-other").await.status(), 404);
    assert_eq!(fake.with(|s| s.requests.len()), 9);
}

#[tokio::test]
async fn pagination_headers_walk() {
    let fake = FakeGitlab::start().await;
    fake.with(|s| {
        let mut project = Project::new(1, "g/p", "public");
        for iid in 1..=5 {
            project.mrs.insert(
                iid,
                Mr::new(iid, &format!("b{iid}"), &["a.rs", "b.rs", "c.rs"]),
            );
        }
        project.mrs.get_mut(&5).unwrap().state = "merged".into();
        for (i, name) in ["old", "feat/new", "mid"].iter().enumerate() {
            let date = [
                "2026-01-01T00:00:00Z",
                "2026-03-01T00:00:00Z",
                "2026-02-01T00:00:00Z",
            ][i];
            project.branches.push(Branch::new(name, date));
        }
        s.projects.insert(1, project);
        s.tokens.insert("t".into(), token("alice", 10, &[]));
    });
    let api = format!("{}/api/v4/projects/1", fake.base);

    // Four opened MRs, newest iid first, two per page.
    let mut iids = Vec::new();
    let mut page = "1".to_string();
    while !page.is_empty() {
        let r = get(
            format!("{api}/merge_requests?state=opened&per_page=2&page={page}"),
            "t",
        )
        .await;
        assert_eq!(
            (
                header(&r, "x-total").as_str(),
                header(&r, "x-total-pages").as_str()
            ),
            ("4", "2")
        );
        assert_eq!(header(&r, "x-page"), page);
        let link = header(&r, "link");
        assert_eq!(link.contains("rel=\"next\""), page == "1");
        assert!(link.contains("rel=\"last\""));
        let next = header(&r, "x-next-page");
        let items: Vec<Value> = r.val().await;
        iids.extend(items.iter().map(|m| m["iid"].as_i64().unwrap()));
        page = next;
    }
    assert_eq!(iids, [4, 3, 2, 1]);

    let r = get(
        format!("{api}/merge_requests/3/diffs?per_page=2&page=2"),
        "t",
    )
    .await;
    assert_eq!(header(&r, "x-next-page"), "");
    let files: Vec<Value> = r.val().await;
    assert_eq!(files, [json!({"old_path": "c.rs", "new_path": "c.rs"})]);

    let r = get(
        format!("{api}/repository/branches?sort=updated_desc&per_page=2&page=1"),
        "t",
    )
    .await;
    assert_eq!(header(&r, "x-next-page"), "2");
    let names: Vec<String> = r
        .val::<Vec<Value>>()
        .await
        .iter()
        .map(|b| b["name"].as_str().unwrap().into())
        .collect();
    assert_eq!(names, ["feat/new", "mid"]);

    // Branch names containing `/` arrive percent-encoded.
    let b: Value = get(format!("{api}/repository/branches/feat%2Fnew"), "t")
        .await
        .val()
        .await;
    assert_eq!(b["commit"]["committed_date"], "2026-03-01T00:00:00Z");
    assert_eq!(
        get(format!("{api}/repository/branches/missing"), "t")
            .await
            .status(),
        404
    );
    let m: Value = get(format!("{api}/merge_requests/2"), "t")
        .await
        .val()
        .await;
    assert_eq!(
        (m["state"].as_str(), m["source_branch"].as_str()),
        (Some("opened"), Some("b2"))
    );
}

#[tokio::test]
async fn note_lost_reply_and_author_check() {
    let fake = FakeGitlab::start().await;
    fake.with(|s| {
        s.projects.insert(1, Project::new(1, "g/p", "public"));
        s.tokens.insert("t-a".into(), token("alice", 10, &[]));
        s.tokens.insert("t-b".into(), token("bob", 11, &[]));
        s.lose_next_note_reply = true;
    });
    let notes = format!("{}/api/v4/projects/1/merge_requests/7/notes", fake.base);

    // The note is stored although the reply is lost.
    let lost = call(
        reqwest::Method::POST,
        notes.clone(),
        Some("t-a"),
        Some(json!({"body": "first"})),
    )
    .await;
    assert_eq!(lost.status(), 502);
    let listed: Vec<Value> = get(notes.clone(), "t-a").await.val().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["author"]["username"], "alice");
    let id = listed[0]["id"].as_i64().unwrap();

    // The next create succeeds normally.
    let ok = call(
        reqwest::Method::POST,
        notes.clone(),
        Some("t-a"),
        Some(json!({"body": "second"})),
    )
    .await;
    assert_eq!(ok.status(), 201);

    let put = |token: &'static str, body: &'static str| {
        call(
            reqwest::Method::PUT,
            format!("{notes}/{id}"),
            Some(token),
            Some(json!({"body": body})),
        )
    };
    assert_eq!(put("t-b", "hijack").await.status(), 403);
    assert_eq!(put("t-a", "edited").await.status(), 200);
    assert_eq!(
        fake.with(|s| s.projects[&1].notes[&7][0].body.clone()),
        "edited"
    );

    let status = call(
        reqwest::Method::POST,
        format!("{}/api/v4/projects/1/statuses/abc", fake.base),
        Some("t-a"),
        Some(json!({"state": "success", "name": "agentscm", "description": "ok"})),
    )
    .await;
    assert_eq!(status.status(), 201);
    assert_eq!(fake.with(|s| s.projects[&1].statuses.len()), 1);
}
