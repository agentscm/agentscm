//! Publishing a native revision as a forge branch and pull request. The hub has no
//! native data and forge credentials never leave it, so the client sends a manifest
//! (commit metadata plus the changed files' hashes), uploads the blobs the hub lacks,
//! and asks the hub to run the forge steps. The steps never force: a branch is created
//! only if absent, and the commit the forge builds must have exactly the SHA the
//! client's mirror holds, which proves the same bytes and metadata.
//!
//! GitLab merge requests are not created; see docs/hub.md.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path};
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::connection::{ConnectedRepository, Connection};
use super::github::{Gh, GhError, parse_rfc3339, valid_ref_name};
use super::http::{ApiError, Authenticated, JsonBody, blocking};
use super::intent::scope;
use super::store::{Event, Principal, lower_hex_of, unix_now};
use super::sync::{failed, find_repository};
use super::{HubError, HubState, Result};
use crate::backend::{ObjectKey, SOURCE_PROFILE_V1};

pub const MAX_BLOB_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CHANGES: usize = 1000;
/// Publications a token may have open (no pull request yet, not rejected).
pub const MAX_UNFINISHED: i64 = 20;
/// A shown publication re-reads its pull request from the forge at most this often.
const READ_INTERVAL: Duration = Duration::from_secs(60);
/// When each publication's pull request was last read; in memory, so a restart reads once more.
static LAST_READ: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
const MAX_PATH_BYTES: usize = 1024;
const MAX_TITLE_BYTES: usize = 256;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_PERSON_BYTES: usize = 256;
/// 1000 paths of 1 KiB plus the message and body.
const MANIFEST_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 300;
/// Blob bytes live under this trust domain, keyed by their SHA-256. The key's repository
/// is the publication id (a valid identifier, unlike `owner/name`), its profile must be one
/// the backend knows, so the source profile is used for what are plain blobs.
const BLOB_DOMAIN: &str = "hub-publish";
const BLOB_INCARNATION: &str = "v1";

/// The file modes git-export and this publisher carry; "000000" marks a deletion.
const MODES: [&str; 3] = ["100644", "100755", "120000"];
const DELETED: &str = "000000";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Person {
    pub name: String,
    pub email: String,
    /// `YYYY-MM-DDTHH:MM:SSZ`.
    pub date: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub path: String,
    pub mode: String,
    /// Git blob SHA-1; `None` for a deletion.
    pub blob: Option<String>,
    /// SHA-256 of the file's bytes; `None` for a deletion.
    pub sha256: Option<String>,
    pub size: u64,
}

/// What the client asks the hub to publish. Field order is the canonical JSON order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub revision: String,
    pub event_seq: i64,
    pub commit: String,
    pub tree: String,
    pub parent: String,
    pub branch: String,
    pub base: String,
    pub title: String,
    pub body: String,
    pub author: Person,
    pub committer: Person,
    pub message: String,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPublication {
    /// `pub_` + 32 lowercase hex, derived by the client so a retry finds the same publication.
    pub id: String,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    pub id: String,
    pub repository: String,
    pub token: String,
    /// The user who owns the creating token; only they may upload and run.
    pub owner: String,
    /// The token's name and its owner's login, for the pull request footer.
    pub agent: String,
    pub owner_login: String,
    pub revision: String,
    pub event_seq: i64,
    pub commit: String,
    pub branch: String,
    pub base: String,
    /// Canonical JSON of the [`Manifest`].
    pub manifest: String,
    /// `pending`, `attempting` (a create was sent, so a branch at our commit is ours), `created`
    /// or `rejected`.
    pub branch_state: String,
    pub branch_message: Option<String>,
    pub pr_number: Option<i64>,
    pub pr_node_id: Option<String>,
    pub pr_url: Option<String>,
    pub merge_commit: Option<String>,
    /// The last run's failure, cleared by a run that gets through.
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The outcome of [`Store::create_publication`](super::Store::create_publication).
// A short-lived return value, matched at once; boxing the row would only add noise.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Create {
    /// New, or the same manifest again.
    Done(Publication),
    /// The id is held by another repository or a different manifest.
    Conflict,
    /// The token already has [`MAX_UNFINISHED`] open publications.
    TooMany,
}

