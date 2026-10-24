//! GitLab REST v4 client for hub sync. It offers the operations `github::Api`
//! does, with the same results, and rewrites merge requests into the pull
//! request JSON the sync already reads. The credential is an argument, never
//! stored here, and never appears in an error.
use std::collections::HashMap;

use reqwest::Method;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
use serde_json::{Value, json};

use super::Result;
use super::forge_http::{self, Full, send_full};
use super::github::{
    API_BODY_BYTES, Branch, CommitStatus, Gh, GhError, MAX_COMMENT_PAGES, MAX_FILE_PAGES,
    MAX_PULL_PAGES, MAX_RECENT_BRANCHES, PER_PAGE, Paged, PullHead, Reply, RepoInfo, StatusState,
    endpoint_kind, parse_rfc3339, valid_ref_name,
};
use super::store::{unix_now, validate_repository};

/// A rate limit never moves the next call less than this far out.
const MIN_BACKOFF_SECONDS: i64 = 60;
const MAX_BACKOFF_SECONDS: i64 = 24 * 60 * 60;
/// Pages of branches read before giving up; the 200-branch cap ends the walk sooner.
const MAX_BRANCH_PAGES: u32 = 10;
/// Pages of the head commit's root listing (1,000 entries); more is undetermined, never guessed.
pub const MAX_TREE_PAGES: u32 = 5;
/// Pages of one commit's statuses of a single name.
pub const MAX_STATUS_PAGES: u32 = 2;

/// The newest status of one name on a commit, as GitLab reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedStatus {
    pub id: i64,
    /// GitLab's own state: pending, running, success, failed, canceled, skipped, manual.
    pub state: String,
    pub description: String,
}

/// GitLab's spelling of a provider-neutral status state.
pub fn status_state(state: StatusState) -> &'static str {
    match state {
        StatusState::Pending => "pending",
        StatusState::Success => "success",
        StatusState::Failure | StatusState::Error => "failed",
    }
}

pub struct Api {
    client: reqwest::Client,
    base: String,
}

impl Api {
    /// `api` is the `…/api/v4` URL.
    pub fn new(api: &str) -> Result<Self> {
        Ok(Self {
            client: forge_http::client()?,
            base: api.to_owned(),
        })
    }

