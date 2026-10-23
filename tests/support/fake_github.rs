//! In-process fake of the GitHub REST/GraphQL surface the hub sync uses. Synthetic data only.
//! Include with `#[path = "support/fake_github.rs"] mod fake_github;`.
#![allow(dead_code)] // each including test uses a different subset
#![allow(clippy::collapsible_if, clippy::result_large_err)] // test fake: clarity over lint style

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const APP_ID: i64 = 4242;
pub const APP_SLUG: &str = "agentscm-test";
pub const WEBHOOK_SECRET: &str = "whsec-synthetic";
pub const CLIENT_SECRET: &str = "client-secret-synthetic";

struct Keys {
    pem: String,
    /// DER `RSAPublicKey`, the format `UnparsedPublicKey` expects.
    public: Vec<u8>,
}

/// One synthetic RSA-2048 key per test process, generated with the `openssl` CLI so no key is committed.
fn keys() -> &'static Keys {
    static KEYS: std::sync::OnceLock<Keys> = std::sync::OnceLock::new();
    KEYS.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.der");
        let status = std::process::Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-outform",
                "DER",
                "-out",
            ])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("the fake GitHub needs the `openssl` CLI on PATH to generate its test key");
        assert!(status.success(), "`openssl genpkey` failed: {status}");
        let pkcs8 = std::fs::read(&path).unwrap();
        let pkcs1 = pkcs1_key(&pkcs8);
        let public = ring::signature::RsaKeyPair::from_der(pkcs1)
            .unwrap()
            .public()
            .as_ref()
            .to_vec();
        let body = base64::engine::general_purpose::STANDARD.encode(pkcs1);
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        let pem = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----\n",
            lines.join("\n")
        );
        Keys { pem, public }
    })
}

/// Reads one DER TLV at the start of `d`: (tag, contents, rest).
fn der_tlv(d: &[u8]) -> (u8, &[u8], &[u8]) {
    let (tag, first) = (d[0], d[1] as usize);
    let (len, start) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        (
            d[2..2 + n].iter().fold(0, |acc, b| acc << 8 | *b as usize),
            2 + n,
        )
    };
    (tag, &d[start..start + len], &d[start + len..])
}

/// The PKCS#1 `RSAPrivateKey` from `genpkey` output. Some OpenSSL builds write PKCS#8
/// (`SEQUENCE { INTEGER, SEQUENCE algId, OCTET STRING key }`), others write PKCS#1 directly.
fn pkcs1_key(der: &[u8]) -> &[u8] {
    let (tag, seq, _) = der_tlv(der);
    assert_eq!(tag, 0x30);
    let (_, _, rest) = der_tlv(seq); // version
    if rest[0] != 0x30 {
        return der; // PKCS#1: the version is followed by the modulus INTEGER
    }
    let (_, _, rest) = der_tlv(rest); // algorithm identifier
    let (tag, key, _) = der_tlv(rest);
    assert_eq!(tag, 0x04);
    key
}

/// PKCS#1 `BEGIN RSA PRIVATE KEY` PEM, the format GitHub's manifest conversion returns.
pub fn private_pem() -> &'static str {
    &keys().pem
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `sha256=<hex hmac>` over `body`, the `X-Hub-Signature-256` format.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    format!("sha256={}", hex(ring::hmac::sign(&key, body).as_ref()))
}