impl Create {
    pub(super) fn of_existing(found: Publication, repository: &str, manifest: &str) -> Self {
        if found.repository == repository && found.manifest == manifest {
            Self::Done(found)
        } else {
            Self::Conflict
        }
    }
}

/// One forge fact to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// Recorded just before the branch create is sent.
    BranchAttempt,
    BranchCreated,
    BranchRejected {
        status: u16,
        message: String,
    },
    PullRequest {
        number: i64,
        node_id: String,
        html_url: String,
    },
    Merged {
        merge_commit: String,
    },
    Error(Option<String>),
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

fn hex_of(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn text(name: &str, value: &str, max: usize, multiline: bool) -> Result<()> {
    let bad = |c: char| c.is_control() && !(multiline && matches!(c, '\n' | '\t'));
    if value.is_empty() || value.len() > max || value.chars().any(bad) {
        return Err(HubError::Invalid(format!(
            "{name} must be 1-{max} bytes without control characters"
        )));
    }
    Ok(())
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        // GitHub refuses `.git` components in a tree.
        && path
            .split('/')
            .all(|s| !matches!(s, "" | "." | "..") && !s.eq_ignore_ascii_case(".git"))
}

impl Person {
    fn validate(&self, what: &str) -> Result<()> {
        text(&format!("{what} name"), &self.name, MAX_PERSON_BYTES, false)?;
        text(
            &format!("{what} email"),
            &self.email,
            MAX_PERSON_BYTES,
            false,
        )?;
        if self.name.contains(['<', '>']) || self.email.contains(['<', '>']) {
            return Err(HubError::Invalid(format!("{what} has a < or > in it")));
        }
        if parse_rfc3339(&self.date).is_none() {
            return Err(HubError::Invalid(format!(
                "{what} date must be YYYY-MM-DDTHH:MM:SSZ"
            )));
        }
        Ok(())
    }
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        crate::store::validate_id(&self.revision).map_err(|_| invalid("bad revision"))?;
        if self.event_seq < 1 {
            return Err(invalid("event_seq must be positive"));
        }
        for (name, sha) in [
            ("commit", &self.commit),
            ("tree", &self.tree),
            ("parent", &self.parent),
        ] {
            if !hex_of(sha, 40) {
                return Err(HubError::Invalid(format!(
                    "{name} must be 40 lowercase hex characters"
                )));
            }
        }
        for (name, value) in [("branch", &self.branch), ("base", &self.base)] {
            if !valid_ref_name(value) {
                return Err(HubError::Invalid(format!(
                    "{name} is not a valid git ref name"
                )));
            }
        }
        if self.branch == self.base {
            return Err(invalid("branch and base must differ"));
        }
        text("title", &self.title, MAX_TITLE_BYTES, false)?;
        if self.body.len() > MAX_TEXT_BYTES || self.body.contains('\0') {
            return Err(invalid("body must be at most 64 KiB without NUL"));
        }
        text("message", &self.message, MAX_TEXT_BYTES, true)?;
        self.author.validate("author")?;
        self.committer.validate("committer")?;
        if !(1..=MAX_CHANGES).contains(&self.changes.len()) {
            return Err(invalid("a publication needs 1 to 1000 changed paths"));
        }
        let mut total = 0_u64;
        let mut paths = BTreeSet::new();
        // One file hash names one git blob and one size, and the reverse.
        let mut by_sha256: BTreeMap<&str, (&str, u64)> = BTreeMap::new();
        let mut by_blob: BTreeMap<&str, &str> = BTreeMap::new();
        for change in &self.changes {
            if !valid_path(&change.path) || !paths.insert(change.path.as_str()) {
                return Err(invalid("changed paths must be unique repo-relative paths"));
            }
            let shape_ok = match (&change.blob, &change.sha256, change.mode.as_str()) {
                (None, None, DELETED) => change.size == 0,
                (Some(blob), Some(sha256), mode) => {
                    MODES.contains(&mode)
                        && hex_of(blob, 40)
                        && hex_of(sha256, 64)
                        && change.size <= MAX_BLOB_BYTES as u64
                }
                _ => false,
            };
            if !shape_ok {
                return Err(invalid(
                    "a change is a deletion (mode 000000, no blob) or a file of at most 8 MiB \
                     with mode 100644, 100755 or 120000",
                ));
            }
            if let (Some(blob), Some(sha256)) = (&change.blob, &change.sha256) {
                let first = by_sha256.entry(sha256).or_insert((blob, change.size));
                let other = by_blob.entry(blob).or_insert(sha256);
                if *first != (blob.as_str(), change.size) || *other != sha256.as_str() {
                    return Err(invalid("a file hash must map to one git blob and one size"));
                }
            }
            total += change.size;
        }
        if total > MAX_TOTAL_BYTES as u64 {
            return Err(invalid("the changed files total more than 64 MiB"));
        }
        Ok(())
    }

    /// The distinct blobs to upload: SHA-256 and size.
    fn blobs(&self) -> Vec<(&str, u64)> {
        let mut blobs: Vec<(&str, u64)> = self
            .changes
            .iter()
            .filter_map(|c| Some((c.sha256.as_deref()?, c.size)))
            .collect();
        blobs.sort_unstable();
        blobs.dedup();
        blobs
    }

    /// Canonical JSON: struct fields are in a fixed order.
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).expect("a manifest is plain data and always serialises")
    }
}

