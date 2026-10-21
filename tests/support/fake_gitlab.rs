//! In-process fake of the self-managed GitLab REST surface the hub sync uses. Synthetic data only.
//! Include with `#[path = "support/fake_gitlab.rs"] mod fake_gitlab;`.
#![allow(dead_code)] // each including test uses a different subset
#![allow(clippy::collapsible_if)] // test fake: clarity over lint style
#![allow(clippy::result_large_err)] // test fake: the Err is a ready HTTP response

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const WEBHOOK_TOKEN: &str = "gitlab-hook-synthetic";
const PREFIX: &str = "/gitlab";

/// POSTs a webhook delivery to the hub and returns the HTTP status.
pub async fn deliver(
    hub: &str,
    connection: &str,
    event: &str,
    uuid: &str,
    body: &Value,
    token: &str,
) -> u16 {
    let bytes = serde_json::to_vec(body).unwrap();
    match reqwest::Client::new()
        .post(format!("{hub}/v1/hub/webhooks/gitlab/{connection}"))
        .header("content-type", "application/json")
        .header("x-gitlab-event", event)
        .header("x-gitlab-event-uuid", uuid)
        .header("x-gitlab-token", token)
        .body(bytes)
        .send()
        .await
    {
        Ok(response) => response.status().as_u16(),
        // A hub that rejects an oversize body may close while the client is still writing it:
        // the reset stands for the 413 it was about to send.
        Err(error) if !error.is_connect() => 413,
        Err(error) => panic!("webhook delivery failed: {error}"),
    }
}

// ---- state ----

