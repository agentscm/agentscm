//! GitHub client for hub sync: app JWTs, webhook signatures and the REST and
//! GraphQL reads and writes the sync uses. Credentials are arguments, never
//! stored here, and never appear in an error.
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use reqwest::Method;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, IF_NONE_MATCH};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};

use super::forge_http::{self, Full, SendError, send_capped};
use super::store::{unix_now, validate_repository};
use super::{HubError, Result};
use crate::hmac::{constant_time_eq, hmac_sha256};

/// PR lists and file lists outgrow the 64 KiB cap of sign-in calls.
pub(super) const API_BODY_BYTES: usize = 1 << 20;
pub(super) const PER_PAGE: u32 = 100;
pub const MAX_PULL_PAGES: u32 = 3;
pub const MAX_FILE_PAGES: u32 = 3;
pub const MAX_EXTERNAL_PAGES: u32 = 3;
pub(super) const MAX_COMMENT_PAGES: u32 = 3;
const MAX_REPO_PAGES: u32 = 10;
pub const MAX_RECENT_BRANCHES: usize = 200;
/// A rate limit never moves the next poll less than this far out.
const MIN_BACKOFF_SECONDS: i64 = 60;

/// How a GitHub call failed. Texts hold a status and an endpoint kind only.
#[derive(Debug)]
pub enum GhError {
    /// 401, or 403 with no rate-limit headers: the credential does not work.
    Auth(String),
    /// Rate limited; do not call again before this unix second.
    Limited(i64),
    Failed(String),
    /// The reply body passed [`API_BODY_BYTES`]; a smaller page may fit. Anywhere but a
    /// size-adaptive read it is a `Failed` with the same text.
    TooLarge(String),
}

impl From<SendError> for GhError {
    fn from(error: SendError) -> Self {
        match error {
            SendError::TooLarge => Self::TooLarge(TOO_LARGE_DETAIL.into()),
            SendError::Failed(error) => error.into(),
        }
    }
}

impl From<HubError> for GhError {
    fn from(error: HubError) -> Self {
        Self::Failed(error.to_string())
    }
}

/// What `GhError::Failed` carried for an oversized reply, kept for every text built from it.
const TOO_LARGE_DETAIL: &str = "unavailable: forge response too large";

/// A commit status state the forge accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusState {
    Pending,
    Success,
    Failure,
    Error,
}

impl StatusState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Error => "error",
        }
    }
}

/// A commit status checked before any request: 40-hex sha, bounded context and description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitStatus {
    sha: String,
    state: StatusState,
    context: String,
    description: String,
    target_url: Option<String>,
}

impl CommitStatus {
    pub(super) fn sha(&self) -> &str {
        &self.sha
    }

    pub(super) fn state(&self) -> StatusState {
        self.state
    }

    pub(super) fn context(&self) -> &str {
        &self.context
    }

    pub(super) fn description(&self) -> &str {
        &self.description
    }

    pub fn new(sha: &str, state: StatusState, context: &str, description: &str) -> Result<Self> {
        let text_ok = |text: &str, max: usize| {
            text.chars().count() <= max && !text.chars().any(char::is_control)
        };
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(HubError::Invalid("status sha is not 40 hex digits".into()));
        }
        if context.is_empty() || !text_ok(context, 255) || !text_ok(description, 140) {
            return Err(HubError::Invalid(
                "status context or description is invalid".into(),
            ));
        }
        Ok(Self {
            sha: sha.to_ascii_lowercase(),
            state,
            context: context.to_owned(),
            description: description.to_owned(),
            target_url: None,
        })
    }

    /// Links the status to `url`, an http(s) URL without control characters.
    pub fn with_target_url(mut self, url: &str) -> Result<Self> {
        let scheme_ok = url.starts_with("https://") || url.starts_with("http://");
        if !scheme_ok || url.chars().count() > 255 || url.chars().any(char::is_control) {
            return Err(HubError::Invalid("status target URL is invalid".into()));
        }
        self.target_url = Some(url.to_owned());
        Ok(self)
    }
}

/// The live facts about one pull request, field for field what a forge check evaluates.
/// `head_repo_id` is 0 when the head repository is gone, which matches no connected repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullHead {
    pub open: bool,
    /// Merged pull requests are left alone: their head is history, not a candidate.
    pub merged: bool,
    /// The merge commit of a merged pull request, when the forge names one.
    pub merge_commit: Option<String>,
    pub head_sha: String,
    pub head_repo_id: u64,
    pub base_repo_id: u64,
    pub base_ref: String,
    pub base_tip: String,
    pub head_tree: String,
    pub head_parents: Vec<String>,
}

pub type Gh<T> = std::result::Result<T, GhError>;

pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub etag: Option<String>,
    pub next: bool,
    pub last_page: Option<u32>,
}

impl Reply {
    /// The body of a 2xx reply.
    pub(super) fn ok(self, kind: &str) -> Gh<Value> {
        if (200..300).contains(&self.status) {
            Ok(self.body)
        } else {
            Err(GhError::Failed(format!("{} from {kind}", self.status)))
        }
    }
}

/// One explicitly sized page of closed proposals.
pub struct ClosedPage {
    pub items: Vec<Value>,
    /// The forge links a further page.
    pub next: bool,
}

pub struct Paged {
    pub items: Vec<Value>,
    pub etag: Option<String>,
    /// Pages read.
    pub pages: u32,
    /// Pages left unread past the cap.
    pub unread: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    pub repository_id: i64,
    pub full_name: String,
    pub node_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub sha: String,
    pub committed_at: String,
}

impl Branch {
    pub fn data(&self) -> String {
        json!({"name": self.name, "sha": self.sha, "committed_at": self.committed_at}).to_string()
    }
}

/// A commit's tree and parents.
type CommitParts = (String, Vec<String>);

pub struct Api {
    client: reqwest::Client,
    base: String,
    graphql: String,
    /// Tree and parents per (repository, commit); a commit never changes.
    /// ponytail: cleared whole past 256 entries; an LRU only matters beyond that many live heads.
    commits: Mutex<HashMap<(String, String), CommitParts>>,
}

impl Api {
    /// For GHES (`api` ends in `/api/v3`) GraphQL is at `{web}/api/graphql`.
    pub fn new(api: &str, web: &str) -> Result<Self> {
        let graphql = if api.ends_with("/api/v3") {
            format!("{web}/api/graphql")
        } else {
            format!("{api}/graphql")
        };
        Ok(Self {
            client: forge_http::client()?,
            base: api.to_owned(),
            graphql,
            commits: Mutex::default(),
        })
    }

    async fn call(
        &self,
        token: &str,
        method: Method,
        url: &str,
        body: Option<Value>,
        etag: Option<&str>,
    ) -> Gh<Reply> {
        self.send(token, method, url, body, etag, true).await
    }