/// POSTs a signed webhook delivery to the hub and returns the HTTP status.
pub async fn deliver(
    hub: &str,
    connection: &str,
    event: &str,
    delivery: &str,
    body: &Value,
    secret: &str,
) -> u16 {
    let bytes = serde_json::to_vec(body).unwrap();
    match reqwest::Client::new()
        .post(format!("{hub}/v1/hub/webhooks/github/{connection}"))
        .header("content-type", "application/json")
        .header("x-github-event", event)
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", sign(secret, &bytes))
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
pub struct Installation {
    pub id: i64,
    pub account: String,
    pub repos: Vec<i64>,
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
    pub if_none_match: Option<String>,
    /// The raw `Authorization` header value.
    pub auth: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Comment {
    pub id: u64,
    pub body: String,
    pub author: String,
}

#[derive(Clone, Debug)]
pub struct Branch {
    pub name: String,
    pub sha: String,
    pub committed_at: String,
}

#[derive(Clone, Debug)]
pub struct Pull {
    pub number: u64,
    pub node_id: String,
    pub title: String,
    pub state: String,
    pub merged: bool,
    pub merge_commit_sha: Option<String>,
    pub draft: bool,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub author_login: String,
    pub author_id: i64,
    pub updated_at: String,
    pub files: Vec<String>,
    pub body: String,
    /// `None`: the head is in this repository. `Some(None)`: the head repository is gone (null).
    /// `Some(Some(id))`: a fork with that repository id.
    pub head_repo: Option<Option<i64>>,
    /// Overrides `base.repo.id`, which is otherwise the repository's own.
    pub base_repo_id: Option<i64>,
}

impl Pull {
    /// Open, non-draft pull against `main` by `octocat`; ids and sha derive from the inputs.
    pub fn new(number: u64, head_ref: &str, files: &[&str]) -> Self {
        Pull {
            number,
            node_id: format!("PR_{number}"),
            title: format!("Pull request {number}"),
            state: "open".into(),
            merged: false,
            merge_commit_sha: None,
            draft: false,
            head_ref: head_ref.into(),
            head_sha: fake_sha(&format!("{head_ref}#{number}")),
            base_ref: "main".into(),
            author_login: "octocat".into(),
            author_id: 1,
            updated_at: "2026-01-01T00:00:00Z".into(),
            files: files.iter().map(|f| f.to_string()).collect(),
            body: String::new(),
            head_repo: None,
            base_repo_id: None,
        }
    }
}

impl Branch {
    pub fn new(name: &str, committed_at: &str) -> Self {
        Branch {
            name: name.into(),
            sha: fake_sha(name),
            committed_at: committed_at.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Repo {
    pub default_branch: Option<String>,
    pub id: i64,
    pub full_name: String,
    pub node_id: String,
    pub private: bool,
    pub pulls: BTreeMap<u64, Pull>,
    pub branches: Vec<Branch>,
    /// Issue/PR number to comments.
    pub comments: BTreeMap<u64, Vec<Comment>>,
    /// (sha, {state, context, description}) in creation order.
    pub statuses: Vec<(String, Value)>,
    /// A bare Git repository behind the git-data API (blobs, trees, commits, refs), if any.
    pub git: Option<std::path::PathBuf>,
    /// Without `git`: commit sha to (tree sha, parent shas) for `GET git/commits/{sha}`.
    pub commits: BTreeMap<String, (String, Vec<String>)>,
    /// Raw `actions/runs` entries; `head_sha` selects them.
    pub runs: Vec<Value>,
    /// Raw deployments with their statuses, newest first; `sha` selects them.
    pub deployments: Vec<(Value, Vec<Value>)>,
    /// Rulesets applying to every branch, as `GET rules/branches/{branch}` lists them. None: `[]`.
    pub rules: Option<Value>,
}

impl Repo {
    /// A private repository; set `private = false` for a publicly readable one.
    pub fn new(id: i64, full_name: &str) -> Self {
        Repo {
            default_branch: Some("main".into()),
            id,
            full_name: full_name.into(),
            node_id: format!("R_{id}"),
            private: true,
            pulls: BTreeMap::new(),
            branches: Vec::new(),
            comments: BTreeMap::new(),
            statuses: Vec::new(),
            git: None,
            commits: BTreeMap::new(),
            runs: Vec::new(),
            deployments: Vec::new(),
            rules: None,
        }
    }
}

/// A workflow run as GitHub lists it; seed it with `Repo::runs`.
pub fn run_json(
    repo_id: i64,
    id: i64,
    sha: &str,
    branch: &str,
    status: &str,
    conclusion: Option<&str>,
) -> Value {
    json!({"id": id, "run_attempt": 1, "head_sha": sha, "head_branch": branch, "event": "push",
        "status": status, "conclusion": conclusion, "path": ".github/workflows/ci.yml",
        "html_url": format!("https://github.com/acme/widgets/actions/runs/{id}"),
        "updated_at": rfc3339(1_700_000_000), "repository": {"id": repo_id}})
}

/// A deployment and one status as GitHub lists them; seed with `Repo::deployments`.
pub fn deployment_json(id: i64, sha: &str, environment: &str) -> Value {
    json!({"id": id, "sha": sha, "ref": "main", "environment": environment,
        "description": "deploy", "updated_at": rfc3339(1_700_000_000)})
}

pub fn status_json(id: i64, state: &str) -> Value {
    json!({"id": id, "state": state, "updated_at": rfc3339(1_700_000_100),
        "log_url": null, "target_url": null})
}

#[derive(Default)]
pub struct FakeState {
    /// Valid manifest conversion codes; a conversion consumes one.
    pub manifest_codes: HashSet<String>,
    pub conversions: u32,
    pub installations: Vec<Installation>,
    pub repos: BTreeMap<i64, Repo>,
    /// PAT to (login, id, readable repo ids).
    pub pats: HashMap<String, (String, i64, BTreeSet<i64>)>,
    /// OAuth user token to (login, id, readable repo ids).
    pub users: HashMap<String, (String, i64, BTreeSet<i64>)>,
    /// The first entry whose `path_contains` matches a request is consumed and answered.
    pub fail: VecDeque<Failure>,
    /// Answer the actions and deployments lists with 200 and an empty object, no array.
    pub arrayless_external_lists: bool,
    /// Store the next created comment, then answer 502 with no body.
    pub lose_next_comment_reply: bool,
    pub requests: Vec<Request>,
    /// Token (PAT or user) to the repo ids it can push to; read-only everywhere else.
    pub pushers: HashMap<String, BTreeSet<i64>>,
    /// Issued installation token to installation id.
    pub installation_tokens: HashMap<String, i64>,
    /// Branch names whose creation is refused, with the status to answer (403 or 422).
    pub protected_branches: HashMap<String, u16>,
    /// Store the next created pull request, then answer 502 with no body.
    pub lose_next_pull_reply: bool,
    pub malformed_next_pull_reply: bool,
    pub refuse_next_pull: bool,
    /// Create the next branch, then answer 502 with no body.
    pub lose_next_ref_reply: bool,
    /// Record the next commit status, then answer 502 with no body.
    pub lose_next_status_reply: bool,
    /// After recording the next commit status, move the first branch's tip to this sha.
    pub advance_base_after_status: Option<String>,
    /// Leave `head.repo` and `base.repo` out of pull request JSON.
    pub omit_pull_repos: bool,
    /// Build every created commit with a different message, so its SHA differs from the client's.
    pub alter_commit_messages: bool,
    /// The login recorded as `creator` of statuses posted through the API. Default: the
    /// caller's own login (`<slug>[bot]` for an installation token).
    pub status_creator: Option<String>,
    /// `permissions` of `GET /app/installations/{id}`. Default: the Hub's own request set,
    /// including `merge_queues: read`.
    pub installation_permissions: Option<Value>,
    /// `events` of `GET /app/installations/{id}`. Default: `merge_group` among the others.
    pub installation_events: Option<Vec<String>>,
}

impl FakeState {
    /// Records a status as another account posted it (newest last, as the API would).
    pub fn push_status(&mut self, repo: i64, sha: &str, state: &str, context: &str, login: &str) {
        let status = json!({"state": state, "context": context, "description": "other",
            "creator": {"login": login}});
        self.repos
            .get_mut(&repo)
            .unwrap()
            .statuses
            .push((sha.to_string(), status));
    }
}

fn fake_sha(seed: &str) -> String {
    hex(&Sha256::digest(seed.as_bytes()))[..40].to_string()
}

// ---- server ----

pub struct FakeGithub {
    /// Used as both the web and the API base.
    pub base: String,
    pub state: Arc<Mutex<FakeState>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Clone)]
struct Ctx {
    base: String,
    state: Arc<Mutex<FakeState>>,
}

impl FakeGithub {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(FakeState::default()));
        let ctx = Ctx {
            base: base.clone(),
            state: state.clone(),
        };
        let app = Router::new().fallback(entry).with_state(ctx);
        let task = Some(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap()
        }));
        FakeGithub { base, state, task }
    }

    pub async fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap();
            assert!(result.is_ok() || result.unwrap_err().is_cancelled());
        }
    }

    pub async fn restart(&mut self) {
        assert!(self.task.is_none());
        let listener = tokio::net::TcpListener::bind(self.base.trim_start_matches("http://"))
            .await
            .unwrap();
        let ctx = Ctx {
            base: self.base.clone(),
            state: self.state.clone(),
        };
        let app = Router::new().fallback(entry).with_state(ctx);
        self.task = Some(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap()
        }));
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
    json_reply(404, json!({"message": "Not Found"}))
}