#[derive(Clone, Debug)]
pub struct Token {
    pub username: String,
    pub user_id: i64,
    /// Project id to access level (10, 20, 30, 40, 50).
    pub access: HashMap<i64, u32>,
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub path_contains: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path_and_query: String,
    /// The raw `Authorization` header value.
    pub auth: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Note {
    pub id: i64,
    pub body: String,
    pub author_username: String,
}

#[derive(Clone, Debug)]
pub struct Branch {
    pub name: String,
    pub sha: String,
    /// RFC 3339.
    pub committed_date: String,
}

impl Branch {
    pub fn new(name: &str, committed_date: &str) -> Self {
        Branch {
            name: name.into(),
            sha: fake_sha(name),
            committed_date: committed_date.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Mr {
    pub iid: i64,
    pub id: i64,
    pub title: String,
    /// opened | closed | merged
    pub state: String,
    pub draft: bool,
    pub source_branch: String,
    pub target_branch: String,
    pub sha: String,
    pub merge_commit_sha: Option<String>,
    pub author_username: String,
    pub author_id: i64,
    pub files: Vec<String>,
    /// (old_path, new_path) entries added to the diffs after `files`.
    pub renamed: Vec<(String, String)>,
    pub description: String,
    /// The source project when it is not the one the MR lives in (a fork).
    pub source_project_id: Option<i64>,
}

impl Mr {
    /// Opened, non-draft MR into `main` by `tanuki`; ids and sha derive from the inputs.
    pub fn new(iid: i64, source_branch: &str, files: &[&str]) -> Self {
        Mr {
            iid,
            id: 9000 + iid,
            title: format!("Merge request {iid}"),
            state: "opened".into(),
            draft: false,
            source_branch: source_branch.into(),
            target_branch: "main".into(),
            sha: fake_sha(&format!("{source_branch}#{iid}")),
            merge_commit_sha: None,
            author_username: "tanuki".into(),
            author_id: 1,
            files: files.iter().map(|f| f.to_string()).collect(),
            renamed: Vec::new(),
            description: String::new(),
            source_project_id: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Project {
    pub id: i64,
    pub path_with_namespace: String,
    /// private | internal | public
    pub visibility: String,
    pub mrs: BTreeMap<i64, Mr>,
    pub branches: Vec<Branch>,
    /// MR iid to notes.
    pub notes: BTreeMap<i64, Vec<Note>>,
    /// (sha, {state, name, description}) in creation order.
    pub statuses: Vec<(String, Value)>,
    pub default_branch: Option<String>,
    /// A bare Git repository behind the branch reads and MR creation, if any.
    pub git: Option<std::path::PathBuf>,
}

impl Project {
    pub fn new(id: i64, path_with_namespace: &str, visibility: &str) -> Self {
        Project {
            id,
            path_with_namespace: path_with_namespace.into(),
            visibility: visibility.into(),
            mrs: BTreeMap::new(),
            branches: Vec::new(),
            notes: BTreeMap::new(),
            statuses: Vec::new(),
            default_branch: Some("main".into()),
            git: None,
        }
    }
}

#[derive(Default)]
pub struct FakeState {
    pub projects: BTreeMap<i64, Project>,
    /// Bearer token to its user and per-project access.
    pub tokens: HashMap<String, Token>,
    /// The first entry whose `path_contains` matches a request is consumed and answered.
    pub fail: VecDeque<Failure>,
    /// Store the next created note, then answer 502 with no body.
    pub lose_next_note_reply: bool,
    /// Store the next created MR, then answer 502 with no body.
    pub lose_next_mr_reply: bool,
    /// Store the next created MR, then answer 201 with a body that is not an MR.
    pub malformed_next_mr_reply: bool,
    /// Answer the next MR create with this status and store nothing.
    pub refuse_next_mr: Option<u16>,
    /// Answer the next MR list with 200 and a JSON object instead of an array.
    pub malformed_next_mr_list: bool,
    /// Answer every MR list with items that carry no project ids, until cleared.
    pub omit_mr_list_project_ids: bool,
    pub requests: Vec<Request>,
}

impl FakeState {
    /// How many MR creates (`POST .../merge_requests`) reached the fake, however they were answered.
    pub fn mr_creates(&self) -> usize {
        self.requests
            .iter()
            .filter(|r| {
                r.method == "POST"
                    && r.path_and_query
                        .split('?')
                        .next()
                        .is_some_and(|p| p.ends_with("/merge_requests"))
            })
            .count()
    }
}

/// Runs Git in a bare repository with no user configuration; None on failure.
pub fn git_text(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/git")
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_DIR", dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn fake_sha(seed: &str) -> String {
    Sha256::digest(seed.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..40]
        .to_string()
}

// ---- server ----

pub struct FakeGitlab {
    /// `http://127.0.0.1:PORT/gitlab`; the API lives at `{base}/api/v4`.
    pub base: String,
    pub state: Arc<Mutex<FakeState>>,
}

#[derive(Clone)]
struct Ctx {
    base: String,
    state: Arc<Mutex<FakeState>>,
}

impl FakeGitlab {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}{PREFIX}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(FakeState::default()));
        let ctx = Ctx {
            base: base.clone(),
            state: state.clone(),
        };
        let app = Router::new().fallback(entry).with_state(ctx);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        FakeGitlab { base, state }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }
}

async fn entry(
    State(ctx): State<Ctx>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(&ctx, &method, &uri, &headers, &body)
}

fn reply(status: u16, headers: &[(String, String)], body: Option<Value>) -> Response {
    let mut response = Response::new(match &body {
        Some(value) => Body::from(value.to_string()),
        None => Body::empty(),
    });
    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    if body.is_some() {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    for (name, value) in headers {
        response.headers_mut().insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    response
}

fn json_reply(status: u16, body: Value) -> Response {
    reply(status, &[], Some(body))
}

fn not_found() -> Response {
    json_reply(404, json!({"message": "404 Not Found"}))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(uri: &Uri) -> Vec<(String, String)> {
    uri.query()
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// One page of `items` plus GitLab's `x-*` pagination headers and a `Link` header.
fn paginate(
    ctx: &Ctx,
    uri: &Uri,
    query: &[(String, String)],
    items: Vec<Value>,
) -> (Vec<Value>, Vec<(String, String)>) {
    let get = |k: &str| {
        query
            .iter()
            .find(|(n, _)| n == k)
            .and_then(|(_, v)| v.parse::<usize>().ok())
    };
    let per_page = get("per_page").unwrap_or(20).clamp(1, 100);
    let page = get("page").unwrap_or(1).max(1);
    let total = items.len();
    let last = total.div_ceil(per_page).max(1);
    let rest: String = query
        .iter()
        .filter(|(k, _)| k != "page" && k != "per_page")
        .map(|(k, v)| format!("{k}={v}&"))
        .collect();
    let host = ctx.base.strip_suffix(PREFIX).unwrap_or(&ctx.base);
    let link = |p: usize, rel: &str| {
        format!(
            "<{host}{}?{rest}page={p}&per_page={per_page}>; rel=\"{rel}\"",
            uri.path()
        )
    };
    let next = if page < last {
        (page + 1).to_string()
    } else {
        String::new()
    };
    let mut links = Vec::new();
    if page < last {
        links.push(link(page + 1, "next"));
    }
    links.push(link(last, "last"));
    let headers = vec![
        ("x-total".to_string(), total.to_string()),
        ("x-total-pages".to_string(), last.to_string()),
        ("x-next-page".to_string(), next),
        ("x-page".to_string(), page.to_string()),
        ("x-per-page".to_string(), per_page.to_string()),
        ("link".to_string(), links.join(", ")),
    ];
    (
        items
            .into_iter()
            .skip((page - 1) * per_page)
            .take(per_page)
            .collect(),
        headers,
    )
}

fn can_read(project: &Project, token: &Token) -> bool {
    project.visibility != "private" || token.access.contains_key(&project.id)
}

/// Resolves a numeric id or a url-encoded path; 404 when missing or unreadable.
fn find_project<'a>(
    state: &'a FakeState,
    token: &Token,
    key: &str,
) -> Result<&'a Project, Response> {
    let by_id = key.parse::<i64>().ok();
    state
        .projects
        .values()
        .find(|p| by_id == Some(p.id) || p.path_with_namespace == key)
        .filter(|p| can_read(p, token))
        .ok_or_else(not_found)
}

fn project_json(ctx: &Ctx, p: &Project, token: &Token) -> Value {
    let level = token.access.get(&p.id);
    json!({
        "id": p.id, "path_with_namespace": p.path_with_namespace, "visibility": p.visibility,
        "default_branch": p.default_branch,
        "web_url": format!("{}/{}", ctx.base, p.path_with_namespace),
        "permissions": {
            "project_access": level.map(|l| json!({"access_level": l})),
            "group_access": null,
        },
    })
}

fn mr_json(ctx: &Ctx, p: &Project, m: &Mr) -> Value {
    json!({
        "id": m.id, "iid": m.iid, "title": m.title, "state": m.state, "draft": m.draft,
        "source_branch": m.source_branch, "target_branch": m.target_branch, "sha": m.sha,
        "merge_commit_sha": m.merge_commit_sha, "squash_commit_sha": null,
        "description": m.description,
        "source_project_id": m.source_project_id.unwrap_or(p.id), "target_project_id": p.id,
        "author": {"id": m.author_id, "username": m.author_username},
        "web_url": format!("{}/{}/-/merge_requests/{}", ctx.base, p.path_with_namespace, m.iid),
    })
}

fn branch_json(b: &Branch) -> Value {
    json!({"name": b.name, "commit": {"id": b.sha, "committed_date": b.committed_date}})
}

/// A branch read from the project's bare repository, if it has one.
fn git_branch(dir: &std::path::Path, name: &str) -> Option<Branch> {
    let refname = format!("refs/heads/{name}");
    let sha = git_text(dir, &["rev-parse", "--verify", "-q", &refname])?;
    let date = git_text(dir, &["log", "-1", "--format=%cI", &sha])?;
    Some(Branch {
        name: name.into(),
        sha,
        committed_date: date,
    })
}

fn handle(ctx: &Ctx, method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let mut guard = ctx.state.lock().unwrap();
    let state = &mut *guard;
    state.requests.push(Request {
        method: method.to_string(),
        path_and_query: path_and_query.clone(),
        auth: auth.clone(),
    });
    if let Some(i) = state
        .fail
        .iter()
        .position(|f| path_and_query.contains(&f.path_contains))
    {
        let failure = state.fail.remove(i).unwrap();
        return reply(failure.status, &failure.headers, None);
    }

    let Some(rest) = uri
        .path()
        .strip_prefix(&format!("{PREFIX}/api/v4/"))
        .or_else(|| uri.path().strip_prefix(&format!("{PREFIX}/api/v4")))
    else {
        return not_found();
    };
    let Some(token) = auth
        .as_deref()
        .and_then(|a| a.strip_prefix("Bearer "))
        .and_then(|t| state.tokens.get(t))
        .cloned()
    else {
        return json_reply(401, json!({"message": "401 Unauthorized"}));
    };
    let segments: Vec<String> = rest
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    let query = parse_query(uri);
    let json_body = || serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);

    if let (&Method::GET, ["user"]) = (method, &segs[..]) {
        return json_reply(
            200,
            json!({"id": token.user_id, "username": token.username}),
        );
    }
    let ["projects", key, tail @ ..] = &segs[..] else {
        return not_found();
    };
    let id = match find_project(state, &token, key) {
        Ok(p) => p.id,
        Err(response) => return response,
    };
    let project = state.projects.get_mut(&id).unwrap();
    match (method, tail) {
        (&Method::GET, []) => json_reply(200, project_json(ctx, project, &token)),
        (&Method::GET, ["merge_requests"]) => {
            let q = |name: &str| {
                query
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.as_str())
            };
            if std::mem::take(&mut state.malformed_next_mr_list) {
                return json_reply(200, json!({"message": "not a list"}));
            }
            let omit = state.omit_mr_list_project_ids;
            let want = q("state").unwrap_or("opened");
            let mrs: Vec<Value> = project
                .mrs
                .values()
                .rev()
                .filter(|m| m.state == want)
                .filter(|m| q("source_branch").is_none_or(|b| m.source_branch == b))
                .filter(|m| q("target_branch").is_none_or(|b| m.target_branch == b))
                .map(|m| {
                    let mut json = mr_json(ctx, project, m);
                    if omit {
                        json["source_project_id"] = Value::Null;
                        json["target_project_id"] = Value::Null;
                    }
                    json
                })
                .collect();
            let (page, link) = paginate(ctx, uri, &query, mrs);
            reply(200, &link, Some(Value::Array(page)))
        }
        (&Method::POST, ["merge_requests"]) => {
            if let Some(status) = state.refuse_next_mr.take() {
                return json_reply(status, json!({"message": "synthetic refusal"}));
            }
            let b = json_body();
            let text = |k: &str| b[k].as_str().unwrap_or_default().to_owned();
            let (source, target) = (text("source_branch"), text("target_branch"));
            let tip = |name: &str| match &project.git {
                Some(dir) => git_branch(dir, name).map(|b| b.sha),
                None => Some(
                    project
                        .branches
                        .iter()
                        .find(|b| b.name == name)
                        .map_or_else(|| fake_sha(name), |b| b.sha.clone()),
                ),
            };
            // Assumption, not verified against live GitLab: a missing source or target branch may
            // be answered 409 instead, which the Hub treats as uncertain rather than a refusal.
            let (Some(sha), Some(_)) = (tip(&source), tip(&target)) else {
                return json_reply(400, json!({"message": "branch does not exist"}));
            };
            if project.mrs.values().any(|m| {
                m.state == "opened"
                    && m.source_branch == source
                    && m.target_branch == target
                    && m.source_project_id.is_none()
            }) {
                return json_reply(
                    409,
                    json!({"message": ["Another open merge request already exists for this source branch"]}),
                );
            }
            let iid = project.mrs.keys().max().copied().unwrap_or(0) + 1;
            let mut mr = Mr::new(iid, &source, &[]);
            mr.id = 9000 + project.id * 1000 + iid;
            mr.title = text("title");
            mr.description = text("description");
            mr.target_branch = target;
            mr.sha = sha;
            mr.author_username = token.username.clone();
            mr.author_id = token.user_id;
            let created = mr_json(ctx, project, &mr);
            project.mrs.insert(iid, mr);
            if std::mem::take(&mut state.lose_next_mr_reply) {
                return reply(502, &[], None);
            }
            if std::mem::take(&mut state.malformed_next_mr_reply) {
                return json_reply(201, json!({"iid": iid}));
            }
            json_reply(201, created)
        }
        (&Method::GET, ["merge_requests", iid]) => {
            match iid.parse::<i64>().ok().and_then(|n| project.mrs.get(&n)) {
                Some(m) => json_reply(200, mr_json(ctx, project, m)),
                None => not_found(),
            }
        }
        (&Method::GET, ["merge_requests", iid, "diffs"]) => {
            match iid.parse::<i64>().ok().and_then(|n| project.mrs.get(&n)) {
                Some(m) => {
                    let files = m
                        .files
                        .iter()
                        .map(|f| json!({"old_path": f, "new_path": f}))
                        .chain(
                            m.renamed
                                .iter()
                                .map(|(old, new)| json!({"old_path": old, "new_path": new})),
                        )
                        .collect();
                    let (page, link) = paginate(ctx, uri, &query, files);
                    reply(200, &link, Some(Value::Array(page)))
                }
                None => not_found(),
            }
        }
        (&Method::GET, ["repository", "branches"]) => {
            let mut branches = project.branches.clone();
            branches.sort_by(|a, b| b.committed_date.cmp(&a.committed_date));
            let items = branches.iter().map(branch_json).collect();
            let (page, link) = paginate(ctx, uri, &query, items);
            reply(200, &link, Some(Value::Array(page)))
        }
        (&Method::GET, ["repository", "branches", name @ ..]) if !name.is_empty() => {
            let name = name.join("/");
            let found = match &project.git {
                Some(dir) => git_branch(dir, &name),
                None => project.branches.iter().find(|b| b.name == name).cloned(),
            };
            match found.as_ref() {
                Some(b) => json_reply(200, branch_json(b)),
                None => not_found(),
            }
        }
        (&Method::GET, ["merge_requests", iid, "notes"]) => {
            let notes = iid
                .parse::<i64>()
                .ok()
                .and_then(|n| project.notes.get(&n))
                .map(|ns| {
                    ns.iter()
                        .map(|n| json!({"id": n.id, "body": n.body, "author": {"username": n.author_username}}))
                        .collect()
                })
                .unwrap_or_default();
            let (page, link) = paginate(ctx, uri, &query, notes);
            reply(200, &link, Some(Value::Array(page)))
        }
        (&Method::POST, ["merge_requests", iid, "notes"]) => {
            let Ok(iid) = iid.parse::<i64>() else {
                return not_found();
            };
            let note = Note {
                id: 5000
                    + state
                        .projects
                        .values()
                        .map(|p| p.notes.values().map(Vec::len).sum::<usize>())
                        .sum::<usize>() as i64
                    + 1,
                body: json_body()["body"].as_str().unwrap_or_default().to_string(),
                author_username: token.username.clone(),
            };
            let created = json!({"id": note.id, "body": note.body, "author": {"username": note.author_username}});
            state
                .projects
                .get_mut(&id)
                .unwrap()
                .notes
                .entry(iid)
                .or_default()
                .push(note);
            if std::mem::take(&mut state.lose_next_note_reply) {
                return reply(502, &[], None);
            }
            json_reply(201, created)
        }
        (&Method::PUT, ["merge_requests", iid, "notes", nid]) => {
            let found = iid
                .parse::<i64>()
                .ok()
                .zip(nid.parse::<i64>().ok())
                .and_then(|(iid, nid)| {
                    project
                        .notes
                        .get_mut(&iid)?
                        .iter_mut()
                        .find(|n| n.id == nid)
                });
            match found {
                Some(n) if n.author_username != token.username => {
                    json_reply(403, json!({"message": "403 Forbidden"}))
                }
                Some(n) => {
                    n.body = json_body()["body"].as_str().unwrap_or_default().to_string();
                    json_reply(
                        200,
                        json!({"id": n.id, "body": n.body, "author": {"username": n.author_username}}),
                    )
                }
                None => not_found(),
            }
        }
        (&Method::POST, ["statuses", sha]) => {
            let b = json_body();
            let status =
                json!({"state": b["state"], "name": b["name"], "description": b["description"]});
            project.statuses.push((sha.to_string(), status.clone()));
            json_reply(201, status)
        }
        _ => not_found(),
    }
}