impl NewPublication {
    /// The id must be the one the client derives, so it cannot name another repository's work.
    pub fn validate(&self, repository: &str) -> Result<()> {
        self.manifest.validate()?;
        let m = &self.manifest;
        if self.id != derive_id(repository, m.event_seq as u64, &m.commit, &m.branch) {
            return Err(invalid(
                "publication id must be derived from the repository, event, commit and branch",
            ));
        }
        Ok(())
    }
}

impl Event {
    pub(super) fn publication_requested(who_repository: &str, p: &NewPublication) -> Result<Self> {
        let m = &p.manifest;
        Self::new(
            "publication_requested",
            &p.id,
            json!({"repository": who_repository, "revision": m.revision, "event_seq": m.event_seq,
                "commit": m.commit, "branch": m.branch, "base": m.base, "changes": m.changes.len()}),
        )
    }
}

impl Publication {
    /// Applies `update`; the event to record, if it changed any state. A forge fact never
    /// goes backwards: a created branch stays created, a recorded pull request stays recorded.
    pub(super) fn apply(&mut self, update: &Update, now: i64) -> Result<(bool, Option<Event>)> {
        let id = self.id.clone();
        let (revision, commit) = (self.revision.clone(), self.commit.clone());
        let event = match update {
            Update::Error(error) => {
                if self.error == *error {
                    return Ok((false, None));
                }
                self.error.clone_from(error);
                None
            }
            Update::BranchAttempt => {
                if matches!(self.branch_state.as_str(), "attempting" | "created") {
                    return Ok((false, None));
                }
                self.branch_state = "attempting".into();
                None
            }
            Update::BranchCreated => {
                if self.branch_state == "created" {
                    return Ok((false, None));
                }
                self.branch_state = "created".into();
                self.branch_message = None;
                let data = json!({"ref": format!("refs/heads/{}", self.branch),
                    "commit": commit, "revision": revision});
                Some(Event::new("publication_branch_created", &id, data)?)
            }
            Update::BranchRejected { status, message } => {
                // The same refusal again (a retry against a protected branch) is not news; only
                // the message is kept, so status is not compared.
                let same = self.branch_message.as_deref() == Some(message.as_str());
                if self.branch_state == "created" || (same && self.branch_state == "rejected") {
                    return Ok((false, None));
                }
                self.branch_state = "rejected".into();
                self.branch_message = Some(message.clone());
                if same {
                    None
                } else {
                    let data = json!({"ref": format!("refs/heads/{}", self.branch),
                        "commit": commit, "revision": revision, "status": status,
                        "message": message});
                    Some(Event::new("publication_branch_rejected", &id, data)?)
                }
            }
            Update::PullRequest {
                number,
                node_id,
                html_url,
            } => {
                if self.pr_number.is_some() {
                    return Ok((false, None));
                }
                if self.branch_state != "created" {
                    return Err(invalid("a pull request needs a created branch"));
                }
                self.pr_number = Some(*number);
                self.pr_node_id = Some(node_id.clone());
                self.pr_url = Some(html_url.clone());
                let data = json!({"number": number, "node_id": node_id, "html_url": html_url,
                    "commit": commit, "revision": revision});
                Some(Event::new("publication_pr_opened", &id, data)?)
            }
            Update::Merged { merge_commit } => {
                let Some(number) = self.pr_number else {
                    return Err(invalid("only a recorded pull request can be merged"));
                };
                if self.merge_commit.is_some() {
                    return Ok((false, None));
                }
                self.merge_commit = Some(merge_commit.clone());
                let data = json!({"number": number, "merge_commit": merge_commit,
                    "commit": commit, "revision": revision});
                Some(Event::new("publication_merged", &id, data)?)
            }
        };
        self.updated_at = now;
        Ok((true, event))
    }