fn unauthorized() -> Response {
    json_reply(401, json!({"message": "Bad credentials"}))
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

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's civil-from-days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// RS256 app JWT: signature against the fixture key, `iss` is the app id, not expired.
fn jwt_valid(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, signature] = parts[..] else {
        return false;
    };
    let decode = |s: &str| URL_SAFE_NO_PAD.decode(s).ok();
    let (Some(header), Some(payload), Some(signature)) =
        (decode(header), decode(payload), decode(signature))
    else {
        return false;
    };
    let (Ok(header), Ok(claims)) = (
        serde_json::from_slice::<Value>(&header),
        serde_json::from_slice::<Value>(&payload),
    ) else {
        return false;
    };
    let signing_input = &token[..token.len() - parts[2].len() - 1];
    let iss_ok = match &claims["iss"] {
        Value::String(s) => s.parse::<i64>().ok() == Some(APP_ID),
        Value::Number(n) => n.as_i64() == Some(APP_ID),
        _ => false,
    };
    let (Some(exp), Some(iat)) = (claims["exp"].as_i64(), claims["iat"].as_i64()) else {
        return false;
    };
    header["alg"] == "RS256"
        && iss_ok
        && exp > now()
        && iat <= now() + 60
        && UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &keys().public)
            .verify(signing_input.as_bytes(), &signature)
            .is_ok()
}

enum Who {
    Installation(i64),
    /// PAT or user token: (login, id, readable repo ids, repo ids it can push to).
    Reader(String, i64, BTreeSet<i64>, BTreeSet<i64>),
}

fn who(state: &FakeState, auth: Option<&str>) -> Option<Who> {
    let auth = auth?;
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("token "))?;
    if let Some(id) = state.installation_tokens.get(token) {
        return Some(Who::Installation(*id));
    }
    let (login, id, repos) = state.pats.get(token).or_else(|| state.users.get(token))?;
    let push = state.pushers.get(token).cloned().unwrap_or_default();
    Some(Who::Reader(login.clone(), *id, repos.clone(), push))
}

fn can_read(state: &FakeState, who: &Who, repo: &Repo) -> bool {
    !repo.private
        || match who {
            Who::Installation(id) => state
                .installations
                .iter()
                .any(|i| i.id == *id && i.repos.contains(&repo.id)),
            Who::Reader(_, _, repos, _) => repos.contains(&repo.id),
        }
}

/// Resolves `owner/name` case-insensitively; 404 when missing or unreadable.
fn find_repo(state: &FakeState, who: &Who, owner: &str, name: &str) -> Result<i64, Response> {
    let wanted = format!("{owner}/{name}").to_lowercase();
    state
        .repos
        .values()
        .find(|r| r.full_name.to_lowercase() == wanted && can_read(state, who, r))
        .map(|r| r.id)
        .ok_or_else(not_found)
}