    async fn call(&self, token: &str, method: Method, url: &str, body: Option<Value>) -> Gh<Reply> {
        let mut request = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, format!("Bearer {token}"));
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        classify(
            endpoint_kind(url, &self.base),
            send_full(request, API_BODY_BYTES).await?,
        )
    }

    async fn get(&self, token: &str, path: &str) -> Gh<Reply> {
        let url = format!("{}{path}", self.base);
        self.call(token, Method::GET, &url, None).await
    }

    /// Up to `max_pages` pages of an array. GitLab has no conditional list, so never a 304.
    async fn paged(&self, token: &str, path: &str, max_pages: u32) -> Gh<Paged> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut items = Vec::new();
        for page in 1.. {
            let url = format!("{}{path}{sep}per_page={PER_PAGE}&page={page}", self.base);
            let reply = self.call(token, Method::GET, &url, None).await?;
            let (next, last) = (reply.next, reply.last_page);
            let body = reply.ok(endpoint_kind(&url, &self.base))?;
            let Some(page_items) = body.as_array() else {
                return Err(GhError::Failed(format!(
                    "unreadable reply from {}",
                    endpoint_kind(&url, &self.base)
                )));
            };
            items.extend(page_items.iter().cloned());
            if !next || page == max_pages {
                let unread = if next {
                    last.map_or(1, |last| last.saturating_sub(max_pages).max(1))
                } else {
                    0
                };
                return Ok(Paged {
                    items,
                    etag: None,
                    pages: page,
                    unread,
                });
            }
        }
        unreachable!("the page loop returns")
    }

    /// By path (`group/sub/name`); None when it is missing or unreadable (404).
    pub async fn repo(&self, token: &str, full_name: &str) -> Gh<Option<RepoInfo>> {
        let reply = self
            .get(token, &format!("/projects/{}", encode(full_name)))
            .await?;
        if matches!(reply.status, 403 | 404) {
            return Ok(None);
        }
        project_info(&reply.ok("projects")?)
            .map(Some)
            .ok_or_else(|| GhError::Failed("unreadable reply from projects".into()))
    }

    /// The user id and username the token acts as.
    pub async fn user(&self, token: &str) -> Gh<(i64, String)> {
        let body = self.get(token, "/user").await?.ok("user")?;
        match (body["id"].as_i64(), body["username"].as_str()) {
            (Some(id), Some(login)) if id > 0 => Ok((id, login.to_owned())),
            _ => Err(GhError::Failed("unreadable reply from user".into())),
        }
    }

    /// Open merge requests, at most [`MAX_PULL_PAGES`] pages.
    pub async fn open_pulls(&self, token: &str, project: i64) -> Gh<Paged> {
        let path = format!("/projects/{project}/merge_requests?state=opened");
        let mut paged = self.paged(token, &path, MAX_PULL_PAGES).await?;
        paged.items = paged.items.iter().map(pull_json).collect();
        Ok(paged)
    }

    pub async fn pull(&self, token: &str, project: i64, iid: i64) -> Gh<Option<Value>> {
        let reply = self
            .get(token, &format!("/projects/{project}/merge_requests/{iid}"))
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        reply.ok("projects").map(|mr| Some(pull_json(&mr)))
    }

    /// Changed paths (renames count both names), and whether more files exist. Needs GitLab 15.7.
    pub async fn pull_files(&self, token: &str, project: i64, iid: i64) -> Gh<(Vec<String>, bool)> {
        let path = format!("/projects/{project}/merge_requests/{iid}/diffs");
        let paged = self.paged(token, &path, MAX_FILE_PAGES).await?;
        let mut files: Vec<String> = paged
            .items
            .iter()
            .flat_map(|file| ["new_path", "old_path"].map(|k| file[k].as_str()))
            .flatten()
            .map(str::to_owned)
            .collect();
        files.sort();
        files.dedup();
        Ok((files, paged.unread > 0))
    }

    /// None when the branch is missing, or the name is not a plausible git ref (never sent).
    pub async fn branch(&self, token: &str, project: i64, name: &str) -> Gh<Option<Branch>> {
        if !valid_ref_name(name) {
            return Ok(None);
        }
        let path = format!("/projects/{project}/repository/branches/{}", encode(name));
        let reply = self.get(token, &path).await?;
        if reply.status == 404 {
            return Ok(None);
        }
        let body = reply.ok("projects")?;
        // A reply for some other branch is not an answer.
        if body["name"].as_str() != Some(name) {
            return Ok(None);
        }
        branch_of(&body)
            .map(Some)
            .ok_or_else(|| GhError::Failed("unreadable reply from projects".into()))
    }

    /// The project's explicit default branch; absence is unsupported, as on GitHub.
    pub async fn default_branch(&self, token: &str, project: i64) -> Gh<String> {
        let body = self
            .get(token, &format!("/projects/{project}"))
            .await?
            .ok("projects")?;
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

    /// A JSON file of at most `max` bytes at `branch`, re-serialized; None when it does not
    /// exist. Raw files that are not JSON are an error (the reply reader only keeps JSON).
    pub async fn file(
        &self,
        token: &str,
        project: i64,
        path: &str,
        branch: &str,
        max: usize,
    ) -> Gh<Option<Vec<u8>>> {
        if !valid_ref_name(branch) {
            return Err(GhError::Failed("bad branch name".into()));
        }
        let url = format!(
            "/projects/{project}/repository/files/{}/raw?ref={}",
            encode(path),
            encode(branch)
        );
        let reply = self.get(token, &url).await?;
        if reply.status == 404 {
            return Ok(None);
        }
        let body = reply.ok("projects")?;
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        if body.is_null() || bytes.len() > max {
            return Err(GhError::Failed("unreadable or oversized file".into()));
        }
        Ok(Some(bytes))
    }

    /// The commit a branch points at; None when it does not exist. A bad name is an error.
    pub async fn ref_sha(&self, token: &str, project: i64, name: &str) -> Gh<Option<String>> {
        if !valid_ref_name(name) {
            return Err(GhError::Failed("bad branch name".into()));
        }
        Ok(self.branch(token, project, name).await?.map(|b| b.sha))
    }

    /// Open merge requests from `source` into `target` within the project (fork merge requests
    /// are left out). Past the page cap the answer is unknown, so it is an error rather than a
    /// partial list: the caller must not create a duplicate.
    pub async fn find_open_pulls(
        &self,
        token: &str,
        project: i64,
        source: &str,
        target: &str,
    ) -> Gh<Vec<Value>> {
        let path = format!(
            "/projects/{project}/merge_requests?state=opened&source_branch={}&target_branch={}",
            encode(source),
            encode(target)
        );
        let paged = self.paged(token, &path, MAX_PULL_PAGES).await?;
        if paged.unread > 0 {
            return Err(GhError::Failed(
                "too many open merge requests from that branch to search".into(),
            ));
        }
        let mut found = Vec::new();
        for mr in &paged.items {
            // An item that cannot be read could be the merge request being looked for.
            if mr["iid"].as_i64().is_none()
                || mr["id"].as_i64().is_none()
                || mr["source_project_id"].as_i64().is_none()
                || mr["target_project_id"].as_i64().is_none()
            {
                return Err(GhError::Failed(
                    "unreadable reply from merge_requests".into(),
                ));
            }
            if in_project(mr, project) {
                found.push(pull_json(mr));
            }
        }
        Ok(found)
    }

    /// Opens a merge request within the project. `Err(status)` is a definitive refusal (400, 403,
    /// 404, 422); anything else that is not a readable 201, including GitLab's 409 for an open
    /// merge request that already exists, leaves the outcome uncertain.
    pub async fn create_candidate_pull(
        &self,
        token: &str,
        project: i64,
        title: &str,
        body: &str,
        source: &str,
        target: &str,
    ) -> Gh<std::result::Result<Value, u16>> {
        let url = format!("{}/projects/{project}/merge_requests", self.base);
        let request = json!({"source_branch": source, "target_branch": target,
            "title": title, "description": body});
        let reply = self.call(token, Method::POST, &url, Some(request)).await?;
        if matches!(reply.status, 400 | 403 | 404 | 422) {
            return Ok(Err(reply.status));
        }
        if reply.status != 201 {
            return Err(GhError::Failed(format!("{} from projects", reply.status)));
        }
        let mr = reply.body;
        if mr["iid"].as_i64().is_none()
            || mr["id"].as_i64().is_none()
            || mr["web_url"].as_str().is_none()
        {
            return Err(GhError::Failed("unreadable reply from projects".into()));
        }
        Ok(Ok(pull_json(&mr)))
    }

    /// Everything a forge check needs from one merge request, as `github::Api::pull_head` does.
    /// None when the merge request does not exist. A `locked` merge request (being merged) is an
    /// `Err(Failed)`: its state is undetermined. A closed or merged one carries only the head sha
    /// and merge commit. The head repository is `source_project_id` (0 when null, a deleted fork).
    /// The root tree OID is rebuilt from a bounded listing of the top-level tree because GitLab's
    /// commit API names none; a listing past [`MAX_TREE_PAGES`] or with a malformed entry is an
    /// error, never a guessed tree. ponytail: head commit parts are not cached (GitHub caches 256).
    pub async fn pull_head(
        &self,
        token: &str,
        project: i64,
        iid: i64,
        base_tips: &mut HashMap<String, String>,
    ) -> Gh<Option<PullHead>> {
        let bad = || GhError::Failed("unreadable reply from merge_requests".into());
        let reply = self
            .get(token, &format!("/projects/{project}/merge_requests/{iid}"))
            .await?;
        if reply.status == 404 {
            return Ok(None);
        }
        let mr = reply.ok("merge_requests")?;
        if mr["iid"].as_i64() != Some(iid) {
            return Err(bad());
        }
        let (open, merged) = match mr["state"].as_str() {
            Some("opened") => (true, false),
            Some("closed") => (false, false),
            Some("merged") => (false, true),
            Some("locked") => {
                return Err(GhError::Failed(
                    "merge request is locked; its state is undetermined".into(),
                ));
            }
            _ => return Err(bad()),
        };
        let head_sha = sha40(&mr["sha"]).ok_or_else(bad)?;
        if !open {
            let merge_commit = if merged {
                [&mr["merge_commit_sha"], &mr["squash_commit_sha"]]
                    .into_iter()
                    .find_map(sha40)
            } else {
                None
            };
            return Ok(Some(PullHead {
                open,
                merged,
                merge_commit,
                head_sha,
                head_repo_id: 0,
                base_repo_id: 0,
                base_ref: String::new(),
                base_tip: String::new(),
                head_tree: String::new(),
                head_parents: Vec::new(),
            }));
        }
        let head_repo_id = match &mr["source_project_id"] {
            Value::Null => 0,
            id => id.as_u64().filter(|id| *id > 0).ok_or_else(bad)?,
        };
        let base_repo_id = mr["target_project_id"]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(bad)?;
        let base_ref = mr["target_branch"]
            .as_str()
            .filter(|name| valid_ref_name(name))
            .ok_or_else(bad)?
            .to_owned();
        let source_branch = mr["source_branch"]
            .as_str()
            .filter(|name| valid_ref_name(name))
            .ok_or_else(bad)?;
        // A fork's merge request is not this project's candidate: its branch, commit and tree are
        // not in this project, so none of them are read.
        if head_repo_id != project as u64 {
            return Ok(Some(PullHead {
                open,
                merged,
                merge_commit: None,
                head_sha,
                head_repo_id,
                base_repo_id,
                base_ref: String::new(),
                base_tip: String::new(),
                head_tree: String::new(),
                head_parents: Vec::new(),
            }));
        }
        // The merge request's `sha` can lag a new push. A head that is not the source branch's tip
        // is superseded, so nothing may be judged on it.
        if self
            .ref_sha(token, project, source_branch)
            .await?
            .as_deref()
            != Some(head_sha.as_str())
        {
            return Err(GhError::Failed(
                "merge request head lags its source branch".into(),
            ));
        }
        let base_tip = match base_tips.get(&base_ref) {
            Some(tip) => tip.clone(),
            None => {
                let tip = self
                    .ref_sha(token, project, &base_ref)
                    .await?
                    .ok_or_else(|| GhError::Failed("base branch not found".into()))?;
                base_tips.insert(base_ref.clone(), tip.clone());
                tip
            }
        };
        let commit = self
            .get(
                token,
                &format!("/projects/{project}/repository/commits/{head_sha}"),
            )
            .await?;
        if commit.status == 404 {
            return Err(GhError::Failed("head commit not found".into()));
        }
        let commit = commit.ok("commits")?;
        let head_parents = (sha40(&commit["id"]).as_deref() == Some(head_sha.as_str()))
            .then(|| commit["parent_ids"].as_array())
            .flatten()
            .and_then(|ids| ids.iter().map(sha40).collect::<Option<Vec<_>>>())
            .ok_or_else(|| GhError::Failed("unreadable reply from commits".into()))?;
        let path = format!("/projects/{project}/repository/tree?ref={head_sha}");
        let listing = self.paged(token, &path, MAX_TREE_PAGES).await?;
        if listing.unread > 0 {
            return Err(GhError::Failed("head tree listing is too large".into()));
        }
        let head_tree = tree_oid(&listing.items)
            .ok_or_else(|| GhError::Failed("unreadable reply from tree".into()))?;
        Ok(Some(PullHead {
            open,
            merged,
            merge_commit: None,
            head_sha,
            head_repo_id,
            base_repo_id,
            base_ref,
            base_tip,
            head_tree,
            head_parents,
        }))
    }

    /// Posts a commit status on the commit with `ref_name` (the merge request's source branch).
    /// `Ok(Err(status))`: 403 or 404 is a definite refusal. 400 is GitLab's "cannot transition",
    /// which it returns only for a state posted onto a status that is still pending or running
    /// in that state; the caller decides from the desired state whether to resolve it with
    /// [`commit_statuses`](Self::commit_statuses). `Limited` and
    /// `Auth` mean it was not applied; any other error (5xx, no reply, a reply that is not this
    /// status in the sent state) is uncertain.
    pub async fn post_commit_status(
        &self,
        token: &str,
        project: i64,
        status: &CommitStatus,
        ref_name: &str,
    ) -> Gh<std::result::Result<(), u16>> {
        if !valid_ref_name(ref_name) {
            return Err(GhError::Failed("bad branch name".into()));
        }
        let url = format!("{}/projects/{project}/statuses/{}", self.base, status.sha());
        let state = status_state(status.state());
        let body = json!({"state": state, "name": status.context(),
            "description": status.description(), "ref": ref_name});
        let reply = self.call(token, Method::POST, &url, Some(body)).await?;
        if matches!(reply.status, 400 | 403 | 404) {
            return Ok(Err(reply.status));
        }
        if !matches!(reply.status, 200 | 201)
            || reply.body["status"] != state
            || reply.body["name"] != status.context()
        {
            return Err(GhError::Failed(format!("{} from statuses", reply.status)));
        }
        Ok(Ok(()))
    }

    /// The newest (highest id) status named `name` on the commit for `ref_name`, from at most
    /// [`MAX_STATUS_PAGES`] pages; None when there is none. Past the cap, or with an item that
    /// cannot be read, it is an error: the answer would be a guess.
    pub async fn commit_statuses(
        &self,
        token: &str,
        project: i64,
        sha: &str,
        name: &str,
        ref_name: &str,
    ) -> Gh<Option<ListedStatus>> {
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(GhError::Failed("bad commit sha".into()));
        }
        if !valid_ref_name(ref_name) {
            return Err(GhError::Failed("bad branch name".into()));
        }
        let path = format!(
            "/projects/{project}/repository/commits/{}/statuses?name={}&ref={}",
            sha.to_ascii_lowercase(),
            encode(name),
            encode(ref_name)
        );
        let paged = self.paged(token, &path, MAX_STATUS_PAGES).await?;
        if paged.unread > 0 {
            return Err(GhError::Failed("too many statuses to read".into()));
        }
        let mut newest: Option<ListedStatus> = None;
        for item in &paged.items {
            let (Some(id), Some(state), Some(item_name)) = (
                item["id"].as_i64(),
                item["status"].as_str(),
                item["name"].as_str(),
            ) else {
                return Err(GhError::Failed("unreadable reply from statuses".into()));
            };
            // The server filtered by name and ref; a reply for another name is not an answer.
            if item_name != name || item["ref"].as_str().is_some_and(|r| r != ref_name) {
                continue;
            }
            if newest.as_ref().is_none_or(|n| id > n.id) {
                newest = Some(ListedStatus {
                    id,
                    state: state.to_owned(),
                    description: item["description"].as_str().unwrap_or_default().to_owned(),
                });
            }
        }
        Ok(newest)
    }

    /// Branches whose tip commit is at or after `since` (unix seconds), newest first, at most
    /// [`MAX_RECENT_BRANCHES`]; true when that cap cut the list short.
    pub async fn recent_branches(
        &self,
        token: &str,
        project: i64,
        since: i64,
    ) -> Gh<(Vec<Branch>, bool)> {
        let mut found = Vec::new();
        for page in 1..=MAX_BRANCH_PAGES {
            let path = format!(
                "/projects/{project}/repository/branches?sort=updated_desc&per_page={PER_PAGE}&page={page}"
            );
            let reply = self.get(token, &path).await?;
            let next = reply.next;
            let body = reply.ok("projects")?;
            for node in body.as_array().into_iter().flatten() {
                let Some(branch) = branch_of(node) else {
                    continue;
                };
                // Newest first, so the first old branch ends the walk.
                // Skipped, not an end of the walk: stopping here would drop every later branch.
                let Some(at) = time(&branch.committed_at) else {
                    eprintln!("hub: gitlab branch with an unreadable commit date; skipped");
                    continue;
                };
                if at < since {
                    return Ok((found, false));
                }
                if found.len() >= MAX_RECENT_BRANCHES {
                    return Ok((found, true));
                }
                found.push(branch);
            }
            if !next {
                break;
            }
        }
        Ok((found, false))
    }

    /// `(note id, author username, body)` of the first pages of notes, oldest first.
    pub async fn comments(
        &self,
        token: &str,
        project: i64,
        iid: i64,
    ) -> Gh<Vec<(i64, String, String)>> {
        let path = format!("/projects/{project}/merge_requests/{iid}/notes?sort=asc");
        let paged = self.paged(token, &path, MAX_COMMENT_PAGES).await?;
        Ok(paged
            .items
            .iter()
            .filter_map(|n| {
                let login = n["author"]["username"].as_str().unwrap_or_default();
                Some((
                    n["id"].as_i64()?,
                    login.to_owned(),
                    n["body"].as_str()?.to_owned(),
                ))
            })
            .collect())
    }

    pub async fn create_comment(&self, token: &str, project: i64, iid: i64, body: &str) -> Gh<i64> {
        let url = format!(
            "{}/projects/{project}/merge_requests/{iid}/notes",
            self.base
        );
        let reply = self
            .call(token, Method::POST, &url, Some(json!({"body": body})))
            .await?;
        reply.ok("projects")?["id"]
            .as_i64()
            .ok_or_else(|| GhError::Failed("unreadable reply from projects".into()))
    }

    /// False when the note no longer exists.
    pub async fn edit_comment(
        &self,
        token: &str,
        project: i64,
        iid: i64,
        note: i64,
        body: &str,
    ) -> Gh<bool> {
        let url = format!(
            "{}/projects/{project}/merge_requests/{iid}/notes/{note}",
            self.base
        );
        let reply = self
            .call(token, Method::PUT, &url, Some(json!({"body": body})))
            .await?;
        if reply.status == 404 {
            return Ok(false);
        }
        reply.ok("projects").map(|_| true)
    }

    pub async fn create_status(
        &self,
        token: &str,
        project: i64,
        sha: &str,
        description: &str,
    ) -> Gh<()> {
        let url = format!("{}/projects/{project}/statuses/{sha}", self.base);
        let body = json!({"state": "success", "name": "agentscm/coordination",
            "description": description});
        self.call(token, Method::POST, &url, Some(body))
            .await?
            .ok("projects")
            .map(drop)
    }
}