    /// Like `call` for a write: a 403 is the forge refusing this change (a protected branch, a
    /// ruleset), not a credential that stopped working, so only a 401 is an auth failure.
    async fn write(&self, token: &str, method: Method, url: &str, body: Value) -> Gh<Reply> {
        self.send(token, method, url, Some(body), None, false).await
    }

    async fn send(
        &self,
        token: &str,
        method: Method,
        url: &str,
        body: Option<Value>,
        etag: Option<&str>,
        strict: bool,
    ) -> Gh<Reply> {
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(token)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        classify_with(
            endpoint_kind(url, &self.base),
            send_capped(request, API_BODY_BYTES).await?,
            strict,
        )
    }

    async fn get(&self, token: &str, path: &str) -> Gh<Reply> {
        self.call(
            token,
            Method::GET,
            &format!("{}{path}", self.base),
            None,
            None,
        )
        .await
    }

    /// Up to `max_pages` pages of an array (or of the array at `key`). `etag` goes
    /// with page 1 only; a 304 is `None`.
    async fn paged(
        &self,
        token: &str,
        path: &str,
        key: Option<&str>,
        max_pages: u32,
        etag: Option<&str>,
    ) -> Gh<Option<Paged>> {
        self.paged_with(token, path, key, max_pages, etag, false)
            .await
    }