/// One page of `items` plus an RFC 5988 `Link` header (next and last) when more pages exist.
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
    let per_page = get("per_page").unwrap_or(30).clamp(1, 100);
    let page = get("page").unwrap_or(1).max(1);
    let last = items.len().div_ceil(per_page).max(1);
    let rest: String = query
        .iter()
        .filter(|(k, _)| k != "page" && k != "per_page")
        .map(|(k, v)| format!("{k}={v}&"))
        .collect();
    let link = |p: usize, rel: &str| {
        format!(
            "<{}{}?{rest}per_page={per_page}&page={p}>; rel=\"{rel}\"",
            ctx.base,
            uri.path()
        )
    };
    let headers = if page < last {
        vec![(
            "link".to_string(),
            format!("{}, {}", link(page + 1, "next"), link(last, "last")),
        )]
    } else {
        vec![]
    };
    (
        items
            .into_iter()
            .skip((page - 1) * per_page)
            .take(per_page)
            .collect(),
        headers,
    )
}

fn pull_json(ctx: &Ctx, repo: &Repo, p: &Pull) -> Value {
    let repo_ref = |id: i64| json!({"id": id});
    let head_repo = match p.head_repo {
        None => repo_ref(repo.id),
        Some(None) => Value::Null,
        Some(Some(id)) => repo_ref(id),
    };
    json!({
        "number": p.number, "node_id": p.node_id, "title": p.title, "body": p.body, "state": p.state,
        "merged": p.merged, "merge_commit_sha": p.merge_commit_sha, "draft": p.draft,
        "head": {"ref": p.head_ref, "sha": p.head_sha, "repo": head_repo},
        "base": {"ref": p.base_ref, "repo": repo_ref(p.base_repo_id.unwrap_or(repo.id))},
        "user": {"login": p.author_login, "id": p.author_id},
        "updated_at": p.updated_at,
        "html_url": format!("{}/{}/pull/{}", ctx.base, repo.full_name, p.number),
    })
}

// ---- a real bare Git repository behind the git-data API ----

/// Clones `source` (a local repository) into `<dir>/bare.git` for a fake repo's `git` field.
pub fn bare_clone(dir: &std::path::Path, source: &std::path::Path) -> std::path::PathBuf {
    let bare = dir.join("bare.git");
    let output = std::process::Command::new("/usr/bin/git")
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["clone", "--bare", "--quiet"])
        .arg(source)
        .arg(&bare)
        .output()
        .unwrap();
    assert!(output.status.success(), "git clone: {:?}", output.stderr);
    bare
}

/// Runs Git in the bare repository with no user configuration; `Err` carries stderr.
pub fn git(
    dir: &std::path::Path,
    args: &[&str],
    env: &[(&str, &str)],
    input: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut command = std::process::Command::new("/usr/bin/git");
    command
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_DIR", dir)
        .envs(env.iter().copied())
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let bytes = input.unwrap_or_default().to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&bytes);
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

fn git_text(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = git(dir, args, &[], None).ok()?;
    Some(String::from_utf8(out).ok()?.trim().to_owned())
}