fn project_info(item: &Value) -> Option<RepoInfo> {
    let id = item["id"].as_i64().filter(|id| *id > 0)?;
    let full_name = item["path_with_namespace"].as_str()?;
    validate_repository(full_name).ok()?;
    Some(RepoInfo {
        repository_id: id,
        full_name: full_name.to_owned(),
        node_id: Some(format!("gid://gitlab/Project/{id}")),
    })
}

/// A merge request as the pull request JSON the sync reads.
fn pull_json(mr: &Value) -> Value {
    let merged = mr["state"] == "merged";
    let open = matches!(mr["state"].as_str(), Some("opened" | "locked"));
    json!({"number": mr["iid"], "node_id": format!("gid://gitlab/MergeRequest/{}", mr["id"]),
        "title": mr["title"], "state": if open { "open" } else { "closed" },
        "draft": mr["draft"].as_bool().or(mr["work_in_progress"].as_bool()).unwrap_or(false),
        "merged": merged, "body": mr["description"],
        "source_project_id": mr["source_project_id"], "target_project_id": mr["target_project_id"],
        "merge_commit_sha": if mr["merge_commit_sha"].is_null() { &mr["squash_commit_sha"] } else { &mr["merge_commit_sha"] },
        "head": {"ref": mr["source_branch"], "sha": mr["sha"]},
        "base": {"ref": mr["target_branch"]},
        "user": {"login": mr["author"]["username"], "id": mr["author"]["id"]},
        "html_url": mr["web_url"]})
}