    /// [`paged`](Self::paged); with `require_array` a 2xx body without the array fails the read
    /// instead of reading as an empty page.
    async fn paged_with(
        &self,
        token: &str,
        path: &str,
        key: Option<&str>,
        max_pages: u32,
        etag: Option<&str>,
        require_array: bool,
    ) -> Gh<Option<Paged>> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut items = Vec::new();
        let mut first_etag = None;
        for page in 1..=max_pages {
            let url = format!("{}{path}{sep}per_page={PER_PAGE}&page={page}", self.base);
            let wanted = if page == 1 { etag } else { None };
            let reply = self.call(token, Method::GET, &url, None, wanted).await?;
            if reply.status == 304 {
                return Ok(None);
            }
            let (next, last, tag) = (reply.next, reply.last_page, reply.etag.clone());
            let body = reply.ok(endpoint_kind(&url, &self.base))?;
            let list = key.map_or(&body, |key| &body[key]);
            match list.as_array() {
                Some(list) => items.extend(list.iter().cloned()),
                None if require_array => {
                    return Err(GhError::Failed(format!(
                        "unreadable reply from {}",
                        endpoint_kind(&url, &self.base)
                    )));
                }
                None => {}
            }
            if page == 1 {
                first_etag = tag;
            }
            if !next {
                return Ok(Some(Paged {
                    items,
                    etag: first_etag,
                    pages: page,
                    unread: 0,
                }));
            }
            if page == max_pages {
                let unread = last.map_or(1, |last| last.saturating_sub(max_pages).max(1));
                return Ok(Some(Paged {
                    items,
                    etag: first_etag,
                    pages: page,
                    unread,
                }));
            }
        }
        Ok(Some(Paged {
            items,
            etag: first_etag,
            pages: max_pages,
            unread: 0,
        }))
    }

    /// `POST /app-manifests/{code}/conversions`; the code is the only credential.
    pub async fn convert_manifest(&self, code: &str) -> Gh<Value> {
        let url = format!("{}/app-manifests/{code}/conversions", self.base);
        let request = self
            .client
            .post(url)
            .header(ACCEPT, "application/vnd.github+json");
        let full = send_capped(request, API_BODY_BYTES).await?;
        if full.status != 201 {
            return Err(GhError::Failed(format!(
                "{} from app-manifests",
                full.status
            )));
        }
        Ok(full.body)
    }

    pub async fn installations(&self, jwt: &str) -> Gh<Vec<(i64, String)>> {
        let paged = self
            .paged(jwt, "/app/installations", None, MAX_REPO_PAGES, None)
            .await?;
        Ok(paged
            .map(|p| p.items)
            .unwrap_or_default()
            .iter()
            .filter_map(|item| {
                let account = item["account"]["login"].as_str().unwrap_or_default();
                Some((
                    item["id"].as_i64().filter(|id| *id > 0)?,
                    account.to_owned(),
                ))
            })
            .collect())
    }

    /// An installation token and the unix second it expires.
    pub async fn installation_token(&self, jwt: &str, installation: i64) -> Gh<(String, i64)> {
        let url = format!(
            "{}/app/installations/{installation}/access_tokens",
            self.base
        );
        let reply = self
            .call(jwt, Method::POST, &url, Some(json!({})), None)
            .await?;
        let body = reply.ok("app")?;
        let token = body["token"].as_str().filter(|t| !t.is_empty());
        // An unreadable expiry is treated as expiring now, so the token is not cached.
        let expires = body["expires_at"]
            .as_str()
            .and_then(parse_rfc3339)
            .unwrap_or(0);
        match token {
            Some(token) => Ok((token.to_owned(), expires)),
            None => Err(GhError::Failed("no token from app".into())),
        }
    }

    pub async fn installation_repos(&self, token: &str) -> Gh<Vec<RepoInfo>> {
        let paged = self
            .paged(
                token,
                "/installation/repositories",
                Some("repositories"),
                MAX_REPO_PAGES,
                None,
            )
            .await?;
        Ok(paged
            .map(|p| p.items)
            .unwrap_or_default()
            .iter()
            .filter_map(repo_info)
            .collect())
    }

    /// None when the repository is missing or unreadable (404).
    pub async fn repo(&self, token: &str, full_name: &str) -> Gh<Option<RepoInfo>> {
        let reply = self.get(token, &format!("/repos/{full_name}")).await?;
        if reply.status == 404 {
            return Ok(None);
        }
        let body = reply.ok("repos")?;
        repo_info(&body)
            .map(Some)
            .ok_or_else(|| GhError::Failed("unreadable reply from repos".into()))
    }

    /// The repository and `login`'s role on it, read with the Hub's own credential: None when
    /// the repository is missing (404), no role when the user is unknown to it. A 401 or 403
    /// means the credential cannot answer.
    pub async fn collaborator_permission(
        &self,
        token: &str,
        full_name: &str,
        login: &str,
    ) -> Gh<Option<(Value, Option<Value>)>> {
        let login = login
            .bytes()
            // Enterprise Managed User logins also have `_`.
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            .then_some(login)
            .filter(|login| !login.is_empty())
            .ok_or_else(|| GhError::Failed("unusable login".into()))?;
        let repo = self.get(token, &format!("/repos/{full_name}")).await?;
        let path = format!("/repos/{full_name}/collaborators/{login}/permission");
        let permission = match repo.status {
            404 => return Ok(None),
            _ => self.get(token, &path).await?,
        };
        let Some(repo) = answer(repo, "repos")? else {
            return Ok(None);
        };
        Ok(Some((repo, answer(permission, "collaborators")?)))
    }

    /// A repository's JSON, read with the Hub's own credential; None for a 404.
    pub async fn repo_json(&self, token: &str, full_name: &str) -> Gh<Option<Value>> {
        answer(
            self.get(token, &format!("/repos/{full_name}")).await?,
            "repos",
        )
    }

    /// The connected forge's explicit default identity; absence is unsupported.
    pub async fn default_branch(&self, token: &str, full_name: &str) -> Gh<String> {
        let body = self
            .get(token, &format!("/repos/{full_name}"))
            .await?
            .ok("repos")?;
        body["default_branch"]
            .as_str()
            .filter(|name| valid_ref_name(name) && *name != "HEAD" && !name.starts_with("refs/"))
            .map(str::to_owned)
            .ok_or_else(|| {
                GhError::Failed(
                    "unsupported candidate profile: forge default branch is unknown".into(),
                )
            })
    }

    /// A file of at most `max` bytes at `branch`; None when it does not exist. A directory, a
    /// larger file or an unreadable reply is an error.
    pub async fn file(
        &self,
        token: &str,
        full_name: &str,
        path: &str,
        branch: &str,
        max: usize,
    ) -> Gh<Option<Vec<u8>>> {
        if !valid_ref_name(branch) {
            return Err(GhError::Failed("bad branch name".into()));
        }
        let url = format!(
            "/repos/{full_name}/contents/{}?ref={}",
            encode_path(path),
            encode_path(branch)
        );
        let reply = self.get(token, &url).await?;
        if reply.status == 404 {
            return Ok(None);
        }
        let body = reply.ok("contents")?;
        let unreadable = || GhError::Failed("unreadable reply from contents".into());
        if body["type"] != "file" || body["encoding"] != "base64" {
            return Err(unreadable());
        }
        if body["size"].as_u64().is_none_or(|size| size > max as u64) {
            return Err(GhError::Failed(format!("file is larger than {max} bytes")));
        }
        let text: String = body["content"]
            .as_str()
            .ok_or_else(unreadable)?
            .split_whitespace()
            .collect();
        let bytes = STANDARD.decode(text).map_err(|_| unreadable())?;
        if bytes.len() > max {
            return Err(unreadable());
        }
        Ok(Some(bytes))
    }

    /// The login the token acts as.
    pub async fn user(&self, token: &str) -> Gh<(i64, String)> {
        let body = self.get(token, "/user").await?.ok("user")?;
        match (body["id"].as_i64(), body["login"].as_str()) {
            (Some(id), Some(login)) if id > 0 => Ok((id, login.to_owned())),
            _ => Err(GhError::Failed("unreadable reply from user".into())),
        }
    }

    /// Open pulls, at most [`MAX_PULL_PAGES`] pages; `None` when `etag` still matches.
    pub async fn open_pulls(
        &self,
        token: &str,
        full_name: &str,
        etag: Option<&str>,
    ) -> Gh<Option<Paged>> {
        let path = format!("/repos/{full_name}/pulls?state=open");
        self.paged(token, &path, None, MAX_PULL_PAGES, etag).await
    }

    /// Bounded recent closed proposals for opt-in local rule reconciliation. Each
    /// candidate is re-read individually before it can be accepted as merged.
    pub async fn recent_closed_pulls(&self, token: &str, full_name: &str) -> Gh<Paged> {
        let path = format!("/repos/{full_name}/pulls?state=closed&sort=updated&direction=desc");
        self.paged_with(token, &path, None, 1, None, true)
            .await?
            .ok_or_else(|| GhError::Failed("unexpected 304 from pulls".into()))
    }

    /// One page of closed proposals, newest updated first. A reply that is not an array fails
    /// the read; a body past [`API_BODY_BYTES`] is [`GhError::TooLarge`].
    pub async fn closed_pulls_page(
        &self,
        token: &str,
        full_name: &str,
        per_page: u32,
        page: u32,
    ) -> Gh<ClosedPage> {
        let path = format!(
            "/repos/{full_name}/pulls?state=closed&sort=updated&direction=desc&per_page={per_page}&page={page}"
        );
        let reply = self.get(token, &path).await?;
        let next = reply.next;
        let body = reply.ok("pulls")?;
        let items = body
            .as_array()
            .ok_or_else(|| GhError::Failed("unreadable reply from pulls".into()))?
            .clone();
        Ok(ClosedPage { items, next })
    }

    /// Every item of a list endpoint, at most `max_pages` pages. A reply that is not an array
    /// fails the read; `unread` says pages were left past the cap.
    pub async fn get_all(&self, token: &str, path: &str, max_pages: u32) -> Gh<Paged> {
        self.paged_with(token, path, None, max_pages, None, true)
            .await?
            .ok_or_else(|| GhError::Failed("unexpected 304".into()))
    }

    pub async fn pull(&self, token: &str, full_name: &str, number: i64) -> Gh<Option<Value>> {
        let reply = self
            .get(token, &format!("/repos/{full_name}/pulls/{number}"))
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        reply.ok("repos").map(Some)
    }

    /// Like [`pull`](Self::pull), but a 403 is a refusal of this read, not a failed credential.
    pub async fn pull_soft(&self, token: &str, full_name: &str, number: i64) -> Gh<Option<Value>> {
        let url = format!("{}/repos/{full_name}/pulls/{number}", self.base);
        let reply = self
            .send(token, Method::GET, &url, None, None, false)
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        reply.ok("repos").map(Some)
    }

    /// One installation of the App, read with the App's JWT: its granted permissions and the
    /// events it is subscribed to. None when GitHub does not know it (404).
    pub async fn installation(&self, jwt: &str, installation: i64) -> Gh<Option<Value>> {
        let reply = self
            .get(jwt, &format!("/app/installations/{installation}"))
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        reply.ok("app").map(Some)
    }

    /// The current merge queue branches of `base` as (branch name, commit), at most one page of
    /// 100, and whether that is all of them. None when GitHub refuses the read.
    pub async fn queue_branches(
        &self,
        token: &str,
        full_name: &str,
        base: &str,
    ) -> Gh<Option<(Vec<(String, String)>, bool)>> {
        if !valid_ref_name(base) {
            return Err(GhError::Failed("bad branch name".into()));
        }
        let url = format!(
            "{}/repos/{full_name}/git/matching-refs/heads/gh-readonly-queue/{}/?per_page=100",
            self.base,
            encode_path(base)
        );
        let reply = self
            .send(token, Method::GET, &url, None, None, false)
            .await?;
        if matches!(reply.status, 403 | 404) {
            return Ok(None);
        }
        let complete = !reply.next;
        let body = reply.ok("git")?;
        let refs = body
            .as_array()
            .ok_or_else(|| GhError::Failed("unreadable reply from git".into()))?
            .iter()
            .map(|item| {
                let name = item["ref"]
                    .as_str()
                    .and_then(|r| r.strip_prefix("refs/heads/"));
                match (name, item["object"]["sha"].as_str()) {
                    (Some(name), Some(sha)) => Ok((name.to_owned(), sha.to_owned())),
                    _ => Err(GhError::Failed("unreadable reply from git".into())),
                }
            })
            .collect::<Gh<Vec<_>>>()?;
        Ok(Some((refs, complete)))
    }

    /// The ruleset rules that apply to `branch` (read-only). None when GitHub refuses or does not
    /// know the read (403, 404): the requirement is then unknown, not absent.
    pub async fn branch_rules(
        &self,
        token: &str,
        full_name: &str,
        branch: &str,
    ) -> Gh<Option<Value>> {
        if !valid_ref_name(branch) {
            return Err(GhError::Failed("bad branch name".into()));
        }
        let url = format!(
            "{}/repos/{full_name}/rules/branches/{}?per_page=100",
            self.base,
            encode_path(branch)
        );
        let reply = self
            .send(token, Method::GET, &url, None, None, false)
            .await?;
        if matches!(reply.status, 403 | 404) {
            return Ok(None);
        }
        let complete = !reply.next;
        let body = reply.ok("repos")?;
        Ok(complete.then_some(body))
    }

    /// The newest page (up to 100) of commit statuses on `sha`, newest first, and whether it is
    /// the whole list. None when GitHub refuses the read.
    pub async fn status_page(
        &self,
        token: &str,
        full_name: &str,
        sha: &str,
    ) -> Gh<Option<(Value, bool)>> {
        let url = format!(
            "{}/repos/{full_name}/commits/{sha}/statuses?per_page=100",
            self.base
        );
        let reply = self
            .send(token, Method::GET, &url, None, None, false)
            .await?;
        if matches!(reply.status, 403 | 404) {
            return Ok(None);
        }
        let complete = !reply.next;
        Ok(Some((reply.ok("repos")?, complete)))
    }

    /// Workflow runs for exactly `sha`, at most [`MAX_EXTERNAL_PAGES`] pages, and whether the
    /// list is complete. Read-only: no logs, no reruns.
    pub async fn workflow_runs(
        &self,
        token: &str,
        full_name: &str,
        sha: &str,
    ) -> Gh<(Vec<Value>, bool)> {
        let path = format!("/repos/{full_name}/actions/runs?head_sha={sha}");
        self.external_list(&path, Some("workflow_runs"), token)
            .await
    }

    /// Deployments of exactly `sha`; bounded like [`workflow_runs`](Self::workflow_runs).
    pub async fn deployments(
        &self,
        token: &str,
        full_name: &str,
        sha: &str,
    ) -> Gh<(Vec<Value>, bool)> {
        let path = format!("/repos/{full_name}/deployments?sha={sha}");
        self.external_list(&path, None, token).await
    }

    /// The newest status of a deployment; `None` when it has none or is gone.
    pub async fn deployment_status(
        &self,
        token: &str,
        full_name: &str,
        id: i64,
    ) -> Gh<Option<Value>> {
        let path = format!("/repos/{full_name}/deployments/{id}/statuses?per_page=1");
        let reply = self.get(token, &path).await.map_err(forbidden)?;
        if reply.status == 404 {
            return Ok(None);
        }
        Ok(reply
            .ok("repos")
            .map_err(forbidden)?
            .as_array()
            .and_then(|list| list.first().cloned()))
    }

    async fn external_list(
        &self,
        path: &str,
        key: Option<&str>,
        token: &str,
    ) -> Gh<(Vec<Value>, bool)> {
        let paged = self
            .paged_with(token, path, key, MAX_EXTERNAL_PAGES, None, true)
            .await
            .map_err(forbidden)?;
        let paged = paged.ok_or_else(|| GhError::Failed("unexpected 304 from repos".into()))?;
        Ok((paged.items, paged.unread == 0))
    }

    /// Changed paths (renames count both names), and whether more files exist.
    pub async fn pull_files(
        &self,
        token: &str,
        full_name: &str,
        number: i64,
    ) -> Gh<(Vec<String>, bool)> {
        let path = format!("/repos/{full_name}/pulls/{number}/files");
        let paged = self.paged(token, &path, None, MAX_FILE_PAGES, None).await?;
        let paged = paged.ok_or_else(|| GhError::Failed("unexpected 304 from repos".into()))?;
        let mut files: Vec<String> = paged
            .items
            .iter()
            .flat_map(|file| ["filename", "previous_filename"].map(|k| file[k].as_str()))
            .flatten()
            .map(str::to_owned)
            .collect();
        files.sort();
        files.dedup();
        Ok((files, paged.unread > 0))
    }

    /// None when the branch is missing, or the name is not a plausible git ref (never sent).
    pub async fn branch(&self, token: &str, full_name: &str, name: &str) -> Gh<Option<Branch>> {
        if !valid_ref_name(name) {
            return Ok(None);
        }
        let reply = self
            .get(
                token,
                &format!("/repos/{full_name}/branches/{}", encode_path(name)),
            )
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        let body = reply.ok("repos")?;
        // A reply for some other branch is not an answer.
        if body["name"].as_str() != Some(name) {
            return Ok(None);
        }
        match (
            body["commit"]["sha"].as_str(),
            body["commit"]["commit"]["committer"]["date"].as_str(),
        ) {
            (Some(sha), Some(date)) => Ok(Some(Branch {
                name: name.to_owned(),
                sha: sha.to_owned(),
                committed_at: date.to_owned(),
            })),
            _ => Err(GhError::Failed("unreadable reply from repos".into())),
        }
    }

    /// Branches whose tip commit is at or after `since` (unix seconds), newest first, at most
    /// [`MAX_RECENT_BRANCHES`]; true when that cap cut the list short.
    pub async fn recent_branches(
        &self,
        token: &str,
        full_name: &str,
        since: i64,
    ) -> Gh<(Vec<Branch>, bool)> {
        const QUERY: &str = "query($owner:String!,$name:String!,$after:String){repository(owner:$owner,name:$name){refs(refPrefix:\"refs/heads/\",first:100,after:$after,orderBy:{field:TAG_COMMIT_DATE,direction:DESC}){pageInfo{hasNextPage endCursor} nodes{name target{oid ... on Commit{committedDate}}}}}}";
        let (owner, name) = full_name
            .split_once('/')
            .ok_or_else(|| GhError::Failed("bad repository name".into()))?;
        let mut found = Vec::new();
        let mut after = Value::Null;
        loop {
            let body = json!({"query": QUERY,
                "variables": {"owner": owner, "name": name, "after": after}});
            let reply = self
                .call(token, Method::POST, &self.graphql, Some(body), None)
                .await?;
            let body = reply.ok("graphql")?;
            let refs = &body["data"]["repository"]["refs"];
            let errors = body["errors"].as_array().into_iter().flatten();
            if errors.clone().any(|e| e["type"] == "RATE_LIMITED") {
                return Err(GhError::Limited(unix_now() + MIN_BACKOFF_SECONDS));
            }
            if !body["errors"].is_null() || refs.is_null() {
                return Err(GhError::Failed("graphql refs unavailable".into()));
            }
            for node in refs["nodes"].as_array().into_iter().flatten() {
                let (Some(name), Some(sha), Some(date)) = (
                    node["name"].as_str(),
                    node["target"]["oid"].as_str(),
                    node["target"]["committedDate"].as_str(),
                ) else {
                    continue;
                };
                // Newest first, so the first old ref ends the walk.
                if parse_rfc3339(date).is_none_or(|at| at < since) {
                    return Ok((found, false));
                }
                if found.len() >= MAX_RECENT_BRANCHES {
                    return Ok((found, true));
                }
                found.push(Branch {
                    name: name.to_owned(),
                    sha: sha.to_owned(),
                    committed_at: date.to_owned(),
                });
            }
            match (
                refs["pageInfo"]["hasNextPage"].as_bool(),
                refs["pageInfo"]["endCursor"].as_str(),
            ) {
                (Some(true), Some(cursor)) => after = json!(cursor),
                _ => return Ok((found, false)),
            }
        }
    }

    /// `(comment id, author login, body)` of the first three pages of comments.
    pub async fn comments(
        &self,
        token: &str,
        full_name: &str,
        number: i64,
    ) -> Gh<Vec<(i64, String, String)>> {
        Ok(self
            .comments_pages(token, full_name, number, MAX_COMMENT_PAGES)
            .await?
            .0)
    }

    /// Like [`comments`](Self::comments) over up to `max_pages` pages; true when pages were left unread.
    pub async fn comments_pages(
        &self,
        token: &str,
        full_name: &str,
        number: i64,
        max_pages: u32,
    ) -> Gh<(Vec<(i64, String, String)>, bool)> {
        let path = format!("/repos/{full_name}/issues/{number}/comments");
        let paged = self.paged(token, &path, None, max_pages, None).await?;
        let truncated = paged.as_ref().is_some_and(|p| p.unread > 0);
        let comments = paged
            .map(|p| p.items)
            .unwrap_or_default()
            .iter()
            .filter_map(|c| {
                let login = c["user"]["login"].as_str().unwrap_or_default();
                Some((
                    c["id"].as_i64()?,
                    login.to_owned(),
                    c["body"].as_str()?.to_owned(),
                ))
            })
            .collect();
        Ok((comments, truncated))
    }

    pub async fn create_comment(
        &self,
        token: &str,
        full_name: &str,
        number: i64,
        body: &str,
    ) -> Gh<i64> {
        let url = format!("{}/repos/{full_name}/issues/{number}/comments", self.base);
        let reply = self
            .call(token, Method::POST, &url, Some(json!({"body": body})), None)
            .await?;
        reply.ok("repos")?["id"]
            .as_i64()
            .ok_or_else(|| GhError::Failed("unreadable reply from repos".into()))
    }

    /// False when the comment no longer exists.
    pub async fn edit_comment(
        &self,
        token: &str,
        full_name: &str,
        comment: i64,
        body: &str,
    ) -> Gh<bool> {
        let url = format!("{}/repos/{full_name}/issues/comments/{comment}", self.base);
        let reply = self
            .call(
                token,
                Method::PATCH,
                &url,
                Some(json!({"body": body})),
                None,
            )
            .await?;
        if reply.status == 404 {
            return Ok(false);
        }
        reply.ok("repos").map(|_| true)
    }

    // ---- publishing: git data and pull request writes ----

    /// The commit a branch points at; None when it does not exist.
    pub async fn ref_sha(&self, token: &str, full_name: &str, branch: &str) -> Gh<Option<String>> {
        if !valid_ref_name(branch) {
            return Err(GhError::Failed("bad branch name".into()));
        }
        let path = format!("/repos/{full_name}/git/ref/heads/{}", encode_path(branch));
        let reply = self.get(token, &path).await?;
        if reply.status == 404 {
            return Ok(None);
        }
        let body = reply.ok("git")?;
        match (body["ref"].as_str(), body["object"]["sha"].as_str()) {
            (Some(name), Some(sha)) if name == format!("refs/heads/{branch}") => {
                Ok(Some(sha.to_owned()))
            }
            _ => Err(GhError::Failed("unreadable reply from git".into())),
        }
    }

    /// The tree of a commit; None when the commit is not on the forge.
    pub async fn commit_tree(
        &self,
        token: &str,
        full_name: &str,
        commit: &str,
    ) -> Gh<Option<String>> {
        let reply = self
            .get(token, &format!("/repos/{full_name}/git/commits/{commit}"))
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        let body = reply.ok("git")?;
        Ok(Some(sha_of(&body["tree"])?))
    }

    /// The tree and parents of a commit; None when the commit is not on the forge.
    pub(super) async fn commit_parts(
        &self,
        token: &str,
        full_name: &str,
        commit: &str,
    ) -> Gh<Option<(String, Vec<String>)>> {
        let key = (full_name.to_ascii_lowercase(), commit.to_owned());
        if let Some(found) = self
            .commits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(Some(found.clone()));
        }
        let reply = self
            .get(token, &format!("/repos/{full_name}/git/commits/{commit}"))
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        let body = reply.ok("git")?;
        let parents = body["parents"]
            .as_array()
            .ok_or_else(|| GhError::Failed("unreadable reply from git".into()))?
            .iter()
            .map(sha_of)
            .collect::<Gh<Vec<_>>>()?;
        let parts = (sha_of(&body["tree"])?, parents);
        let mut commits = self.commits.lock().unwrap_or_else(PoisonError::into_inner);
        if commits.len() >= 256 {
            commits.clear();
        }
        commits.insert(key, parts.clone());
        Ok(Some(parts))
    }

    /// Everything a forge check needs from one pull request: the pull, the live tip of its
    /// base ref and the head commit's tree and parents. None when the pull does not exist.
    /// A missing or ill-typed field is an error; a deleted head repository is id 0.
    /// `base_tips` holds the base ref tips already read in this pass, so each is read once.
    pub async fn pull_head(
        &self,
        token: &str,
        full_name: &str,
        number: i64,
        base_tips: &mut HashMap<String, String>,
    ) -> Gh<Option<PullHead>> {
        let bad = || GhError::Failed("unreadable reply from pulls".into());
        let Some(pull) = self.pull(token, full_name, number).await? else {
            return Ok(None);
        };
        let open = match pull["state"].as_str() {
            Some("open") => true,
            Some("closed") => false,
            _ => return Err(bad()),
        };
        let merged = pull["merged"].as_bool().ok_or_else(bad)?;
        let merge_commit = match &pull["merge_commit_sha"] {
            Value::String(sha) if merged => Some(sha.clone()),
            Value::Null => None,
            _ if !merged => None,
            _ => return Err(bad()),
        };
        if !open {
            // Only the head matters to retire a success; the base and head commit may be gone.
            return Ok(Some(PullHead {
                open,
                merged,
                merge_commit,
                head_sha: sha_of(&pull["head"])?,
                head_repo_id: 0,
                base_repo_id: 0,
                base_ref: String::new(),
                base_tip: String::new(),
                head_tree: String::new(),
                head_parents: Vec::new(),
            }));
        }
        let repo_id = |repo: Option<&Value>| match repo {
            Some(Value::Null) => Ok(0),
            Some(repo) => repo["id"].as_u64().filter(|id| *id > 0).ok_or_else(bad),
            None => Err(bad()),
        };
        let head_repo_id = repo_id(pull["head"].get("repo"))?;
        let base_repo_id = match pull["base"].get("repo") {
            Some(Value::Null) | None => return Err(bad()),
            repo => repo_id(repo)?,
        };
        let head_sha = sha_of(&pull["head"])?;
        let base_ref = pull["base"]["ref"]
            .as_str()
            .filter(|name| valid_ref_name(name))
            .ok_or_else(bad)?
            .to_owned();
        let base_tip = match base_tips.get(&base_ref) {
            Some(tip) => tip.clone(),
            None => {
                let tip = self
                    .ref_sha(token, full_name, &base_ref)
                    .await?
                    .ok_or_else(|| GhError::Failed("base branch not found".into()))?;
                base_tips.insert(base_ref.clone(), tip.clone());
                tip
            }
        };
        let (head_tree, head_parents) = self
            .commit_parts(token, full_name, &head_sha)
            .await?
            .ok_or_else(|| GhError::Failed("head commit not found".into()))?;
        Ok(Some(PullHead {
            open,
            merged,
            merge_commit,
            head_sha,
            head_repo_id,
            base_repo_id,
            base_ref,
            base_tip,
            head_tree,
            head_parents,
        }))
    }

    /// Posts a commit status. `Ok(Err(status))` is a definite refusal (403, 404 or 422); `Auth`
    /// and `Limited` mean it was not applied. Any other `Err` (no reply, 5xx, an unreadable or
    /// different reply) is uncertain: the status may or may not exist.
    pub async fn post_commit_status(
        &self,
        token: &str,
        full_name: &str,
        status: &CommitStatus,
    ) -> Gh<std::result::Result<(), u16>> {
        let url = format!("{}/repos/{full_name}/statuses/{}", self.base, status.sha);
        let mut body = json!({"state": status.state.as_str(), "context": status.context,
            "description": status.description});
        if let Some(target) = &status.target_url {
            body["target_url"] = json!(target);
        }
        let reply = self.write(token, Method::POST, &url, body).await?;
        if matches!(reply.status, 403 | 404 | 422) {
            return Ok(Err(reply.status));
        }
        if reply.status != 201
            || reply.body["state"] != status.state.as_str()
            || reply.body["context"] != status.context.as_str()
        {
            return Err(GhError::Failed(format!("{} from statuses", reply.status)));
        }
        Ok(Ok(()))
    }

    async fn git_write(&self, token: &str, full_name: &str, what: &str, body: Value) -> Gh<Value> {
        let url = format!("{}/repos/{full_name}/git/{what}", self.base);
        let reply = self.write(token, Method::POST, &url, body).await?;
        let (status, message) = (reply.status, refusal(&reply.body));
        if status != 201 {
            return Err(GhError::Failed(format!(
                "{status} from git {what}: {message}"
            )));
        }
        Ok(reply.body)
    }

    /// The SHA the forge gives a blob.
    pub async fn create_blob(&self, token: &str, full_name: &str, bytes: &[u8]) -> Gh<String> {
        let body = json!({"content": STANDARD.encode(bytes), "encoding": "base64"});
        sha_of(&self.git_write(token, full_name, "blobs", body).await?)
    }

    /// `entries` are `{path, mode, type, sha}`; a null `sha` deletes the path from `base_tree`.
    pub async fn create_tree(
        &self,
        token: &str,
        full_name: &str,
        base_tree: &str,
        entries: Vec<Value>,
    ) -> Gh<String> {
        let body = json!({"base_tree": base_tree, "tree": entries});
        sha_of(&self.git_write(token, full_name, "trees", body).await?)
    }

    pub async fn create_commit(&self, token: &str, full_name: &str, body: Value) -> Gh<String> {
        sha_of(&self.git_write(token, full_name, "commits", body).await?)
    }

    /// Creates `refs/heads/{branch}` and never moves one. `Err((status, message))` when the forge
    /// refuses (422: it exists or a rule forbids it; 403: protected or a ruleset).
    pub async fn create_ref(
        &self,
        token: &str,
        full_name: &str,
        branch: &str,
        sha: &str,
    ) -> Gh<std::result::Result<(), (u16, String)>> {
        if !valid_ref_name(branch) {
            return Err(GhError::Failed("bad branch name".into()));
        }
        let url = format!("{}/repos/{full_name}/git/refs", self.base);
        let body = json!({"ref": format!("refs/heads/{branch}"), "sha": sha});
        let reply = self.write(token, Method::POST, &url, body).await?;
        match reply.status {
            201 => Ok(Ok(())),
            status @ (403 | 422) => Ok(Err((status, refusal(&reply.body)))),
            status => Err(GhError::Failed(format!(
                "{status} from git refs: {}",
                refusal(&reply.body)
            ))),
        }
    }

    /// Open pull requests whose head is `owner:branch`. Past the page cap the answer is unknown,
    /// so it is an error rather than a partial list: the caller must not create a duplicate.
    pub async fn find_open_pulls(
        &self,
        token: &str,
        full_name: &str,
        owner: &str,
        branch: &str,
    ) -> Gh<Vec<Value>> {
        let path = format!(
            "/repos/{full_name}/pulls?state=open&head={owner}:{}",
            encode_path(branch)
        );
        let paged = self
            .paged(token, &path, None, MAX_PULL_PAGES, None)
            .await?
            .ok_or_else(|| GhError::Failed("unexpected 304 from pulls".into()))?;
        if paged.unread > 0 {
            return Err(GhError::Failed(
                "too many open pull requests from that branch to search".into(),
            ));
        }
        Ok(paged.items)
    }

    pub async fn create_pull(
        &self,
        token: &str,
        full_name: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Gh<Value> {
        let url = format!("{}/repos/{full_name}/pulls", self.base);
        let request = json!({"title": title, "body": body, "head": head, "base": base});
        let reply = self.write(token, Method::POST, &url, request).await?;
        if reply.status != 201 {
            return Err(GhError::Failed(format!(
                "{} from pulls: {}",
                reply.status,
                refusal(&reply.body)
            )));
        }
        Ok(reply.body)
    }

    pub async fn create_candidate_pull(
        &self,
        token: &str,
        full_name: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Gh<std::result::Result<Value, u16>> {
        let url = format!("{}/repos/{full_name}/pulls", self.base);
        let request = json!({"title": title, "body": body, "head": head, "base": base});
        let reply = self.write(token, Method::POST, &url, request).await?;
        if matches!(reply.status, 403 | 404 | 422) {
            return Ok(Err(reply.status));
        }
        if reply.status != 201 {
            return Err(GhError::Failed(format!(
                "{} from pulls: {}",
                reply.status,
                refusal(&reply.body)
            )));
        }
        Ok(Ok(reply.body))
    }

    pub async fn create_status(
        &self,
        token: &str,
        full_name: &str,
        sha: &str,
        description: &str,
    ) -> Gh<()> {
        let url = format!("{}/repos/{full_name}/statuses/{sha}", self.base);
        let body = json!({"state": "success", "context": "agentscm/coordination",
            "description": description});
        self.call(token, Method::POST, &url, Some(body), None)
            .await?
            .ok("repos")
            .map(drop)
    }
}

fn repo_info(item: &Value) -> Option<RepoInfo> {
    let full_name = item["full_name"].as_str()?;
    validate_repository(full_name).ok()?;
    Some(RepoInfo {
        repository_id: item["id"].as_i64().filter(|id| *id > 0)?,
        full_name: full_name.to_owned(),
        node_id: item["node_id"].as_str().map(str::to_owned),
    })
}

/// A 2xx body, None for a 404, `Auth` for a 401 or 403 (the credential cannot answer).
pub(super) fn answer(reply: Reply, kind: &str) -> Gh<Option<Value>> {
    match reply.status {
        404 => Ok(None),
        401 | 403 => Err(GhError::Auth(format!("{} from {kind}", reply.status))),
        _ => reply.ok(kind).map(Some),
    }
}

/// The first path segment of the URL under `base`: a kind, never an id.
pub(super) fn endpoint_kind<'a>(url: &'a str, base: &str) -> &'a str {
    let path = url.strip_prefix(base).unwrap_or(url);
    path.trim_start_matches('/')
        .split(['/', '?'])
        .next()
        .unwrap_or_default()
}