/// The git-data and pull request writes of the publisher. `None` when the path is not one of them.
fn git_data(
    ctx: &Ctx,
    state: &mut FakeState,
    id: i64,
    method: &Method,
    rest: &[&str],
    body: &Value,
) -> Option<Response> {
    let dir = state.repos[&id].git.clone()?;
    let message = |status, text: &str| Some(json_reply(status, json!({"message": text})));
    match (method, rest) {
        (&Method::GET, ["git", "commits", sha]) => Some(
            match git_text(
                &dir,
                &["rev-parse", "--verify", "-q", &format!("{sha}^{{tree}}")],
            ) {
                Some(tree) => {
                    let line = git_text(&dir, &["rev-list", "--parents", "-n1", sha]);
                    let parents: Vec<&str> = line
                        .as_deref()
                        .map(|l| l.split(' ').skip(1).collect())
                        .unwrap_or_default();
                    json_reply(
                        200,
                        json!({"sha": sha, "tree": {"sha": tree},
                            "parents": parents.iter().map(|p| json!({"sha": p})).collect::<Vec<_>>()}),
                    )
                }
                None => not_found(),
            },
        ),
        (&Method::GET, ["git", "ref", "heads", name @ ..]) => {
            let branch = name.join("/");
            Some(
                match git_text(
                    &dir,
                    &[
                        "rev-parse",
                        "--verify",
                        "-q",
                        &format!("refs/heads/{branch}"),
                    ],
                ) {
                    Some(sha) => json_reply(
                        200,
                        json!({"ref": format!("refs/heads/{branch}"), "object": {"type": "commit", "sha": sha}}),
                    ),
                    None => json_reply(404, json!({"message": "Not Found"})),
                },
            )
        }
        (&Method::POST, ["git", "blobs"]) => {
            let content = body["content"].as_str().unwrap_or_default();
            let bytes = if body["encoding"] == "base64" {
                base64::engine::general_purpose::STANDARD
                    .decode(content)
                    .unwrap_or_default()
            } else {
                content.as_bytes().to_vec()
            };
            let sha = git_text_in(
                &dir,
                &["hash-object", "-w", "--no-filters", "--stdin"],
                &bytes,
            )?;
            Some(json_reply(201, json!({"sha": sha})))
        }
        (&Method::POST, ["git", "trees"]) => {
            let index = dir.join(format!("fake-index-{}", state.requests.len()));
            let env = [
                ("GIT_INDEX_FILE", index.to_str().unwrap()),
                ("GIT_WORK_TREE", dir.to_str().unwrap()),
            ];
            let run = |args: &[&str]| git(&dir, args, &env, None);
            if let Some(base) = body["base_tree"].as_str() {
                run(&["read-tree", base]).ok()?;
            }
            for entry in body["tree"].as_array().into_iter().flatten() {
                let path = entry["path"].as_str().unwrap_or_default();
                let updated = match entry["sha"].as_str() {
                    Some(sha) => {
                        let mode = entry["mode"].as_str().unwrap_or("100644");
                        run(&[
                            "update-index",
                            "--add",
                            "--cacheinfo",
                            &format!("{mode},{sha},{path}"),
                        ])
                    }
                    None => run(&["update-index", "--force-remove", "--", path]),
                };
                if updated.is_err() {
                    return message(422, "Invalid tree");
                }
            }
            let tree = run(&["write-tree"])
                .ok()
                .and_then(|o| String::from_utf8(o).ok());
            let _ = std::fs::remove_file(&index);
            Some(json_reply(201, json!({"sha": tree?.trim()})))
        }
        (&Method::POST, ["git", "commits"]) => {
            let person =
                |key: &str, field: &str| body[key][field].as_str().unwrap_or_default().to_owned();
            let env: Vec<(&str, String)> = vec![
                ("GIT_AUTHOR_NAME", person("author", "name")),
                ("GIT_AUTHOR_EMAIL", person("author", "email")),
                ("GIT_AUTHOR_DATE", person("author", "date")),
                ("GIT_COMMITTER_NAME", person("committer", "name")),
                ("GIT_COMMITTER_EMAIL", person("committer", "email")),
                ("GIT_COMMITTER_DATE", person("committer", "date")),
            ];
            let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let tree = body["tree"].as_str().unwrap_or_default();
            let mut args = vec!["commit-tree".to_owned(), tree.to_owned()];
            for parent in body["parents"].as_array().into_iter().flatten() {
                args.extend([
                    "-p".to_owned(),
                    parent.as_str().unwrap_or_default().to_owned(),
                ]);
            }
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let mut message_text = body["message"].as_str().unwrap_or_default().to_owned();
            if state.alter_commit_messages {
                message_text.push_str("(altered by the forge)");
            }
            match git(&dir, &args, &env, Some(message_text.as_bytes())) {
                Ok(out) => Some(json_reply(
                    201,
                    json!({"sha": String::from_utf8_lossy(&out).trim()}),
                )),
                Err(_) => message(422, "Invalid commit"),
            }
        }
        (&Method::POST, ["git", "refs"]) => {
            let name = body["ref"].as_str().unwrap_or_default();
            let sha = body["sha"].as_str().unwrap_or_default();
            let branch = name.strip_prefix("refs/heads/").unwrap_or(name);
            if let Some(status) = state.protected_branches.get(branch) {
                return message(
                    *status,
                    "Cannot create ref due to creations being restricted.",
                );
            }
            if git(
                &dir,
                &["cat-file", "-e", &format!("{sha}^{{commit}}")],
                &[],
                None,
            )
            .is_err()
            {
                return message(422, "Object does not exist");
            }
            Some(
                match git(&dir, &["update-ref", name, sha, &"0".repeat(40)], &[], None) {
                    Ok(_) if std::mem::take(&mut state.lose_next_ref_reply) => {
                        reply(502, &[], None)
                    }
                    Ok(_) => json_reply(201, json!({"ref": name, "object": {"sha": sha}})),
                    Err(_) => json_reply(422, json!({"message": "Reference already exists"})),
                },
            )
        }
        (&Method::POST, ["pulls"]) => {
            if std::mem::take(&mut state.refuse_next_pull) {
                return Some(json_reply(422, json!({"message":"synthetic refusal"})));
            }
            let head = body["head"].as_str().unwrap_or_default();
            let Some(head_sha) = git_text(
                &dir,
                &["rev-parse", "--verify", "-q", &format!("refs/heads/{head}")],
            ) else {
                return message(422, "Validation Failed: head invalid");
            };
            let repo = state.repos.get_mut(&id).unwrap();
            let number = repo.pulls.keys().max().copied().unwrap_or(0) + 1;
            let mut pull = Pull::new(number, head, &[]);
            pull.title = body["title"].as_str().unwrap_or_default().to_owned();
            pull.body = body["body"].as_str().unwrap_or_default().to_owned();
            pull.base_ref = body["base"].as_str().unwrap_or("main").to_owned();
            pull.head_sha = head_sha;
            pull.author_login = format!("{APP_SLUG}[bot]");
            let created = pull_json(ctx, repo, &pull);
            repo.pulls.insert(number, pull);
            if std::mem::take(&mut state.lose_next_pull_reply) {
                return Some(reply(502, &[], None));
            }
            if std::mem::take(&mut state.malformed_next_pull_reply) {
                return Some(json_reply(201, json!({"number": number})));
            }
            Some(json_reply(201, created))
        }
        _ => None,
    }
}

fn git_text_in(dir: &std::path::Path, args: &[&str], input: &[u8]) -> Option<String> {
    let out = git(dir, args, &[], Some(input)).ok()?;
    Some(String::from_utf8(out).ok()?.trim().to_owned())
}