    /// The states a client sees; an attempt in flight is still `pending` to it.
    pub fn states(&self) -> Value {
        let pull_request = match (&self.pr_number, &self.merge_commit) {
            (Some(number), merge) => json!({
                "state": if merge.is_some() { "merged" } else { "opened" },
                "number": number, "node_id": self.pr_node_id, "html_url": self.pr_url,
                "merge_commit": merge}),
            (None, _) => json!({"state": "pending"}),
        };
        let (branch, message) = if self.branch_state == "attempting" {
            ("pending", None)
        } else {
            (self.branch_state.as_str(), self.branch_message.as_ref())
        };
        // `native_landed` is the client's claim: the hub has no native data to check it against.
        json!({"native_landed": true,
            "branch": {"state": branch, "commit": self.commit,
                "ref": format!("refs/heads/{}", self.branch), "message": message},
            "pull_request": pull_request})
    }

    pub fn to_json(&self) -> Value {
        let mut reply = json!({"id": self.id, "repository": self.repository,
            "revision": self.revision, "event_seq": self.event_seq, "commit": self.commit,
            "branch": self.branch, "base": self.base, "states": self.states()});
        if let Some(error) = &self.error {
            reply["error"] = json!(error);
        }
        reply
    }

    fn manifest(&self) -> Result<Manifest> {
        serde_json::from_str(&self.manifest).map_err(|e| HubError::Unavailable(e.to_string()))
    }
}

/// Column order read by both backends' row mappers.
pub(super) const PUBLICATION_SELECT: &str = "SELECT p.id, p.repository, p.token, t.name, u.login,
        p.revision, p.event_seq, p.commit_sha, p.branch, p.base, p.manifest, p.branch_state,
        p.branch_message, p.pr_number, p.pr_node_id, p.pr_url, p.merge_commit, p.error,
        p.created_at, p.updated_at, t.owner
    FROM hub_publications p JOIN hub_agent_tokens t ON t.id = p.token
    JOIN hub_users u ON u.id = t.owner";

pub fn routes() -> Router<HubState> {
    Router::new()
        .route(
            "/v1/hub/repos/{repo}/publications",
            post(create).layer(DefaultBodyLimit::max(MANIFEST_BODY_BYTES)),
        )
        .route("/v1/hub/repos/{repo}/publications/{id}", get(show))
        .route(
            "/v1/hub/repos/{repo}/publications/{id}/blobs/{sha256}",
            put(upload).layer(DefaultBodyLimit::max(MAX_BLOB_BYTES)),
        )
        .route("/v1/hub/repos/{repo}/publications/{id}/run", post(run))
}

type ApiResult = std::result::Result<Json<Value>, ApiError>;

fn conflict(code: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code)
}

fn blob_key(publication: &str, sha256: &str) -> Result<ObjectKey> {
    Ok(ObjectKey::new(
        BLOB_DOMAIN,
        publication,
        SOURCE_PROFILE_V1,
        sha256,
    )?)
}