/// A 403 that was not a rate limit, as `Auth("403 ...")`. A permission refusal carries
/// `x-ratelimit-remaining` on real GitHub, which `classify_with` reads as an ordinary reply,
/// so the external reads normalise both forms for the caller to skip the repository.
fn forbidden(error: GhError) -> GhError {
    match error {
        GhError::Failed(detail) if detail.starts_with("403") => GhError::Auth(detail),
        other => other,
    }
}

fn classify_with(kind: &str, full: Full, strict: bool) -> Gh<Reply> {
    let header = |name: &str| full.headers.get(name).and_then(|v| v.to_str().ok());
    let number = |name: &str| header(name).and_then(|v| v.trim().parse::<i64>().ok());
    let (remaining, retry, reset) = (
        number("x-ratelimit-remaining"),
        number("retry-after"),
        number("x-ratelimit-reset"),
    );
    let limited =
        full.status == 429 || (full.status == 403 && (remaining == Some(0) || retry.is_some()));
    if limited {
        let now = unix_now();
        let until = [
            Some(now + MIN_BACKOFF_SECONDS),
            retry.map(|s| now + s),
            reset,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(now + MIN_BACKOFF_SECONDS);
        return Err(GhError::Limited(until));
    }
    if full.status == 401 || (strict && full.status == 403 && remaining.is_none()) {
        return Err(GhError::Auth(format!("{} from {kind}", full.status)));
    }
    if full.status >= 500 {
        return Err(GhError::Failed(format!("{} from {kind}", full.status)));
    }
    let link = header("link").unwrap_or_default();
    Ok(Reply {
        status: full.status,
        etag: header("etag").map(str::to_owned),
        next: link.contains("rel=\"next\""),
        last_page: last_page(link),
        body: full.body,
    })
}

fn sha_of(body: &Value) -> Gh<String> {
    body["sha"]
        .as_str()
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| GhError::Failed("unreadable reply from git".into()))
}