/// A lowercase 40-hex object id, or None.
fn sha40(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

/// The Git tree OID of a top-level listing (`id`, `name`, `type`, `mode` per entry). None for
/// anything Git could not hold: a bad id, name, mode or type, a mode that disagrees with the type,
/// or a duplicate name.
fn tree_oid(items: &[Value]) -> Option<String> {
    let mut entries = Vec::new();
    for item in items {
        let (name, kind) = (item["name"].as_str()?, item["type"].as_str()?);
        let mode = match (kind, item["mode"].as_str()?) {
            ("tree", "040000" | "40000") => "40000",
            ("blob", mode @ ("100644" | "100755" | "120000")) => mode,
            ("commit", "160000") => "160000",
            _ => return None,
        };
        if name.is_empty() || name.contains(['/', '\0']) || matches!(name, "." | "..") {
            return None;
        }
        let id = sha40(&item["id"])?;
        // Git sorts a directory as if its name ended in '/'.
        let key = if kind == "tree" {
            format!("{name}/")
        } else {
            name.to_owned()
        };
        entries.push((key, mode, name, id));
    }
    entries.sort();
    if entries.windows(2).any(|pair| pair[0].2 == pair[1].2) {
        return None;
    }
    let mut body = Vec::new();
    for (_, mode, name, id) in &entries {
        body.extend_from_slice(format!("{mode} {name}\0").as_bytes());
        for pair in id.as_bytes().chunks(2) {
            body.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
        }
    }
    let mut hash = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    hash.update(format!("tree {}\0", body.len()).as_bytes());
    hash.update(&body);
    Some(
        hash.finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// Whether the merge request is from the project into itself.
fn in_project(mr: &Value, project: i64) -> bool {
    mr["source_project_id"].as_i64() == Some(project)
        && mr["target_project_id"].as_i64() == Some(project)
}

fn branch_of(item: &Value) -> Option<Branch> {
    Some(Branch {
        name: item["name"].as_str()?.to_owned(),
        sha: item["commit"]["id"].as_str()?.to_owned(),
        committed_at: item["commit"]["committed_date"].as_str()?.to_owned(),
    })
}

/// Unix seconds of an ISO 8601 time; GitLab writes the committer's own UTC offset.
fn time(text: &str) -> Option<i64> {
    // Slicing below is by byte; ASCII keeps it on character boundaries.
    if !text.is_ascii() {
        return None;
    }
    if text.ends_with('Z') {
        return parse_rfc3339(text);
    }
    let at = text.len().checked_sub(6)?;
    let (local, offset) = text.split_at(at);
    let sign = match offset.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    // `+HH:MM`; anything else (signs, one digit) is unreadable.
    let digits = offset[1..].replacen(':', "", 1);
    if digits.len() != 4
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || offset.as_bytes()[3] != b':'
    {
        return None;
    }
    let shift =
        sign * (digits[..2].parse::<i64>().ok()? * 3600 + digits[2..].parse::<i64>().ok()? * 60);
    Some(parse_rfc3339(&format!("{local}Z"))? - shift)
}

/// Percent-encodes everything but letters, digits and `-_~`, so a `/` stays inside one path segment.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn classify(kind: &str, full: Full) -> Gh<Reply> {
    let header = |name: &str| full.headers.get(name).and_then(|v| v.to_str().ok());
    let number = |name: &str| header(name).and_then(|v| v.trim().parse::<i64>().ok());
    if full.status == 429 {
        let (retry, reset) = (number("retry-after"), number("ratelimit-reset"));
        let now = unix_now();
        let until = [
            Some(now + MIN_BACKOFF_SECONDS),
            retry.map(|s| now.saturating_add(s)),
            reset,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(now + MIN_BACKOFF_SECONDS);
        // A forge cannot park the connection for longer than a day.
        return Err(GhError::Limited(until.min(now + MAX_BACKOFF_SECONDS)));
    }
    // Only 401 says the credential is bad; a 403 is one project saying no (see `Reply::ok`).
    if full.status == 401 {
        return Err(GhError::Auth(format!("401 from {kind}")));
    }
    if full.status >= 500 {
        return Err(GhError::Failed(format!("{} from {kind}", full.status)));
    }
    Ok(Reply {
        status: full.status,
        etag: None,
        next: header("x-next-page").is_some_and(|page| !page.trim().is_empty()),
        last_page: number("x-total-pages").and_then(|n| u32::try_from(n).ok()),
        body: full.body,
    })
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderName};

    use super::*;

    fn full(status: u16, headers: &[(&str, &str)]) -> Full {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
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
    fn merge_requests_read_like_pull_requests() {
        let mr = json!({"id": 77, "iid": 3, "title": "t", "state": "merged", "draft": true,
            "squash_commit_sha": "abc", "merge_commit_sha": null, "sha": "head",
            "source_branch": "feat/x", "target_branch": "main",
            "author": {"username": "alice", "id": 5}, "web_url": "https://g/x/-/merge_requests/3"});
        let pull = pull_json(&mr);
        assert_eq!(pull["number"], 3);
        assert_eq!(pull["node_id"], "gid://gitlab/MergeRequest/77");
        assert_eq!(
            (&pull["state"], &pull["merged"]),
            (&json!("closed"), &json!(true))
        );
        assert_eq!(pull["merge_commit_sha"], "abc");
        assert_eq!(pull["head"], json!({"ref": "feat/x", "sha": "head"}));
        assert_eq!(pull["user"], json!({"login": "alice", "id": 5}));
        assert_eq!(pull_json(&json!({"state": "opened"}))["state"], "open");
    }

    #[test]
    fn merge_requests_carry_body_and_projects() {
        let mr = json!({"iid": 3, "description": "text\n\n<!-- m -->",
            "source_project_id": 1, "target_project_id": 2});
        let pull = pull_json(&mr);
        assert_eq!(pull["body"], "text\n\n<!-- m -->");
        assert_eq!(
            (&pull["source_project_id"], &pull["target_project_id"]),
            (&json!(1), &json!(2))
        );
    }

    #[test]
    fn only_same_project_merge_requests_count() {
        let mr = |s: Value, t: Value| json!({"source_project_id": s, "target_project_id": t});
        assert!(in_project(&mr(json!(1), json!(1)), 1));
        assert!(!in_project(&mr(json!(9), json!(1)), 1));
        assert!(!in_project(&mr(json!(1), json!(9)), 1));
        assert!(!in_project(&json!({}), 1));
    }

    #[test]
    fn commit_times_honour_their_offset() {
        assert_eq!(time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(time("1970-01-01T01:00:00.000+01:00"), Some(0));
        assert_eq!(time("1969-12-31T19:00:00-05:00"), Some(0));
        assert_eq!(time("1970-01-01T00:00:00"), None);
        assert_eq!(time("x"), None);
        assert_eq!(time("2026-10-16T12:30:05é+01:00"), None);
        assert_eq!(time("é"), None);
        assert_eq!(time("1970-01-01T00:00:00+-5:00"), None);
        assert_eq!(time("1970-01-01T00:00:00+1:+00"), None);
    }

    #[test]
    fn names_stay_in_one_path_segment() {
        assert_eq!(encode("group/sub/name"), "group%2Fsub%2Fname");
        assert_eq!(encode("a/%2F.."), "a%2F%252F%2E%2E");
    }

    #[test]
    fn failures_are_classified_by_status() {
        let now = unix_now();
        assert!(matches!(
            classify("projects", full(401, &[])),
            Err(GhError::Auth(_))
        ));
        // A 403 is not the credential failing; `ok` turns it into a plain failure.
        let forbidden = classify("projects", full(403, &[])).unwrap();
        assert!(
            matches!(forbidden.ok("projects"), Err(GhError::Failed(d)) if d == "403 from projects")
        );
        assert!(matches!(classify("projects", full(404, &[])), Ok(r) if r.status == 404));
        assert!(matches!(
            classify("projects", full(502, &[])),
            Err(GhError::Failed(_))
        ));
        let Err(GhError::Limited(until)) =
            classify("projects", full(429, &[("retry-after", "600")]))
        else {
            panic!("429 is a rate limit");
        };
        assert!((now + 599..=now + 605).contains(&until));
        let reset = (now + 5000).to_string();
        let Err(GhError::Limited(until)) =
            classify("projects", full(429, &[("ratelimit-reset", &reset)]))
        else {
            panic!("429 is a rate limit");
        };
        assert_eq!(until, now + 5000);
        let page = classify(
            "projects",
            full(200, &[("x-next-page", "2"), ("x-total-pages", "7")]),
        )
        .unwrap();
        assert!(page.next && page.last_page == Some(7));
        assert!(
            !classify("projects", full(200, &[("x-next-page", "")]))
                .unwrap()
                .next
        );
        let huge = i64::MAX.to_string();
        let Err(GhError::Limited(until)) =
            classify("projects", full(429, &[("retry-after", &huge)]))
        else {
            panic!("429 is a rate limit");
        };
        assert!((now + 86_399..=now + 86_405).contains(&until));
    }
}