/// The connected repository and its connection for `principal`; 409 unless connected
/// (and, with `outbound`, the connection has opted in to writes).
async fn target(
    state: &HubState,
    principal: &Principal,
    outbound: bool,
) -> std::result::Result<(Connection, ConnectedRepository), ApiError> {
    let (host, name) = (
        principal.owner.identity.host.clone(),
        principal.repository.clone(),
    );
    let repository_id = principal.repository_id;
    let store = state.store.clone();
    let found = blocking(state, move || {
        let Some(repo) = find_repository(&*store, &host, &name, repository_id)? else {
            return Ok(None);
        };
        Ok(store.connection(&repo.connection)?.map(|conn| (conn, repo)))
    })
    .await?
    .ok_or_else(|| conflict("not_connected"))?;
    // Publishing speaks the GitHub API; another forge's token must never be sent there.
    if found.0.provider != "github" {
        return Err(conflict("unsupported_provider"));
    }
    if outbound && !found.0.outbound {
        return Err(conflict("outbound_disabled"));
    }
    Ok(found)
}

async fn load(
    state: &HubState,
    repository: &str,
    id: &str,
) -> std::result::Result<Publication, ApiError> {
    let (store, repository, id) = (state.store.clone(), repository.to_owned(), id.to_owned());
    blocking(state, move || store.publication(&repository, &id))
        .await?
        .ok_or_else(ApiError::not_found)
}

async fn record(state: &HubState, id: &str, update: Update) -> Result<Publication> {
    let (store, id) = (state.store.clone(), id.to_owned());
    blocking(state, move || {
        store.update_publication(&id, &update, unix_now())
    })
    .await
}

/// Only the creating token's owner may upload and run; anyone else sees an unknown publication.
fn owned(principal: &Principal, publication: &Publication) -> std::result::Result<(), ApiError> {
    if publication.owner != principal.owner.id {
        return Err(ApiError::not_found());
    }
    Ok(())
}

async fn create(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(repo): Path<String>,
    JsonBody(mut request): JsonBody<Value>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let id = request
        .as_object_mut()
        .and_then(|object| object.remove("publication_id"))
        .and_then(|id| id.as_str().map(str::to_owned))
        .ok_or_else(ApiError::invalid)?;
    let manifest: Manifest = serde_json::from_value(request).map_err(|_| ApiError::invalid())?;
    let new = NewPublication { id, manifest };
    new.validate(&principal.repository)?;
    target(&state, &principal, true).await?;
    let store = state.store.clone();
    let token = principal.token_id.clone();
    let created = blocking(&state, move || {
        store.create_publication(&token, &new, unix_now())
    })
    .await?;
    let publication = match created {
        Create::Done(publication) => publication,
        Create::Conflict => return Err(conflict("publication_conflict")),
        Create::TooMany => {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_publications",
            ));
        }
    };
    owned(&principal, &publication)?;
    let missing = missing_blobs(&state, &publication).await?;
    Ok(Json(
        json!({"id": publication.id, "state": publication.states(), "missing_blobs": missing}),
    ))
}