/// The forge's message for a refused write, bounded and without control characters.
fn refusal(body: &Value) -> String {
    body["message"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}

fn last_page(link: &str) -> Option<u32> {
    let part = link.split(',').find(|part| part.contains("rel=\"last\""))?;
    let at = part.rfind("&page=").or_else(|| part.rfind("?page="))?;
    let digits: String = part[at + 6..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Whether `name` could be a git branch name. Anything else is never put in a URL or looked up.
pub fn valid_ref_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
        && !name.contains("..")
        && !name.contains("@{")
        && !name.ends_with(".lock")
        && name.split('/').all(|segment| segment != ".")
        && !name
            .chars()
            .any(|c| c.is_control() || c == ' ' || "~^:?*[\\".contains(c))
}

/// Percent-encodes everything but letters, digits, `-_~` and `/` (so `.` is encoded too).
fn encode_path(path: &str) -> String {
    path.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'~' | b'/') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `YYYY-MM-DDTHH:MM:SSZ` (or `+00:00`) to unix seconds.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let text = text
        .strip_suffix('Z')
        .or_else(|| text.strip_suffix("+00:00"))?;
    let (date, time) = text.split_once('T')?;
    let num = |s: &str| s.parse::<i64>().ok();
    let mut d = date.split('-');
    let (y, m, day) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
    let mut t = time.split(':');
    let (h, min, sec) = (
        num(t.next()?)?,
        num(t.next()?)?,
        num(t.next()?.split('.').next()?)?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    // Howard Hinnant's days-from-civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + min * 60 + sec)
}

/// A GitHub App's private key, for signing app JWTs.
pub struct AppKey(RsaKeyPair);

impl AppKey {
    /// The PKCS#1 `BEGIN RSA PRIVATE KEY` PEM GitHub returns.
    pub fn from_pem(pem: &str) -> Result<Self> {
        let body: String = pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .flat_map(|line| line.split_whitespace())
            .collect();
        let bad = || HubError::Invalid("app private key is not a PKCS#1 PEM".into());
        if !pem.contains("BEGIN RSA PRIVATE KEY") {
            return Err(bad());
        }
        let der = STANDARD.decode(body).map_err(|_| bad())?;
        RsaKeyPair::from_der(&der).map(Self).map_err(|_| bad())
    }

    /// RS256 JWT: iat 60 s in the past for clock skew, 9 minutes of life.
    pub fn jwt(&self, app_id: i64, now: i64) -> Result<String> {
        let claims = json!({"iat": now - 60, "exp": now + 540, "iss": app_id.to_string()});
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; self.0.public().modulus_len()];
        self.0
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| HubError::Unavailable("could not sign app JWT".into()))?;
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

/// Whether `header` is `sha256=<hex HMAC of body under secret>`, compared in constant time.
pub fn webhook_signature_ok(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(hex) = header.strip_prefix("sha256=") else {
        return false;
    };
    let expected = hmac_sha256(secret.as_bytes(), body);
    let given: Vec<u8> = (0..hex.len() / 2)
        .filter_map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok())
        .collect();
    hex.len() == 64 && constant_time_eq(&given, &expected)
}

pub fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::forge_http::serve_once;

    #[tokio::test]
    async fn closed_pulls_page_asks_for_its_page_and_reports_next() {
        let link = "link: <http://x/p?page=3>; rel=\"next\"\r\n";
        let (base, server) = serve_once(200, link, r#"[{"number":1},{"number":2}]"#.into());
        let api = Api::new(&base, &base).unwrap();
        let page = api.closed_pulls_page("t", "o/r", 25, 2).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.next);
        assert_eq!(
            server.join().unwrap(),
            "GET /repos/o/r/pulls?state=closed&sort=updated&direction=desc&per_page=25&page=2 HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn closed_pulls_page_rejects_non_arrays_and_types_oversize() {
        let (base, server) = serve_once(200, "", r#"{"message":"odd"}"#.into());
        let api = Api::new(&base, &base).unwrap();
        let error = api.closed_pulls_page("t", "o/r", 10, 1).await.err();
        assert!(matches!(error, Some(GhError::Failed(_))), "{error:?}");
        server.join().unwrap();

        let big = format!("[\"{}\"]", "x".repeat(API_BODY_BYTES));
        let (base, server) = serve_once(200, "", big);
        let api = Api::new(&base, &base).unwrap();
        let error = api.closed_pulls_page("t", "o/r", 100, 1).await.err();
        assert!(matches!(error, Some(GhError::TooLarge(_))), "{error:?}");
        server.join().unwrap();
    }

    #[test]
    fn signatures_need_the_exact_mac() {
        let mac: String = hmac_sha256(b"s", b"body")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(webhook_signature_ok("s", b"body", &format!("sha256={mac}")));
        for bad in [
            format!("sha256={}", mac.to_uppercase().replace('A', "0")),
            format!("sha1={mac}"),
            format!("sha256={}", &mac[..62]),
            format!("sha256={mac}00"),
            "sha256=".to_owned(),
            String::new(),
            "sha256=é".to_owned(),
        ] {
            assert!(!webhook_signature_ok("s", b"body", &bad), "{bad}");
        }
        assert!(!webhook_signature_ok(
            "t",
            b"body",
            &format!("sha256={mac}")
        ));
        assert!(!webhook_signature_ok(
            "s",
            b"bodY",
            &format!("sha256={mac}")
        ));
    }

    #[test]
    fn timestamps_parse() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-10-16T12:30:05Z"), Some(1_792_153_805));
        assert_eq!(
            parse_rfc3339("2026-10-16T12:30:05+00:00"),
            Some(1_792_153_805)
        );
        assert_eq!(parse_rfc3339("2000-02-29T00:00:00Z"), Some(951_782_400));
        for bad in [
            "",
            "2026-13-01T00:00:00Z",
            "2026-10-16",
            "2026-10-16T25:00:00Z",
            "x",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn link_headers_and_paths() {
        let link = "<https://x/repos/a/b/pulls?state=open&per_page=100&page=2>; rel=\"next\", \
                    <https://x/repos/a/b/pulls?state=open&per_page=100&page=7>; rel=\"last\"";
        assert_eq!(last_page(link), Some(7));
        assert_eq!(last_page("<https://x?page=2>; rel=\"next\""), None);
        assert_eq!(encode_path("feat/a b#1%"), "feat/a%20b%231%25");
        assert_eq!(encode_path("v1.2/%2e%2e"), "v1%2E2/%252e%252e");
        assert_eq!(
            endpoint_kind("http://h/repos/a/b/pulls?x=1", "http://h"),
            "repos"
        );
        assert_eq!(endpoint_kind("http://h/graphql", "http://h"), "graphql");
    }

    #[test]
    fn branch_names_must_look_like_git_refs() {
        for good in [
            "main",
            "feat/x",
            "release/v1.2",
            "a-b_c",
            "x@y",
            "%2e%2e",
            "a/%2F",
        ] {
            assert!(valid_ref_name(good), "{good}");
        }
        for bad in [
            "",
            "/a",
            "a/",
            "a//b",
            "../../x",
            "..",
            ".",
            "a/./b",
            "a/../b",
            "a..b",
            "a@{1}",
            "a.lock",
            "a b",
            "a~",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[",
            "a\\b",
            "a\nb",
            "a\0b",
            &"x".repeat(256),
        ] {
            assert!(!valid_ref_name(bad), "{bad:?}");
        }
    }

    fn full(status: u16, headers: &[(&str, &str)]) -> Full {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        Full {
            status,
            headers: map,
            body: Value::Null,
        }
    }

    #[test]
    fn failures_are_classified_by_status_and_rate_limit_headers() {
        let now = unix_now();
        assert!(
            matches!(classify_with("repos", full(401, &[]), true), Err(GhError::Auth(d)) if d == "401 from repos")
        );
        assert!(matches!(
            classify_with("repos", full(403, &[]), true),
            Err(GhError::Auth(_))
        ));
        assert!(matches!(classify_with("repos", full(404, &[]), true), Ok(r) if r.status == 404));
        // A write refused with 403 is the change being refused, not a bad credential.
        assert!(matches!(classify_with("git", full(403, &[]), false), Ok(r) if r.status == 403));
        assert!(matches!(
            classify_with("git", full(401, &[]), false),
            Err(GhError::Auth(_))
        ));
        assert!(matches!(classify_with("repos", full(304, &[]), true), Ok(r) if r.status == 304));
        assert!(matches!(
            classify_with("repos", full(502, &[]), true),
            Err(GhError::Failed(_))
        ));
        // 403 with quota left is a permission problem on one resource, not a bad credential.
        assert!(matches!(
            classify_with("repos", full(403, &[("x-ratelimit-remaining", "10")]), true),
            Ok(r) if r.status == 403
        ));
        let Err(GhError::Limited(until)) =
            classify_with("repos", full(429, &[("retry-after", "600")]), true)
        else {
            panic!("429 is a rate limit");
        };
        assert!((now + 599..=now + 605).contains(&until));
        let Err(GhError::Limited(until)) = classify_with(
            "repos",
            full(
                403,
                &[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", &(now + 5000).to_string()),
                ],
            ),
            true,
        ) else {
            panic!("403 with no quota left is a rate limit");
        };
        assert_eq!(until, now + 5000);
        let Err(GhError::Limited(until)) = classify_with("repos", full(429, &[]), true) else {
            panic!()
        };
        assert!(until >= now + MIN_BACKOFF_SECONDS);
    }
}