fn handle(ctx: &Ctx, method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Response {
    let text = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    let auth = text(header::AUTHORIZATION);
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
        if_none_match: text(header::IF_NONE_MATCH),
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

    let segments: Vec<String> = uri
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    let query = parse_query(uri);
    let json_body = || serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);

    // Manifest conversion needs no credentials.
    if let (&Method::POST, ["app-manifests", code, "conversions"]) = (method, &segs[..]) {
        if !state.manifest_codes.remove(*code) {
            return not_found();
        }
        state.conversions += 1;
        return json_reply(
            201,
            json!({
                "id": APP_ID, "slug": APP_SLUG, "node_id": "A_1", "owner": {"login": "acme"},
                "name": "agentscm-test", "client_id": "Iv1.synthetic",
                "client_secret": CLIENT_SECRET, "webhook_secret": WEBHOOK_SECRET,
                "pem": private_pem(), "html_url": format!("{}/apps/{APP_SLUG}", ctx.base),
            }),
        );
    }

    // App JWT routes.
    if let ["app", rest @ ..] = &segs[..] {
        let bearer = auth.as_deref().and_then(|a| a.strip_prefix("Bearer "));
        if !bearer.is_some_and(jwt_valid) {
            return unauthorized();
        }
        return match (method, rest) {
            (&Method::GET, ["installations"]) => json_reply(
                200,
                state
                    .installations
                    .iter()
                    .map(|i| json!({"id": i.id, "account": {"login": i.account}}))
                    .collect::<Vec<_>>()
                    .into(),
            ),
            (&Method::GET, ["installations", id]) => {
                let Some(installation) = id
                    .parse::<i64>()
                    .ok()
                    .and_then(|id| state.installations.iter().find(|i| i.id == id))
                else {
                    return not_found();
                };
                let permissions = state.installation_permissions.clone().unwrap_or_else(|| {
                    json!({"merge_queues": "read", "statuses": "write", "pull_requests": "read",
                        "contents": "read", "metadata": "read"})
                });
                let events = state.installation_events.clone().unwrap_or_else(|| {
                    ["merge_group", "pull_request", "push"]
                        .map(String::from)
                        .into()
                });
                json_reply(
                    200,
                    json!({"id": installation.id, "account": {"login": installation.account},
                        "permissions": permissions, "events": events}),
                )
            }
            (&Method::POST, ["installations", id, "access_tokens"]) => {
                let Some(id) = id
                    .parse::<i64>()
                    .ok()
                    .filter(|id| state.installations.iter().any(|i| i.id == *id))
                else {
                    return not_found();
                };
                let token = format!(
                    "ghs_{}",
                    &hex(&Sha256::digest(format!(
                        "tok{}-{id}-{}",
                        state.installation_tokens.len(),
                        now()
                    )))[..32]
                );
                state.installation_tokens.insert(token.clone(), id);
                json_reply(
                    201,
                    json!({"token": token, "expires_at": rfc3339(now() + 3600)}),
                )
            }
            _ => not_found(),
        };
    }

    // Token routes.
    let Some(who) = who(state, auth.as_deref()) else {
        return unauthorized();
    };
    match (method, &segs[..]) {
        (&Method::GET, ["installation", "repositories"]) => {
            let Who::Installation(id) = who else {
                return unauthorized();
            };
            let repos: Vec<Value> = state
                .installations
                .iter()
                .filter(|i| i.id == id)
                .flat_map(|i| i.repos.iter())
                .filter_map(|r| state.repos.get(r))
                .map(|r| json!({"id": r.id, "full_name": r.full_name, "node_id": r.node_id, "private": r.private, "default_branch": r.default_branch}))
                .collect();
            let total = repos.len();
            let (page, link) = paginate(ctx, uri, &query, repos);
            reply(
                200,
                &link,
                Some(json!({"total_count": total, "repositories": page})),
            )
        }
        (&Method::GET, ["user"]) => match who {
            Who::Reader(login, id, ..) => json_reply(200, json!({"id": id, "login": login})),
            Who::Installation(_) => unauthorized(),
        },
        (&Method::POST, ["graphql"]) => {
            let vars = &json_body()["variables"];
            let (Some(owner), Some(name)) = (vars["owner"].as_str(), vars["name"].as_str()) else {
                return json_reply(
                    200,
                    json!({"errors": [{"message": "owner and name required"}]}),
                );
            };
            let Ok(id) = find_repo(state, &who, owner, name) else {
                return json_reply(200, json!({"data": {"repository": null}}));
            };
            let mut branches = state.repos[&id].branches.clone();
            branches.sort_by(|a, b| {
                b.committed_at
                    .cmp(&a.committed_at)
                    .then(a.name.cmp(&b.name))
            });
            let start = vars["after"]
                .as_str()
                .and_then(|c| c.parse::<usize>().ok())
                .unwrap_or(0);
            let end = (start + 100).min(branches.len());
            let nodes: Vec<Value> = branches[start.min(end)..end]
                .iter()
                .map(|b| json!({"name": b.name, "target": {"oid": b.sha, "committedDate": b.committed_at}}))
                .collect();
            json_reply(
                200,
                json!({"data": {"repository": {"refs": {
                    "pageInfo": {"hasNextPage": end < branches.len(), "endCursor": end.to_string()},
                    "nodes": nodes,
                }}}}),
            )
        }
        (_, ["repos", owner, name, rest @ ..]) => {
            let id = match find_repo(state, &who, owner, name) {
                Ok(id) => id,
                Err(response) => return response,
            };
            if let Some(response) = git_data(ctx, state, id, method, rest, &json_body()) {
                return response;
            }
            match (method, rest) {
                (&Method::GET, []) => {
                    let r = &state.repos[&id];
                    let push = match &who {
                        Who::Installation(_) => true,
                        Who::Reader(_, _, _, pushable) => pushable.contains(&r.id),
                    };
                    json_reply(
                        200,
                        json!({"id": r.id, "full_name": r.full_name, "node_id": r.node_id, "private": r.private, "default_branch": r.default_branch, "permissions": {"pull": true, "push": push}}),
                    )
                }
                (&Method::GET, ["pulls"]) => {
                    let repo = &state.repos[&id];
                    // `state` is `open` (the default), `closed` or `all`; `head=owner:branch`
                    // keeps the pulls from that branch.
                    let wanted = |key: &str| {
                        query
                            .iter()
                            .find(|(k, _)| k == key)
                            .map(|(_, v)| v.as_str())
                    };
                    let (state_filter, head) = (wanted("state").unwrap_or("open"), wanted("head"));
                    let head = head.map(|v| v.rsplit_once(':').map_or(v, |x| x.1).to_owned());
                    let open: Vec<Value> = repo
                        .pulls
                        .values()
                        .rev()
                        .filter(|p| state_filter == "all" || p.state == state_filter)
                        .filter(|p| head.as_ref().is_none_or(|head| &p.head_ref == head))
                        .map(|p| pull_json(ctx, repo, p))
                        .collect();
                    let etag = format!(
                        "\"{}\"",
                        hex(&Sha256::digest(Value::Array(open.clone()).to_string()))
                    );
                    if text(header::IF_NONE_MATCH).as_deref() == Some(etag.as_str()) {
                        return reply(304, &[("etag".into(), etag)], None);
                    }
                    let (page, mut link) = paginate(ctx, uri, &query, open);
                    link.push(("etag".into(), etag));
                    reply(200, &link, Some(Value::Array(page)))
                }
                (&Method::GET, ["actions", "runs"] | ["deployments"])
                    if state.arrayless_external_lists =>
                {
                    json_reply(200, json!({}))
                }
                (&Method::GET, ["actions", "runs"]) => {
                    let wanted = query.iter().find(|(k, _)| k == "head_sha").map(|(_, v)| v);
                    let runs: Vec<Value> = state.repos[&id]
                        .runs
                        .iter()
                        .filter(|r| wanted.is_none_or(|w| r["head_sha"].as_str() == Some(w)))
                        .cloned()
                        .collect();
                    let total = runs.len();
                    let (page, link) = paginate(ctx, uri, &query, runs);
                    reply(
                        200,
                        &link,
                        Some(json!({"total_count": total, "workflow_runs": page})),
                    )
                }
                (&Method::GET, ["deployments"]) => {
                    let wanted = query.iter().find(|(k, _)| k == "sha").map(|(_, v)| v);
                    let found: Vec<Value> = state.repos[&id]
                        .deployments
                        .iter()
                        .filter(|(d, _)| wanted.is_none_or(|w| d["sha"].as_str() == Some(w)))
                        .map(|(d, _)| d.clone())
                        .collect();
                    let (page, link) = paginate(ctx, uri, &query, found);
                    reply(200, &link, Some(Value::Array(page)))
                }
                (&Method::GET, ["deployments", deployment, "statuses"]) => {
                    let found = deployment.parse::<i64>().ok().and_then(|n| {
                        state.repos[&id]
                            .deployments
                            .iter()
                            .find(|(d, _)| d["id"].as_i64() == Some(n))
                    });
                    match found {
                        Some((_, statuses)) => {
                            let (page, link) = paginate(ctx, uri, &query, statuses.clone());
                            reply(200, &link, Some(Value::Array(page)))
                        }
                        None => not_found(),
                    }
                }
                (&Method::GET, ["pulls", num]) => {
                    let repo = &state.repos[&id];
                    match num.parse::<u64>().ok().and_then(|n| repo.pulls.get(&n)) {
                        Some(p) => {
                            let mut pull = pull_json(ctx, repo, p);
                            if state.omit_pull_repos {
                                pull["head"].as_object_mut().unwrap().remove("repo");
                                pull["base"].as_object_mut().unwrap().remove("repo");
                            }
                            json_reply(200, pull)
                        }
                        None => not_found(),
                    }
                }
                (&Method::GET, ["pulls", num, "files"]) => {
                    match num
                        .parse::<u64>()
                        .ok()
                        .and_then(|n| state.repos[&id].pulls.get(&n))
                    {
                        Some(p) => {
                            let files = p
                                .files
                                .iter()
                                .map(|f| json!({"filename": f, "status": "modified"}))
                                .collect();
                            let (page, link) = paginate(ctx, uri, &query, files);
                            reply(200, &link, Some(Value::Array(page)))
                        }
                        None => not_found(),
                    }
                }
                (&Method::GET, ["branches", name @ ..]) if !name.is_empty() => {
                    let wanted = name.join("/");
                    match state.repos[&id].branches.iter().find(|b| b.name == wanted) {
                        Some(b) => json_reply(
                            200,
                            json!({"name": b.name, "commit": {"sha": b.sha, "commit": {"committer": {"date": b.committed_at}}}}),
                        ),
                        None => not_found(),
                    }
                }
                (&Method::GET, ["issues", num, "comments"]) => {
                    let comments = num
                        .parse::<u64>()
                        .ok()
                        .and_then(|n| state.repos[&id].comments.get(&n))
                        .map(|cs| cs.iter().map(|c| json!({"id": c.id, "body": c.body, "user": {"login": c.author}})).collect())
                        .unwrap_or_default();
                    let (page, link) = paginate(ctx, uri, &query, comments);
                    reply(200, &link, Some(Value::Array(page)))
                }
                (&Method::POST, ["issues", num, "comments"]) => {
                    let Ok(num) = num.parse::<u64>() else {
                        return not_found();
                    };
                    let next = state
                        .repos
                        .values()
                        .flat_map(|r| r.comments.values().flatten())
                        .map(|c| c.id)
                        .max()
                        .unwrap_or(0)
                        + 1;
                    let comment = Comment {
                        id: next,
                        body: json_body()["body"].as_str().unwrap_or_default().to_string(),
                        author: match &who {
                            Who::Reader(login, ..) => login.clone(),
                            Who::Installation(_) => format!("{APP_SLUG}[bot]"),
                        },
                    };
                    let created = json!({"id": comment.id, "body": comment.body});
                    state
                        .repos
                        .get_mut(&id)
                        .unwrap()
                        .comments
                        .entry(num)
                        .or_default()
                        .push(comment);
                    if std::mem::take(&mut state.lose_next_comment_reply) {
                        return reply(502, &[], None);
                    }
                    json_reply(201, created)
                }
                (&Method::PATCH, ["issues", "comments", cid]) => {
                    let found = cid.parse::<u64>().ok().and_then(|cid| {
                        state
                            .repos
                            .get_mut(&id)
                            .unwrap()
                            .comments
                            .values_mut()
                            .flatten()
                            .find(|c| c.id == cid)
                    });
                    match found {
                        Some(c) => {
                            c.body = json_body()["body"].as_str().unwrap_or_default().to_string();
                            json_reply(200, json!({"id": c.id, "body": c.body}))
                        }
                        None => not_found(),
                    }
                }
                // Every branch whose name starts with the prefix, as GitHub's matching-refs does
                // (a trailing slash is part of the prefix), paginated.
                (&Method::GET, ["git", "matching-refs", "heads", prefix @ ..]) => {
                    let slash = if uri.path().ends_with('/') { "/" } else { "" };
                    let prefix = format!("{}{slash}", prefix.join("/"));
                    let refs: Vec<Value> = state.repos[&id]
                        .branches
                        .iter()
                        .filter(|b| b.name.starts_with(&prefix))
                        .map(|b| {
                            json!({"ref": format!("refs/heads/{}", b.name),
                                "object": {"type": "commit", "sha": b.sha}})
                        })
                        .collect();
                    let (page, link) = paginate(ctx, uri, &query, refs);
                    reply(200, &link, Some(Value::Array(page)))
                }
                (&Method::GET, ["git", "commits", sha]) => {
                    match state.repos[&id].commits.get(*sha) {
                        Some((tree, parents)) => json_reply(
                            200,
                            json!({"sha": sha, "tree": {"sha": tree},
                                "parents": parents.iter().map(|p| json!({"sha": p})).collect::<Vec<_>>()}),
                        ),
                        None => not_found(),
                    }
                }
                (&Method::GET, ["git", "ref", "heads", name @ ..]) => {
                    let wanted = name.join("/");
                    match state.repos[&id].branches.iter().find(|b| b.name == wanted) {
                        Some(b) => json_reply(
                            200,
                            json!({"ref": format!("refs/heads/{wanted}"), "object": {"type": "commit", "sha": b.sha}}),
                        ),
                        None => not_found(),
                    }
                }
                (&Method::GET, ["commits", sha, "statuses"]) => {
                    let list: Vec<Value> = state.repos[&id]
                        .statuses
                        .iter()
                        .rev()
                        .filter(|(at, _)| at == sha)
                        .map(|(_, v)| v.clone())
                        .collect();
                    json_reply(200, Value::Array(list))
                }
                (&Method::GET, ["rules", "branches", _name @ ..]) => json_reply(
                    200,
                    state.repos[&id].rules.clone().unwrap_or_else(|| json!([])),
                ),
                (&Method::POST, ["statuses", sha]) => {
                    let b = json_body();
                    let valid_state = matches!(
                        b["state"].as_str(),
                        Some("pending" | "success" | "failure" | "error")
                    );
                    if !valid_state
                        || sha.len() != 40
                        || !sha.bytes().all(|c| c.is_ascii_hexdigit())
                    {
                        return json_reply(422, json!({"message": "Validation Failed"}));
                    }
                    let status = json!({"state": b["state"], "context": b["context"], "description": b["description"]});
                    let creator = state.status_creator.clone().unwrap_or_else(|| match &who {
                        Who::Reader(login, ..) => login.clone(),
                        Who::Installation(_) => format!("{APP_SLUG}[bot]"),
                    });
                    let mut stored = status.clone();
                    stored["creator"] = json!({"login": creator});
                    state
                        .repos
                        .get_mut(&id)
                        .unwrap()
                        .statuses
                        .push((sha.to_string(), stored));
                    if let Some(tip) = state.advance_base_after_status.take() {
                        state.repos.get_mut(&id).unwrap().branches[0].sha = tip;
                    }
                    if std::mem::take(&mut state.lose_next_status_reply) {
                        return reply(502, &[], None);
                    }
                    json_reply(201, status)
                }
                _ => not_found(),
            }
        }
        _ => not_found(),
    }
}

impl Drop for FakeGithub {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