/// SHA-256s of the manifest's blobs the hub does not hold yet.
async fn missing_blobs(state: &HubState, publication: &Publication) -> Result<Vec<String>> {
    let manifest = publication.manifest()?;
    let objects = state.objects.clone();
    let id = publication.id.clone();
    let wanted: Vec<String> = manifest.blobs().iter().map(|b| b.0.to_owned()).collect();
    blocking(state, move || {
        let mut missing = Vec::new();
        for sha in wanted {
            let key = blob_key(&id, &sha)?;
            objects.prepare_publication(&key, BLOB_INCARNATION)?;
            match objects.inspect_verified(&key, BLOB_INCARNATION) {
                Ok(_) => {}
                Err(crate::Error::NotFound(_) | crate::Error::MissingObject(_)) => {
                    missing.push(sha)
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(missing)
    })
    .await
}

async fn upload(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, id, sha256)): Path<(String, String, String)>,
    body: Bytes,
) -> std::result::Result<StatusCode, ApiError> {
    scope(&principal, &repo)?;
    let publication = load(&state, &repo, &id).await?;
    owned(&principal, &publication)?;
    if !hex_of(&sha256, 64) {
        return Err(ApiError::invalid());
    }
    let manifest = publication.manifest()?;
    let size = manifest
        .blobs()
        .into_iter()
        .find(|(sha, _)| *sha == sha256)
        .map(|(_, size)| size)
        .ok_or_else(ApiError::not_found)?;
    // Refused, never truncated: the body limit answers 413 past 8 MiB, and the bytes must be
    // exactly the blob the manifest names.
    if body.len() as u64 != size || lower_hex_of(&body) != sha256 {
        return Err(ApiError::invalid());
    }
    let objects = state.objects.clone();
    blocking(&state, move || {
        let key = blob_key(&id, &sha256)?;
        objects.prepare_publication(&key, BLOB_INCARNATION)?;
        objects.put_if_absent(&key, BLOB_INCARNATION, &body)?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// True when `id`'s pull request was not read within [`READ_INTERVAL`]; marks it read now.
fn read_due(id: &str) -> bool {
    let mut guard = LAST_READ.lock().unwrap_or_else(|e| e.into_inner());
    let reads = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    if reads
        .get(id)
        .is_some_and(|at| now.duration_since(*at) < READ_INTERVAL)
    {
        return false;
    }
    // shortcut: forget everything past 10000 publications; the cost is one extra forge read each
    if reads.len() >= 10_000 {
        reads.clear();
    }
    reads.insert(id.to_owned(), now);
    true
}

async fn show(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, id)): Path<(String, String)>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let mut publication = load(&state, &repo, &id).await?;
    if let (Some(number), None) = (publication.pr_number, &publication.merge_commit)
        && read_due(&id)
        && let Ok((conn, repository)) = target(&state, &principal, false).await
    {
        match merged_commit(&state, &conn, &repository, number).await {
            Ok(Some(merge_commit)) => {
                publication = record(&state, &id, Update::Merged { merge_commit }).await?;
            }
            Ok(None) => {}
            // The stored state is still true; a failed read must not hide it. Only a 401 marks the
            // connection: a 403 on this read says nothing about its credentials.
            Err(error @ (GhError::Auth(_) | GhError::Limited(_))) => {
                forge_failure(&state, &conn, error).await?;
            }
            Err(GhError::Failed(detail)) => {
                eprintln!("hub: reading a published pull request: {detail}")
            }
        }
    }
    Ok(Json(publication.to_json()))
}

async fn merged_commit(
    state: &HubState,
    conn: &Connection,
    repository: &ConnectedRepository,
    number: i64,
) -> Gh<Option<String>> {
    let token = state.sync.token(conn, repository.installation_id).await?;
    let pull = state
        .sync
        .api()?
        .pull_soft(&token, &repository.full_name, number)
        .await?;
    Ok(pull
        .filter(|pull| pull["merged"] == true)
        .and_then(|pull| pull["merge_commit_sha"].as_str().map(str::to_owned)))
}

/// Records what a failed call means for the connection and returns text for the publication.
async fn forge_failure(state: &HubState, conn: &Connection, error: GhError) -> Result<String> {
    let text = match &error {
        GhError::Auth(_) => "GitHub rejected the hub's credentials".to_owned(),
        GhError::Limited(_) => "GitHub rate limit reached; retry later".to_owned(),
        GhError::Failed(detail) => clean(detail),
    };
    failed(state, conn, "publication", error).await?;
    Ok(text)
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_ERROR_CHARS)
        .collect()
}

async fn run(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, id)): Path<(String, String)>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let publication = load(&state, &repo, &id).await?;
    owned(&principal, &publication)?;
    let (_, repository) = target(&state, &principal, true).await?;
    if !missing_blobs(&state, &publication).await?.is_empty() {
        return Err(conflict("blobs_missing"));
    }
    // A dropped client must not stop the run halfway; the steps are idempotent anyway.
    let job = tokio::spawn(execute(state, repository, publication));
    let publication = job
        .await
        .map_err(|_| HubError::Unavailable("publication task failed".into()))??;
    Ok(Json(publication.to_json()))
}

/// Why a run stopped.
enum Stop {
    Forge(GhError),
    Hub(HubError),
    /// Nothing was pushed, or what the forge showed disagrees with the manifest.
    Message(String),
    /// Branch rejected and recorded; there is nothing more to do.
    Done,
}

