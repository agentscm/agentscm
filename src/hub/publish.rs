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
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Path, Query};
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::connection::{ConnectedRepository, Connection};
use super::external_runs::ExternalKind;
use super::github::{Gh, GhError, parse_rfc3339, valid_ref_name};
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_seq: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<Candidate>,
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

/// A draft subject is not evidence of native integration. Its Git branch was
/// published by the scoped Mirror transport before this bridge opens the PR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub subject: crate::candidate::CandidateSubject,
    pub endpoint: String,
    pub previous_publication: Option<String>,
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
    pub event_seq: Option<i64>,
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

/// Shared transactional lineage admission for both persistent backends.
pub(super) fn candidate_continuation(
    new: &NewPublication,
    previous: Option<&Publication>,
    owner: &str,
) -> Result<bool> {
    let Some(candidate) = &new.manifest.candidate else {
        return Ok(previous.is_none());
    };
    let Some(previous) = previous else {
        return Ok(candidate.previous_publication.is_none());
    };
    let old = previous.manifest()?;
    Ok(
        candidate.previous_publication.as_deref() == Some(&previous.id)
            && previous.owner == owner
            && (previous.pr_number.is_some()
                || matches!(
                    previous.branch_state.as_str(),
                    "pending" | "created" | "rejected" | "pr_refused"
                ))
            && previous.merge_commit.is_none()
            && previous.branch == new.manifest.branch
            && previous.base == new.manifest.base
            && old.candidate.as_ref().is_some_and(|old| {
                old.subject.same_lineage(&candidate.subject) && old.endpoint == candidate.endpoint
            }),
    )
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
    PullRequestAttempt,
    PullRequestRefused {
        status: u16,
    },
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

fn valid_pull_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
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
        match (&self.event_seq, &self.candidate) {
            (Some(event), None) if *event > 0 => {}
            (None, Some(candidate)) => {
                let subject = &candidate.subject;
                subject
                    .change
                    .validate()
                    .map_err(|_| invalid("invalid candidate Change"))?;
                for id in [
                    &subject.id,
                    &subject.attachment,
                    &subject.context_digest,
                    &subject.revision,
                    &subject.source_fingerprint,
                    &subject.comparison_fingerprint,
                ] {
                    crate::store::validate_id(id)
                        .map_err(|_| invalid("invalid candidate identity"))?;
                }
                if subject
                    .canonical_id()
                    .map_err(|_| invalid("invalid candidate digest"))?
                    != subject.id
                    || subject.endpoint != candidate.endpoint
                    || subject.output != self.branch
                    || subject.revision != self.revision
                    || subject.target != self.base
                    || self.base == self.branch
                    || !hex_of(&subject.baseline_commit, 40)
                    || !hex_of(&subject.source_commit, 40)
                    || subject.parent_commit != self.parent
                {
                    return Err(invalid("candidate binding differs from publication"));
                }
                candidate_repository(&candidate.endpoint)?;
                if candidate
                    .previous_publication
                    .as_ref()
                    .is_some_and(|id| !id.starts_with("pub_") || !hex_of(&id[4..], 32))
                {
                    return Err(invalid("invalid previous publication"));
                }
            }
            _ => {
                return Err(invalid(
                    "exactly one positive event_seq or typed candidate is required",
                ));
            }
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
        let expected = match (&m.event_seq, &m.candidate) {
            (Some(event), None) => derive_id(repository, *event as u64, &m.commit, &m.branch),
            (None, Some(candidate)) => {
                if candidate_repository(&candidate.endpoint)?.1 != repository {
                    return Err(invalid("candidate endpoint repository differs"));
                }
                derive_candidate_id(repository, &candidate.subject.id, &m.commit, &m.branch)
            }
            _ => return Err(invalid("invalid publication subject")),
        };
        if self.id != expected {
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
                if matches!(
                    self.branch_state.as_str(),
                    "attempting" | "created" | "pr_attempting" | "pr_refused"
                ) {
                    return Ok((false, None));
                }
                self.branch_state = "attempting".into();
                None
            }
            Update::PullRequestAttempt => {
                if matches!(self.branch_state.as_str(), "pr_attempting" | "pr_refused")
                    || self.pr_number.is_some()
                {
                    return Err(invalid("PR write was already attempted"));
                }
                if self.branch_state != "created" {
                    return Err(invalid("PR attempt requires observed branch"));
                }
                self.branch_state = "pr_attempting".into();
                None
            }
            Update::PullRequestRefused { status } => {
                if self.branch_state != "pr_attempting" || !matches!(status, 400 | 403 | 404 | 422)
                {
                    return Err(invalid(
                        "PR refusal requires an attempted write and definitive refusal",
                    ));
                }
                self.branch_state = "pr_refused".into();
                self.branch_message = Some(format!(
                    "PR create refused ({status}); manual adoption or new qualified successor required"
                ));
                None
            }
            Update::BranchCreated => {
                if matches!(
                    self.branch_state.as_str(),
                    "created" | "pr_attempting" | "pr_refused"
                ) {
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
                if matches!(
                    self.branch_state.as_str(),
                    "created" | "pr_attempting" | "pr_refused"
                ) || (same && self.branch_state == "rejected")
                {
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
                if *number <= 0 || node_id.is_empty() || !valid_pull_url(html_url) {
                    return Err(invalid("invalid PR identity"));
                }
                if self.pr_number.is_some() {
                    return Ok((false, None));
                }
                if !matches!(
                    self.branch_state.as_str(),
                    "created" | "pr_attempting" | "pr_refused"
                ) {
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
        } else if matches!(self.branch_state.as_str(), "pr_attempting" | "pr_refused") {
            ("created", None)
        } else {
            (self.branch_state.as_str(), self.branch_message.as_ref())
        };
        // `native_landed` is the client's claim: the hub has no native data to check it against.
        json!({"native_landed": self.event_seq.is_some(),
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
        if let Ok(manifest) = self.manifest()
            && let Some(candidate) = manifest.candidate
        {
            reply["candidate"] = json!(candidate);
            reply["historical"] = json!(true);
            reply["qualification"] = json!("not_checked");
        }
        reply
    }

    pub(super) fn manifest(&self) -> Result<Manifest> {
        if self.manifest.len() > MANIFEST_BODY_BYTES {
            return Err(invalid("retained publication exceeds manifest budget"));
        }
        let manifest: Manifest = serde_json::from_str(&self.manifest)
            .map_err(|e| HubError::Unavailable(e.to_string()))?;
        NewPublication {
            id: self.id.clone(),
            manifest: manifest.clone(),
        }
        .validate(&self.repository)?;
        if manifest.revision != self.revision
            || manifest.event_seq != self.event_seq
            || manifest.commit != self.commit
            || manifest.branch != self.branch
            || manifest.base != self.base
        {
            return Err(invalid("retained publication columns differ from manifest"));
        }
        Ok(manifest)
    }
}

/// Column order read by both backends' row mappers.
pub(super) const PUBLICATION_SELECT: &str = "SELECT p.id, p.repository, p.token, t.name, u.login,
        p.revision, p.event_seq, p.commit_sha, p.branch, p.base, CASE WHEN octet_length(p.manifest)<=2097152 THEN p.manifest END, p.branch_state,
        p.branch_message, p.pr_number, p.pr_node_id, p.pr_url, p.merge_commit, p.error,
        p.created_at, p.updated_at, t.owner
    FROM hub_publications p JOIN hub_agent_tokens t ON t.id = p.token
    JOIN hub_users u ON u.id = t.owner";

pub fn routes() -> Router<HubState> {
    Router::new()
        .route(
            "/v1/hub/repos/{repo}/publications",
            post(create)
                .layer(DefaultBodyLimit::max(MANIFEST_BODY_BYTES))
                .get(list),
        )
        .route("/v1/hub/repos/{repo}/publications/{id}", get(show))
        .route(
            "/v1/hub/repos/{repo}/publications/{id}/blobs/{sha256}",
            put(upload).layer(DefaultBodyLimit::max(MAX_BLOB_BYTES)),
        )
        .route("/v1/hub/repos/{repo}/publications/{id}/run", post(run))
        .route(
            "/v1/hub/repos/{repo}/publications/{id}/recover",
            post(recover),
        )
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
    candidate: bool,
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
    // Each forge's token goes only to that forge's API (see `Sync::forge`). Event publication
    // rebuilds commits through the GitHub Git Data API, so only candidates may use GitLab.
    if !(found.0.provider == "github" || (candidate && found.0.provider == "gitlab")) {
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
    let publication = blocking(state, move || store.publication(&repository, &id))
        .await?
        .ok_or_else(ApiError::not_found)?;
    publication.manifest()?;
    Ok(publication)
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
    let (connection, connected) =
        target(&state, &principal, true, new.manifest.candidate.is_some()).await?;
    if let Some(candidate) = &new.manifest.candidate {
        let (host, full) = candidate_repository(&candidate.endpoint)?;
        if host != connected.host || full != connected.full_name {
            return Err(conflict("candidate_endpoint_mismatch"));
        }
        if name_shared_across_hosts(&state, &connected).await? {
            return Err(conflict("ambiguous_repository_name"));
        }
        let token = state
            .sync
            .token(&connection, connected.installation_id)
            .await
            .map_err(|_| conflict("candidate_default_unavailable"))?;
        let default = state
            .sync
            .forge(&connection)
            .map_err(|_| conflict("candidate_default_unavailable"))?
            .default_branch(&token, &connected)
            .await
            .map_err(|_| conflict("unsupported_candidate_default"))?;
        if new.manifest.branch == default
            || new.manifest.branch == "HEAD"
            || new.manifest.branch.starts_with("refs/")
        {
            return Err(conflict("candidate_output_is_default_or_alias"));
        }
    }
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

/// Whether the candidate's endpoint is this repository's host and no other host shares its name.
pub(super) async fn candidate_on_host(
    state: &HubState,
    publication: &Publication,
    repository: &ConnectedRepository,
) -> bool {
    let on_host = publication
        .manifest()
        .ok()
        .and_then(|m| m.candidate)
        .and_then(|c| candidate_repository(&c.endpoint).ok())
        .is_some_and(|(host, _)| host == repository.host);
    on_host && matches!(name_shared_across_hosts(state, repository).await, Ok(false))
}

/// Whether a forge-side read or post for `publication` may use `repository`'s pull request of the
/// same number. Event publications are GitHub-only. A candidate publication is keyed by name alone,
/// so it counts only on the host it was published to and where no other host shares the name.
pub(super) async fn publication_on_forge(
    state: &HubState,
    publication: &Publication,
    repository: &ConnectedRepository,
) -> bool {
    let candidate = publication.manifest().ok().map(|m| m.candidate.is_some());
    match candidate {
        Some(false) => true,
        Some(true) => candidate_on_host(state, publication, repository).await,
        None => false,
    }
}

/// Publications listed per page by default and at most.
const DEFAULT_LISTED: usize = 25;
const MAX_LISTED: usize = 100;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    limit: Option<usize>,
    before: Option<String>,
}

/// The repository's publications, newest first, for the status page. Store rows only: no forge
/// call, and none of the per-publication `forge_check` or `external` views that `show` adds.
async fn list(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
    query: std::result::Result<Query<ListQuery>, QueryRejection>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let limit = query.limit.unwrap_or(DEFAULT_LISTED);
    if !(1..=MAX_LISTED).contains(&limit) {
        return Err(ApiError::invalid());
    }
    let store = state.store.clone();
    let mut rows = blocking(&state, move || {
        // An unknown cursor is a bad request, not an empty page.
        if let Some(before) = &query.before
            && store.publication(&repo, before)?.is_none()
        {
            return Ok(None);
        }
        store
            .publications(&repo, query.before.as_deref(), limit + 1)
            .map(Some)
    })
    .await?
    .ok_or_else(ApiError::invalid)?;
    let next_cursor = if rows.len() > limit {
        rows.truncate(limit);
        rows.last().map(|p| p.id.clone())
    } else {
        None
    };
    let publications: Vec<Value> = rows
        .iter()
        .map(|p| {
            let mut item = p.to_json();
            item["created_at"] = json!(p.created_at);
            item["updated_at"] = json!(p.updated_at);
            item
        })
        .collect();
    Ok(Json(json!({"publications": publications,
        "complete": next_cursor.is_none(), "next_cursor": next_cursor})))
}

async fn show(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path((repo, id)): Path<(String, String)>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let mut publication = load(&state, &repo, &id).await?;
    // Reading the merge from the forge records it and can mark the connection failed, so a
    // read-only caller gets the stored row; agent reads and the sync still record merges.
    if !principal.read_only
        && let (Some(number), None) = (publication.pr_number, &publication.merge_commit)
        && read_due(&id)
        && let Some(candidate) = publication.manifest().ok().map(|m| m.candidate.is_some())
        && let Ok((conn, repository)) = target(&state, &principal, false, candidate).await
        // Publications are keyed by repository name alone: read the merge only from the forge the
        // candidate was published to, and never where another forge host shares the name.
        && (!candidate || candidate_on_host(&state, &publication, &repository).await)
    {
        match merged_commit(&state, &conn, &repository, number, &publication.commit).await {
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
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut reply = publication.to_json();
    #[cfg(unix)]
    if let Some(view) = forge_check_view(&state, &principal, &publication).await? {
        reply["forge_check"] = view;
    }
    if let Some(view) = external_view(&state, &principal, &publication).await? {
        reply["external"] = view;
    }
    Ok(Json(reply))
}

/// External runs shown per publication; one more is read to tell a full list from a cut one.
const EXTERNAL_SHOWN: usize = 200;

/// External CI and deployment outcomes stored for a merged publication's merge commit, only
/// where the repository is opted in. Read-only: it reads the store and calls no forge.
async fn external_view(
    state: &HubState,
    principal: &Principal,
    publication: &Publication,
) -> std::result::Result<Option<Value>, ApiError> {
    let Some(sha) = publication.merge_commit.clone() else {
        return Ok(None);
    };
    if !state.sync.external_runs_enabled(&principal.repository) {
        return Ok(None);
    }
    let (host, name, repository_id) = (
        principal.owner.identity.host.clone(),
        principal.repository.clone(),
        principal.repository_id,
    );
    let store = state.store.clone();
    let found = blocking(state, move || {
        let Some(repo) = find_repository(&*store, &host, &name, repository_id)? else {
            return Ok(None);
        };
        Ok(store.connection(&repo.connection)?.map(|conn| (conn, repo)))
    })
    .await?;
    let Some((conn, repo)) = found else {
        return Ok(None);
    };
    if conn.provider != "github"
        || !state.sync.external_runs_enabled(&repo.full_name)
        || !publication_on_forge(state, publication, &repo).await
    {
        return Ok(None);
    }
    let (store, id, commit) = (state.store.clone(), repo.id.clone(), sha.clone());
    // Each kind has its own cap, so many CI runs cannot hide the deployments.
    let (mut ci_runs, mut deployed, marker) = blocking(state, move || {
        Ok((
            store.external_runs(&id, &commit, ExternalKind::Ci, EXTERNAL_SHOWN + 1)?,
            store.external_runs(&id, &commit, ExternalKind::Deployment, EXTERNAL_SHOWN + 1)?,
            store.external_reconciled(&id, &commit)?,
        ))
    })
    .await?;
    let cut = ci_runs.len() > EXTERNAL_SHOWN || deployed.len() > EXTERNAL_SHOWN;
    ci_runs.truncate(EXTERNAL_SHOWN);
    deployed.truncate(EXTERNAL_SHOWN);
    let status = match marker {
        None => "unobserved",
        Some((_, true)) if !cut => "observed",
        Some(_) => "incomplete",
    };
    let ci: Vec<Value> = ci_runs
        .into_iter()
        .map(|run| {
            json!({"run_id": run.run_id, "attempt": run.attempt,
                "branch": run.branch, "name": run.name, "outcome": run.outcome.as_str(),
                "provider_updated_at": run.provider_updated_at, "html_url": run.html_url})
        })
        .collect();
    let deployments: Vec<Value> = deployed
        .into_iter()
        .map(|run| {
            json!({"run_id": run.run_id, "environment": run.environment, "name": run.name,
                "outcome": run.outcome.as_str(),
                "provider_updated_at": run.provider_updated_at, "html_url": run.html_url})
        })
        .collect();
    Ok(Some(json!({"provider": "github",
        "note": "Observed after landing; not evidence for the landing decision.",
        "merge_commit": sha, "state": status,
        "reconciled_at": marker.map(|(at, _)| at),
        "ci": ci, "deployments": deployments})))
}

/// What the Hub does about a forge check for this publication's pull request. Read-only:
/// it reads the journal and the configuration and posts nothing.
#[cfg(unix)]
async fn forge_check_view(
    state: &HubState,
    principal: &Principal,
    publication: &Publication,
) -> std::result::Result<Option<Value>, ApiError> {
    let (Some(number), Ok(manifest)) = (publication.pr_number, publication.manifest()) else {
        return Ok(None);
    };
    let Some(candidate) = manifest.candidate else {
        return Ok(None);
    };
    let (host, name, repository_id) = (
        principal.owner.identity.host.clone(),
        principal.repository.clone(),
        principal.repository_id,
    );
    let store = state.store.clone();
    let found = blocking(state, move || {
        let Some(repo) = find_repository(&*store, &host, &name, repository_id)? else {
            return Ok(None);
        };
        Ok(store.connection(&repo.connection)?.map(|conn| (conn, repo)))
    })
    .await?;
    let Some((conn, repo)) = found else {
        return Ok(None);
    };
    if !publication_on_forge(state, publication, &repo).await {
        return Ok(None);
    }
    let target = &candidate.subject.target;
    let mut view = json!({"mode": "git_default", "enabled": false,
        "provider": match conn.provider.as_str() {
            provider @ ("github" | "gitlab") => provider,
            _ => "unsupported",
        },
        "protection": "unverified",
        "note": "Branch protection, raw pushes and administrator bypass are not enforced or verified by AgentSCM."});
    let enabled = matches!(conn.provider.as_str(), "github" | "gitlab")
        && state.workspace_execution.as_ref().is_some_and(|service| {
            service.agent_task_profiles.values().any(|p| {
                p.publish_forge_status
                    && p.target == *target
                    && p.repository_id.eq_ignore_ascii_case(&repo.full_name)
            })
        });
    if !enabled {
        return Ok(Some(view));
    }
    // An unusable target has no status to report; the publication itself still shows.
    let Ok(context) = super::forge_check::status_context(target) else {
        view["enabled"] = json!(true);
        return Ok(Some(view));
    };
    let (store, id, ctx) = (state.store.clone(), repo.id.clone(), context.clone());
    let row = blocking(state, move || store.forge_check(&id, number, &ctx)).await?;
    view["enabled"] = json!(true);
    view["context"] = json!(context);
    view["state"] = json!(row.as_ref().map(|r| r.state.as_str()));
    view["head_sha"] = json!(row.as_ref().map(|r| r.head_sha.as_str()));
    view["posting"] = json!(row.as_ref().map(|r| match r.phase.as_str() {
        "posted" => "posted",
        "refused" => "refused",
        _ => "uncertain",
    }));
    Ok(Some(view))
}

pub(super) async fn merged_commit(
    state: &HubState,
    conn: &Connection,
    repository: &ConnectedRepository,
    number: i64,
    published: &str,
) -> Gh<Option<String>> {
    let token = state.sync.token(conn, repository.installation_id).await?;
    let pull = state
        .sync
        .forge(conn)?
        .pull_soft(&token, repository, number)
        .await?;
    Ok(pull.filter(|pull| pull["merged"] == true).and_then(|pull| {
        pull["merge_commit_sha"]
            .as_str()
            .or_else(|| {
                // A GitLab fast-forward merge reports no merge commit: the branch tip became
                // the target tip. That proves the merge only when the tip is the published commit.
                (conn.provider == "gitlab")
                    .then(|| pull["head"]["sha"].as_str().filter(|sha| *sha == published))
                    .flatten()
            })
            .map(str::to_owned)
    }))
}

/// Whether a repository of the same name is also connected on another configured forge host.
/// Publications and candidate lineages are keyed by repository name alone, so two forges
/// sharing a name could be read as each other's.
async fn name_shared_across_hosts(
    state: &HubState,
    repository: &ConnectedRepository,
) -> Result<bool> {
    let hosts: Vec<String> = state
        .sync
        .hosts()
        .into_iter()
        .filter(|host| *host != repository.host)
        .collect();
    let (store, name) = (state.store.clone(), repository.full_name.clone());
    blocking(state, move || {
        for host in hosts {
            if store.repository_named(&host, &name)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    })
    .await
}

/// Records what a failed call means for the connection and returns text for the publication.
async fn forge_failure(state: &HubState, conn: &Connection, error: GhError) -> Result<String> {
    let who = super::sync::who(conn);
    let text = match &error {
        GhError::Auth(_) => format!("{who} rejected the hub's credentials"),
        GhError::Limited(_) => format!("{who} rate limit reached; retry later"),
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
    let candidate = publication.manifest()?.candidate.is_some();
    let (_, repository) = target(&state, &principal, true, candidate).await?;
    if !missing_blobs(&state, &publication).await?.is_empty() {
        return Err(conflict("blobs_missing"));
    }
    // A dropped client must not stop the run halfway; the steps are idempotent anyway.
    let job = tokio::spawn(execute(state, repository, publication, false));
    let publication = job
        .await
        .map_err(|_| HubError::Unavailable("publication task failed".into()))??;
    Ok(Json(publication.to_json()))
}

/// Candidate recovery observes only; absence never authorizes a second create.
async fn recover(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, id)): Path<(String, String)>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let publication = load(&state, &repo, &id).await?;
    owned(&principal, &publication)?;
    if publication.manifest()?.candidate.is_none() {
        return Err(conflict("candidate_required"));
    }
    let (_, repository) = target(&state, &principal, false, true).await?;
    let result = execute(state, repository, publication, true).await?;
    Ok(Json(result.to_json()))
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
    recovery: bool,
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
    let error = match push(&state, &conn, &repository, &publication, recovery).await {
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
    recovery: bool,
) -> std::result::Result<(), Stop> {
    publication.manifest()?;
    // Done is done: a branch moved or deleted since (after a merge, say) is not this run's business.
    if publication.pr_number.is_some() {
        return Ok(());
    }
    if !conn.outbound && !recovery {
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
    let token = state.sync.token(conn, repository.installation_id).await?;
    let (full, branch, id) = (&repository.full_name, &manifest.branch, &publication.id);
    if let Some(candidate) = &manifest.candidate {
        return candidate_pull(
            state,
            conn,
            repository,
            publication,
            &manifest,
            candidate,
            recovery,
        )
        .await;
    }
    if recovery {
        return Err(Stop::Message(
            "read-only candidate recovery requires a candidate subject".into(),
        ));
    }
    // Event publication rebuilds commits through the GitHub Git Data API.
    if conn.provider != "github" {
        return Err(Stop::Message(
            "event publication supports only GitHub".into(),
        ));
    }
    let api = state.sync.api()?;
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

/// Credential-free canonical HTTPS repository identity. SSH publication remains
/// an explicit unsupported attachment profile until its mapping is qualified.
pub fn candidate_repository(endpoint: &str) -> Result<(String, String)> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| invalid("invalid candidate endpoint"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.as_str() != endpoint
    {
        return Err(invalid(
            "candidate publication requires canonical credential-free HTTPS",
        ));
    }
    let path = url
        .path()
        .trim_start_matches('/')
        .strip_suffix(".git")
        .unwrap_or(url.path().trim_start_matches('/'));
    super::store::validate_repository(path)?;
    Ok((
        match url.port() {
            Some(port) => format!(
                "{}:{port}",
                url.host_str()
                    .ok_or_else(|| invalid("missing endpoint host"))?
            ),
            None => url
                .host_str()
                .ok_or_else(|| invalid("missing endpoint host"))?
                .to_owned(),
        },
        path.to_owned(),
    ))
}

pub fn derive_candidate_id(repository: &str, subject: &str, commit: &str, branch: &str) -> String {
    let bytes = format!("candidate\0{repository}\0{subject}\0{commit}\0{branch}\0");
    format!("pub_{}", &lower_hex_of(bytes.as_bytes())[..32])
}

async fn candidate_pull(
    state: &HubState,
    conn: &Connection,
    repository: &ConnectedRepository,
    publication: &Publication,
    manifest: &Manifest,
    candidate: &Candidate,
    recovery: bool,
) -> std::result::Result<(), Stop> {
    let (host, full) = candidate_repository(&candidate.endpoint)?;
    if host != repository.host || full != repository.full_name {
        return Err(Stop::Message(
            "candidate endpoint differs from connected repository".into(),
        ));
    }
    if name_shared_across_hosts(state, repository).await? {
        return Err(Stop::Message(
            "repository name is also connected on another forge host".into(),
        ));
    }
    let api = state.sync.forge(conn)?;
    let token = state.sync.token(conn, repository.installation_id).await?;
    if !recovery
        && publication.pr_number.is_none()
        && publication.branch_state != "pr_attempting"
        && (api
            .ref_sha(&token, repository, &manifest.base)
            .await?
            .as_deref()
            != Some(&candidate.subject.baseline_commit)
            || api
                .ref_sha(&token, repository, &manifest.branch)
                .await?
                .as_deref()
                != Some(&manifest.commit))
    {
        return Err(Stop::Message(
            "candidate head or target differs; retain history and requalify".into(),
        ));
    }
    // Walk only unattempted predecessors. Their fixed manifests preserve the chain
    // to the last observed PR even when a stale preflight was superseded.
    let mut prior = None;
    let mut previous = candidate.previous_publication.clone();
    let mut seen = BTreeSet::new();
    while let Some(id) = previous {
        if seen.len() >= 4096 || !seen.insert(id.clone()) {
            return Err(Stop::Message(
                "candidate lineage is cyclic or exceeds limit".into(),
            ));
        }
        let store = state.store.clone();
        let full = full.clone();
        let retained = blocking(state, move || store.publication(&full, &id))
            .await?
            .ok_or_else(|| Stop::Message("previous candidate publication missing".into()))?;
        let old = retained.manifest()?;
        if retained.owner != publication.owner
            || retained.branch != manifest.branch
            || retained.base != manifest.base
            || retained.merge_commit.is_some()
            || !old.candidate.as_ref().is_some_and(|c| {
                c.subject.same_lineage(&candidate.subject) && c.endpoint == candidate.endpoint
            })
        {
            return Err(Stop::Message(
                "candidate continuation is not the same owned Change/PR".into(),
            ));
        }
        if retained.pr_number.is_some() {
            prior = Some(retained);
            break;
        }
        if !matches!(
            retained.branch_state.as_str(),
            "pending" | "created" | "rejected" | "pr_refused"
        ) {
            return Err(Stop::Message("previous PR write remains uncertain".into()));
        }
        previous = old.candidate.and_then(|c| c.previous_publication);
    }
    let marker = format!(
        "<!-- agentscm-candidate:{}:{} -->",
        publication.id, manifest.commit
    );
    let pulls = api
        .find_open_pulls(&token, repository, &manifest.branch, &manifest.base)
        .await?;
    let matching: Vec<_> = pulls
        .iter()
        .filter(|p| {
            p["base"]["ref"] == manifest.base
                && (p["head"]["sha"] == manifest.commit
                    || (recovery
                        && p["body"]
                            .as_str()
                            .is_some_and(|body| body.lines().any(|line| line == marker))))
        })
        .collect();
    if matching.len() > 1 {
        return Err(Stop::Message("ambiguous candidate pull requests".into()));
    }
    let pull = match matching.first() {
        Some(pull) => {
            if prior
                .as_ref()
                .is_some_and(|p| pull["number"].as_i64() != p.pr_number)
            {
                return Err(Stop::Message("candidate PR identity changed".into()));
            }
            (*pull).clone()
        }
        None => {
            if recovery
                || prior.is_some()
                || matches!(
                    publication.branch_state.as_str(),
                    "pr_attempting" | "pr_refused"
                )
            {
                return Err(Stop::Message(
                    "PR outcome unknown or prior PR unavailable; read-only recovery only".into(),
                ));
            }
            let default = api.default_branch(&token, repository).await?;
            if manifest.branch == default
                || manifest.branch == "HEAD"
                || manifest.branch.starts_with("refs/")
            {
                return Err(Stop::Message(
                    "candidate output must not name the forge default branch or a ref alias".into(),
                ));
            }
            // Long provider reads do not establish a lease. Compare again at the
            // new PR effect boundary and retain an attempted state before dispatch.
            if api
                .ref_sha(&token, repository, &manifest.base)
                .await?
                .as_deref()
                != Some(&candidate.subject.baseline_commit)
                || api
                    .ref_sha(&token, repository, &manifest.branch)
                    .await?
                    .as_deref()
                    != Some(&manifest.commit)
            {
                return Err(Stop::Message("candidate changed before PR dispatch".into()));
            }
            let store = state.store.clone();
            let connection = conn.id.clone();
            let live = blocking(state, move || store.connection(&connection)).await?;
            if live.is_none_or(|c| !c.outbound || c.status != "enabled") {
                return Err(Stop::Message(
                    "connection no longer permits publication".into(),
                ));
            }
            record(state, &publication.id, Update::BranchCreated).await?;
            record(state, &publication.id, Update::PullRequestAttempt).await?;
            let result = api
                .create_candidate_pull(
                    &token,
                    repository,
                    &manifest.title,
                    &format!("{}\n\n{marker}", manifest.body),
                    &manifest.branch,
                    &manifest.base,
                )
                .await?;
            match result {
                Ok(pull) => pull,
                Err(status) => {
                    record(
                        state,
                        &publication.id,
                        Update::PullRequestRefused { status },
                    )
                    .await?;
                    return Err(Stop::Message(format!(
                        "PR create refused ({status}); manually open the exact head/base and recover-pr, or prepare a new qualified successor"
                    )));
                }
            }
        }
    };
    let (Some(number), Some(node_id), Some(html_url)) = (
        pull["number"].as_i64(),
        pull["node_id"].as_str(),
        pull["html_url"].as_str(),
    ) else {
        return Err(Stop::Message("unreadable candidate pull request".into()));
    };
    if number <= 0
        || node_id.is_empty()
        || node_id.len() > 256
        || html_url.len() > 2048
        || !valid_pull_url(html_url)
        || pull["head"]["ref"] != manifest.branch
        || pull["base"]["ref"] != manifest.base
        || (pull["head"]["sha"] != manifest.commit
            && !(recovery
                && pull["body"]
                    .as_str()
                    .is_some_and(|body| body.lines().any(|line| line == marker))))
    {
        return Err(Stop::Message(
            "unreadable candidate pull request identity".into(),
        ));
    }
    record(state, &publication.id, Update::BranchCreated).await?;
    record(
        state,
        &publication.id,
        Update::PullRequest {
            number,
            node_id: node_id.into(),
            html_url: html_url.into(),
        },
    )
    .await?;
    if api
        .ref_sha(&token, repository, &manifest.base)
        .await?
        .as_deref()
        != Some(&candidate.subject.baseline_commit)
        || api
            .ref_sha(&token, repository, &manifest.branch)
            .await?
            .as_deref()
            != Some(&manifest.commit)
    {
        return Err(Stop::Message(
            "historical PR receipt: source/target changed after effect".into(),
        ));
    }
    Ok(())
}