impl From<GhError> for Stop {
    fn from(error: GhError) -> Self {
        Self::Forge(error)
    }
}

impl From<HubError> for Stop {
    fn from(error: HubError) -> Self {
        Self::Hub(error)
    }
}

/// Runs the forge steps for `publication` under the repository's lock, which sync of that
/// repository also takes: it waits for a run in progress.
async fn execute(
    state: HubState,
    repository: ConnectedRepository,
    publication: Publication,
) -> Result<Publication> {
    let _guard = state.sync.lock_repo(&repository.id).await;
    // Everything is read again under the lock: a run ahead of this one may have recorded
    // progress, and the connection may have been switched off or failed while this one waited.
    let (store, name, id, connection) = (
        state.store.clone(),
        publication.repository.clone(),
        publication.id.clone(),
        repository.connection.clone(),
    );
    let (publication, conn) = blocking(&state, move || {
        Ok((
            store.publication(&name, &id)?,
            store.connection(&connection)?,
        ))
    })
    .await?;
    let (Some(publication), Some(conn)) = (publication, conn) else {
        return Err(invalid("unknown publication or connection"));
    };
    let error = match push(&state, &conn, &repository, &publication).await {
        Ok(()) | Err(Stop::Done) => None,
        Err(Stop::Hub(error)) => return Err(error),
        Err(Stop::Message(message)) => Some(message),
        Err(Stop::Forge(error)) => Some(forge_failure(&state, &conn, error).await?),
    };
    record(&state, &publication.id, Update::Error(error)).await
}

/// The forge steps. Each starts from what the forge shows, so a rerun after any failure
/// (including a reply lost after the forge acted) continues rather than repeats.
async fn push(
    state: &HubState,
    conn: &Connection,
    repository: &ConnectedRepository,
    publication: &Publication,
) -> std::result::Result<(), Stop> {
    // Done is done: a branch moved or deleted since (after a merge, say) is not this run's business.
    if publication.pr_number.is_some() {
        return Ok(());
    }
    if !conn.outbound {
        return Err(Stop::Message(
            "outbound writes are off for the connection".into(),
        ));
    }
    if conn.status != "enabled" {
        return Err(Stop::Message(
            "the connection's credentials failed; fix it, then run again".into(),
        ));
    }
    let manifest = publication.manifest()?;
    let api = state.sync.api()?;
    let token = state.sync.token(conn, repository.installation_id).await?;
    let (full, branch, id) = (&repository.full_name, &manifest.branch, &publication.id);
    let created_before = publication.branch_state == "created";
    match api.ref_sha(&token, full, branch).await? {
        // A branch at our commit is ours only if this publication sent the create (a reply
        // can be lost); otherwise someone else pushed the same commit, and we do not adopt it.
        Some(sha)
            if sha == manifest.commit
                && matches!(publication.branch_state.as_str(), "attempting" | "created") =>
        {
            record(state, id, Update::BranchCreated).await?;
        }
        Some(sha) if sha == manifest.commit => {
            let message = "branch already exists".to_owned();
            let status = 422;
            record(state, id, Update::BranchRejected { status, message }).await?;
            return Err(Stop::Done);
        }
        Some(_) if created_before => {
            return Err(Stop::Message(
                "the branch no longer points at the published commit".into(),
            ));
        }
        Some(_) => {
            let message = "branch exists at another commit".to_owned();
            record(
                state,
                id,
                Update::BranchRejected {
                    status: 422,
                    message,
                },
            )
            .await?;
            return Err(Stop::Done);
        }
        None if created_before => {
            return Err(Stop::Message(
                "the branch was deleted after it was created; publish under a new branch name"
                    .into(),
            ));
        }
        None => {
            build_commit(state, &token, full, publication, &manifest).await?;
            record(state, id, Update::BranchAttempt).await?;
            match api
                .create_ref(&token, full, branch, &manifest.commit)
                .await?
            {
                Ok(()) => {
                    record(state, id, Update::BranchCreated).await?;
                }
                Err((status, message)) => {
                    // A lost reply to an earlier create looks the same as a refusal.
                    if api.ref_sha(&token, full, branch).await?.as_deref() == Some(&manifest.commit)
                    {
                        record(state, id, Update::BranchCreated).await?;
                    } else {
                        let message = clean(&message);
                        record(state, id, Update::BranchRejected { status, message }).await?;
                        return Err(Stop::Done);
                    }
                }
            }
        }
    }
    let owner = full.split('/').next().unwrap_or_default();
    let open = api.find_open_pulls(&token, full, owner, branch).await?;
    let adopted = open.iter().find(|pull| {
        pull["head"]["sha"] == manifest.commit.as_str()
            && pull["base"]["ref"] == manifest.base.as_str()
    });
    let pull = match adopted {
        Some(pull) => pull.clone(),
        None => {
            let footer = format!(
                "Published by AgentSCM for {}'s agent ({}) from revision {}.",
                publication.owner_login, publication.agent, manifest.revision
            );
            let body = match manifest.body.trim() {
                "" => footer,
                text => format!("{text}\n\n{footer}"),
            };
            api.create_pull(&token, full, &manifest.title, &body, branch, &manifest.base)
                .await?
        }
    };
    let (Some(number), Some(node_id), Some(html_url)) = (
        pull["number"].as_i64(),
        pull["node_id"].as_str(),
        pull["html_url"].as_str(),
    ) else {
        return Err(Stop::Message(
            "unreadable pull request from the forge".into(),
        ));
    };
    let update = Update::PullRequest {
        number,
        node_id: node_id.to_owned(),
        html_url: html_url.to_owned(),
    };
    record(state, id, update).await?;
    Ok(())
}

/// Blobs, tree and commit, checked against the manifest. Nothing is referenced yet.
async fn build_commit(
    state: &HubState,
    token: &str,
    full: &str,
    publication: &Publication,
    manifest: &Manifest,
) -> std::result::Result<(), Stop> {
    let api = state.sync.api()?;
    let base_tree = api
        .commit_tree(token, full, &manifest.parent)
        .await?
        .ok_or_else(|| Stop::Message("the base commit is not on the forge".into()))?;
    let mut entries = Vec::new();
    let mut uploaded = BTreeSet::new();
    for change in &manifest.changes {
        let sha = match (&change.blob, &change.sha256) {
            (Some(blob), Some(sha256)) => {
                if uploaded.insert(sha256.as_str()) {
                    let (objects, key) =
                        (state.objects.clone(), blob_key(&publication.id, sha256)?);
                    let bytes = blocking(state, move || {
                        Ok(objects.get_verified(&key, BLOB_INCARNATION)?)
                    })
                    .await?;
                    if api.create_blob(token, full, &bytes).await? != *blob {
                        return Err(Stop::Message(
                            "blob_mismatch: the forge hashed a file differently".into(),
                        ));
                    }
                }
                Some(blob.as_str())
            }
            _ => None,
        };
        let mode = if sha.is_some() {
            &change.mode
        } else {
            "100644"
        };
        entries.push(json!({"path": change.path, "mode": mode, "type": "blob", "sha": sha}));
    }
    let tree = api.create_tree(token, full, &base_tree, entries).await?;
    if tree != manifest.tree {
        return Err(Stop::Message(format!(
            "projection_mismatch: the forge built tree {tree}, expected {}",
            manifest.tree
        )));
    }
    let commit = api
        .create_commit(
            token,
            full,
            json!({"message": manifest.message, "tree": manifest.tree,
                "parents": [manifest.parent], "author": manifest.author,
                "committer": manifest.committer}),
        )
        .await?;
    if commit != manifest.commit {
        return Err(Stop::Message(format!(
            "projection_mismatch: the forge built commit {commit}, expected {}",
            manifest.commit
        )));
    }
    Ok(())
}

/// The id a client derives so that rerunning a publish finds the same publication.
pub fn derive_id(repository: &str, event: u64, commit: &str, branch: &str) -> String {
    let mut hash = Sha256::new();
    for part in [repository, &event.to_string(), commit, branch] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    let digest = hash.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("pub_{}", &hex[..32])
}
