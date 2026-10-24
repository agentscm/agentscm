//! Supported skills on the hub (Experimental): the library view, repository adoption, and
//! self-reported evidence recorded per run, over HTTP and MCP.
//!
//! A run is one (repository, branch, skill). Evidence is what an agent says it did; the hub
//! labels it `self_reported` and only checks the ordering rules in
//! `skill_workflow::supported::admit`. Adoption comes from the lock file on the forge's default
//! branch when the repository is connected, else from what a checkout reported.

use std::collections::BTreeMap;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, put};
use serde::Deserialize;
use serde_json::{Value, json};

use super::connection::ConnectedRepository;
use super::forge_api::ForgeApi;
use super::github::GhError;
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::mcp::{hub_failure, invalid_args, try_args};
use super::store::{Store, new_id, unix_now};
use super::{HubError, HubState, Principal, Result};
use crate::skill_workflow::supported::{
    self, AdoptionStatus, Assurance, Detail, Evidence, EvidenceKind, GateState, PinStatus,
    RunIdentity, RunState, RunView, SkillLock, StoredEvidence,
};
use crate::store::digest;

/// Most evidence rows one run keeps; more is refused with `run_full`.
pub const MAX_RUN_EVIDENCE: usize = 256;
pub const MAX_RUNS: usize = 100;
const DEFAULT_RUNS: usize = 50;
/// The forge lock file is read at most this often per repository.
pub const READ_EVERY_SECONDS: i64 = 300;
const MAX_LOCK_BYTES: usize = 64 * 1024;

// ---- stored shapes --------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillAdoption {
    pub repository: String,
    /// `default_branch` or `reported_by_checkout`.
    pub source: String,
    /// The lock file in canonical JSON.
    pub lock_json: String,
    pub default_branch: Option<String>,
    pub observed_at: i64,
    pub reported_by: Option<String>,
}

/// One evidence row as the tables hold it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceRow {
    pub id: String,
    pub repository: String,
    pub branch: String,
    pub run_id: String,
    pub skill: String,
    pub skill_sha256: String,
    pub gate: String,
    pub kind: String,
    pub head_sha: String,
    pub assurance: String,
    pub summary: String,
    pub url: Option<String>,
    pub pull_request: Option<i64>,
    pub detail: Option<String>,
    pub session: String,
    pub agent: String,
    pub recorded_at: i64,
}

/// Which run: the latest row of each (repository, run).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunHead {
    pub run_id: String,
    pub branch: String,
    pub skill: String,
    pub skill_sha256: String,
}

fn word<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn unword<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    serde_json::from_value(Value::String(text.to_owned())).ok()
}

impl EvidenceRow {
    fn from_stored(
        repository: &str,
        run_id: &str,
        branch: &str,
        skill: &str,
        skill_sha256: &str,
        s: &StoredEvidence,
    ) -> Self {
        let e = &s.evidence;
        Self {
            id: s.id.clone(),
            repository: repository.to_owned(),
            branch: branch.to_owned(),
            run_id: run_id.to_owned(),
            skill: skill.to_owned(),
            skill_sha256: skill_sha256.to_owned(),
            gate: e.gate.clone(),
            kind: word(&e.kind),
            head_sha: e.head_sha.clone(),
            assurance: word(&e.assurance),
            summary: e.summary.clone(),
            url: e.url.clone(),
            pull_request: e.pull_request.and_then(|n| i64::try_from(n).ok()),
            detail: e
                .detail
                .as_ref()
                .and_then(|d| serde_json::to_string(d).ok()),
            session: s.session.clone(),
            agent: s.agent.clone(),
            recorded_at: s.recorded_at as i64,
        }
    }

    fn to_stored(&self) -> Option<StoredEvidence> {
        Some(StoredEvidence {
            id: self.id.clone(),
            session: self.session.clone(),
            agent: self.agent.clone(),
            recorded_at: u64::try_from(self.recorded_at).ok()?,
            evidence: Evidence {
                skill: Some(self.skill.clone()),
                gate: self.gate.clone(),
                kind: unword(&self.kind)?,
                head_sha: self.head_sha.clone(),
                assurance: unword(&self.assurance)?,
                summary: self.summary.clone(),
                url: self.url.clone(),
                branch: Some(self.branch.clone()),
                pull_request: self.pull_request.and_then(|n| u64::try_from(n).ok()),
                detail: match &self.detail {
                    Some(text) => Some(serde_json::from_str::<Detail>(text).ok()?),
                    None => None,
                },
            },
        })
    }
}

// ---- failures -------------------------------------------------------------------------------

/// Why a request failed: a store/transport error, or a domain refusal with its own code.
pub enum Failure {
    Hub(HubError),
    Refused(StatusCode, &'static str, String),
}

impl From<HubError> for Failure {
    fn from(error: HubError) -> Self {
        Self::Hub(error)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Self::Hub(error) => ApiError::from(error).into_response(),
            Self::Refused(status, code, message) => {
                (status, Json(json!({"error": code, "message": message}))).into_response()
            }
        }
    }
}

type Out<T> = std::result::Result<T, Failure>;

fn conflict(code: &'static str, message: impl Into<String>) -> Failure {
    Failure::Refused(StatusCode::CONFLICT, code, message.into())
}

fn bad_request(message: impl Into<String>) -> Failure {
    Failure::Refused(StatusCode::BAD_REQUEST, "invalid_request", message.into())
}

fn not_found() -> Failure {
    Failure::Refused(StatusCode::NOT_FOUND, "not_found", "no such run".to_owned())
}

// ---- domain ---------------------------------------------------------------------------------

pub fn run_id_of(repo: &str, branch: &str, skill: &str) -> String {
    let full = digest(format!("{repo}\0{branch}\0{skill}").as_bytes());
    format!("run_{}", &full[..16])
}

fn pins_of(adoption: Option<&SkillAdoption>) -> Vec<PinStatus> {
    adoption
        .and_then(|a| SkillLock::parse(a.lock_json.as_bytes()).ok())
        .map(|lock| supported::resolve(&lock))
        .unwrap_or_default()
}

fn adopted_pin<'a>(pins: &'a [PinStatus], name: &str) -> Option<&'a PinStatus> {
    pins.iter()
        .find(|p| p.pin.name == name && p.status == AdoptionStatus::Adopted)
}

/// The run as it stands, or None when its skill is no longer in this binary's catalog.
fn run_view(
    store: &dyn Store,
    repo: &str,
    head: &RunHead,
    pins: &[PinStatus],
) -> Result<Option<RunView>> {
    let Some(entry) = supported::catalog_entry_by_sha256(&head.skill_sha256) else {
        return Ok(None);
    };
    let stored: Vec<StoredEvidence> = store
        .skill_evidence(repo, &head.run_id)?
        .iter()
        .filter_map(EvidenceRow::to_stored)
        .collect();
    let overrides = adopted_pin(pins, &head.skill)
        .filter(|p| p.pin.sha256 == head.skill_sha256)
        .and_then(|p| p.pin.params.as_ref());
    let params = entry
        .manifest
        .effective_params(overrides)
        .or_else(|_| entry.manifest.effective_params(None))
        .unwrap_or_default();
    let identity = RunIdentity {
        run_id: head.run_id.clone(),
        repo: repo.to_owned(),
        branch: head.branch.clone(),
        pull_request: None,
        skill_sha256: head.skill_sha256.clone(),
        params,
        updated_at: 0,
    };
    Ok(Some(supported::evaluate(
        &entry.manifest,
        &identity,
        &stored,
    )))
}

fn views(
    store: &dyn Store,
    repo: &str,
    branch: Option<&str>,
    run_id: Option<&str>,
    limit: usize,
) -> Result<Vec<RunView>> {
    let pins = pins_of(store.skill_adoption(repo)?.as_ref());
    let mut out = Vec::new();
    for head in store.skill_run_heads(repo, branch, run_id, limit)? {
        out.extend(run_view(store, repo, &head, &pins)?);
    }
    Ok(out)
}

fn enforce_stop(view: &RunView) -> bool {
    view.current_gate.as_deref().is_some_and(|id| {
        view.gates
            .iter()
            .any(|g| g.id == id && g.enforce.contains(&supported::Enforce::Stop))
    })
}

fn library(store: &dyn Store, repo: Option<&str>) -> Result<Value> {
    let catalog = supported::catalog();
    let mut adoption = json!({"repo": repo.unwrap_or(""), "source": "none", "observed_at": null,
        "default_branch": null, "pins": []});
    let mut counts = BTreeMap::new();
    if let Some(repo) = repo {
        let stored = store.skill_adoption(repo)?;
        if let Some(a) = &stored {
            adoption["source"] = json!(a.source);
            adoption["observed_at"] = json!(a.observed_at);
            adoption["default_branch"] = json!(a.default_branch);
        }
        adoption["pins"] = json!(pins_of(stored.as_ref()));
        for view in views(store, repo, None, None, MAX_RUNS)? {
            let c = counts
                .entry(view.skill.name.clone())
                .or_insert_with(|| json!({"open": 0, "at_stop_gate": 0, "completed": 0}));
            let bump = |c: &mut Value, key: &str| c[key] = json!(c[key].as_u64().unwrap_or(0) + 1);
            match view.state {
                RunState::Completed => bump(c, "completed"),
                RunState::Open => {
                    bump(c, "open");
                    if enforce_stop(&view) {
                        bump(c, "at_stop_gate");
                    }
                }
                RunState::Abandoned => {}
            }
        }
    }
    Ok(json!({"catalog": catalog, "adoption": adoption, "counts": counts}))
}

/// Stores a lock a checkout reported; the forge's own reading, when there is one, still wins.
fn report_adoption(store: &dyn Store, repo: &str, token: &str, body: &[u8]) -> Out<Value> {
    let lock =
        SkillLock::parse(body).map_err(|e| bad_request(format!("invalid lock file: {e}")))?;
    let lock_json = serde_json::to_string(&lock).map_err(|_| bad_request("unserializable lock"))?;
    let applied = store.skill_adoption_put(&SkillAdoption {
        repository: repo.to_owned(),
        source: "reported_by_checkout".into(),
        lock_json: lock_json.clone(),
        default_branch: None,
        observed_at: unix_now(),
        reported_by: Some(token.to_owned()),
    })?;
    Ok(json!({
        "source": applied.source,
        "applied": applied.lock_json == lock_json && applied.source == "reported_by_checkout",
        "observed_at": applied.observed_at,
        "default_branch": applied.default_branch,
        "pins": pins_of(Some(&applied)),
    }))
}

struct Recorder<'a> {
    repo: &'a str,
    token: &'a str,
    agent: &'a str,
    session: Option<&'a str>,
}

fn record(store: &dyn Store, who: Recorder<'_>, mut evidence: Evidence) -> Out<RunView> {
    let skill = evidence
        .skill
        .clone()
        .ok_or_else(|| bad_request("skill is required"))?;
    let branch = evidence
        .branch
        .clone()
        .filter(|b| !b.is_empty())
        .ok_or_else(|| bad_request("branch is required"))?;
    let pins = pins_of(store.skill_adoption(who.repo)?.as_ref());
    let pin = adopted_pin(&pins, &skill).ok_or_else(|| {
        conflict(
            "not_adopted",
            format!("skill {skill} is not adopted in this repository"),
        )
    })?;
    let entry = supported::catalog_entry_by_sha256(&pin.pin.sha256)
        .ok_or_else(|| conflict("not_adopted", format!("skill {skill} is not available")))?;
    let now = unix_now();
    let session = store.session_name_for(who.token, who.session, now)?;
    let run_id = run_id_of(who.repo, &branch, &skill);
    let existing: Vec<StoredEvidence> = store
        .skill_evidence(who.repo, &run_id)?
        .iter()
        .filter_map(EvidenceRow::to_stored)
        .collect();
    if existing.len() >= MAX_RUN_EVIDENCE {
        return Err(conflict(
            "run_full",
            format!("a run keeps at most {MAX_RUN_EVIDENCE} evidence records"),
        ));
    }
    supported::admit(&entry.manifest, &existing, &evidence, &session).map_err(|r| {
        if r.code == "invalid" {
            bad_request(r.message)
        } else {
            conflict(r.code, r.message)
        }
    })?;
    evidence.skill = Some(skill.clone());
    let stored = StoredEvidence {
        id: new_id("evd")?,
        session,
        agent: who.agent.to_owned(),
        recorded_at: now as u64,
        evidence,
    };
    store.skill_evidence_add(&EvidenceRow::from_stored(
        who.repo,
        &run_id,
        &branch,
        &skill,
        &entry.sha256,
        &stored,
    ))?;
    let head = RunHead {
        run_id,
        branch,
        skill,
        skill_sha256: entry.sha256.clone(),
    };
    run_view(store, who.repo, &head, &pins)?.ok_or_else(not_found)
}

// ---- forge read -----------------------------------------------------------------------------

/// Reads the lock file from the repository's default branch and makes it the adoption; no file
/// (or an unreadable one) clears an earlier forge adoption. Errors are logged, never fatal: a
/// skills read must not fail a repository sync.
pub(super) async fn sync_adoption(
    state: &HubState,
    api: ForgeApi<'_>,
    repo: &ConnectedRepository,
    token: &str,
) {
    let read = async {
        let branch = api.default_branch(token, repo).await?;
        let file = api
            .file(token, repo, supported::LOCK_PATH, &branch, MAX_LOCK_BYTES)
            .await?;
        Ok::<_, GhError>((branch, file))
    }
    .await;
    let (branch, file) = match read {
        Ok(read) => read,
        Err(error) => {
            eprintln!("hub: skills lock read for {}: {error:?}", repo.full_name);
            return;
        }
    };
    let lock = file.and_then(|bytes| SkillLock::parse(&bytes).ok());
    let (store, name) = (state.store.clone(), repo.full_name.clone());
    let done = blocking(state, move || match lock {
        Some(lock) => store
            .skill_adoption_put(&SkillAdoption {
                repository: name,
                source: "default_branch".into(),
                lock_json: serde_json::to_string(&lock)
                    .map_err(|_| HubError::Invalid("lock".into()))?,
                default_branch: Some(branch),
                observed_at: unix_now(),
                reported_by: None,
            })
            .map(|_| ()),
        None => store.skill_adoption_clear_forge(&name),
    })
    .await;
    if let Err(error) = done {
        eprintln!("hub: skills adoption store for {}: {error}", repo.full_name);
    }
}

// ---- HTTP -----------------------------------------------------------------------------------

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/skills", get(library_route))
        .route(
            "/v1/hub/repos/{repo}/skills/adoption",
            put(adoption_route).layer(DefaultBodyLimit::max(300 * 1024)),
        )
        .route("/v1/hub/repos/{repo}/skills/runs", get(runs_route))
        .route(
            "/v1/hub/repos/{repo}/skills/runs/by-branch",
            get(by_branch_route),
        )
        .route("/v1/hub/repos/{repo}/skills/runs/{run_id}", get(run_route))
        .route(
            "/v1/hub/repos/{repo}/skills/evidence",
            axum::routing::post(evidence_route),
        )
}

fn scope(principal: &Principal, repo: &str) -> std::result::Result<(), Failure> {
    if principal.repository != repo {
        return Err(Failure::Hub(HubError::Unauthorized));
    }
    Ok(())
}

#[derive(Deserialize)]
struct LibraryQuery {
    repo: Option<String>,
}

async fn library_route(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Query(query): Query<LibraryQuery>,
) -> Out<Json<Value>> {
    if let Some(repo) = &query.repo {
        scope(&principal, repo)?;
    }
    let store = state.store.clone();
    let value = blocking(&state, move || library(&*store, query.repo.as_deref())).await?;
    Ok(Json(value))
}

async fn adoption_route(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(repo): Path<String>,
    body: Bytes,
) -> Out<Json<Value>> {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    let token = principal.token_id;
    blocking(&state, move || {
        Ok(report_adoption(&*store, &repo, &token, &body))
    })
    .await?
    .map(Json)
}

#[derive(Deserialize)]
struct RunsQuery {
    state: Option<String>,
    limit: Option<usize>,
}

async fn runs_route(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
    Query(query): Query<RunsQuery>,
) -> Out<Json<Value>> {
    scope(&principal, &repo)?;
    let want = match query.state.as_deref().unwrap_or("open") {
        "open" => Some(RunState::Open),
        "completed" => Some(RunState::Completed),
        "all" => None,
        _ => return Err(bad_request("state is open, completed or all")),
    };
    let limit = query.limit.unwrap_or(DEFAULT_RUNS).clamp(1, MAX_RUNS);
    let store = state.store.clone();
    // Filtering happens after evaluation, so scan the newest MAX_RUNS runs.
    let all = blocking(&state, move || views(&*store, &repo, None, None, MAX_RUNS)).await?;
    let mut runs: Vec<RunView> = all
        .into_iter()
        .filter(|r| want.is_none_or(|w| r.state == w))
        .collect();
    let truncated = runs.len() > limit;
    runs.truncate(limit);
    Ok(Json(json!({"runs": runs, "truncated": truncated})))
}

#[derive(Deserialize)]
struct BranchQuery {
    branch: String,
}

async fn by_branch_route(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
    Query(query): Query<BranchQuery>,
) -> Out<Json<RunView>> {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    let mut found = blocking(&state, move || {
        views(&*store, &repo, Some(&query.branch), None, 1)
    })
    .await?;
    found.pop().map(Json).ok_or_else(not_found)
}

async fn run_route(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path((repo, run_id)): Path<(String, String)>,
) -> Out<Json<RunView>> {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    let mut found = blocking(&state, move || {
        views(&*store, &repo, None, Some(&run_id), 1)
    })
    .await?;
    found.pop().map(Json).ok_or_else(not_found)
}

#[derive(Deserialize)]
struct EvidenceRequest {
    #[serde(flatten)]
    evidence: Evidence,
    #[serde(default)]
    session: Option<String>,
}

async fn evidence_route(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(repo): Path<String>,
    JsonBody(request): JsonBody<EvidenceRequest>,
) -> Out<Json<RunView>> {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    let view = blocking(&state, move || {
        Ok(record(
            &*store,
            Recorder {
                repo: &repo,
                token: &principal.token_id,
                agent: &principal.name,
                session: request.session.as_deref(),
            },
            request.evidence,
        ))
    })
    .await??;
    Ok(Json(view))
}

// ---- MCP ------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    repo: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    repo: String,
    branch: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceArgs {
    repo: String,
    skill: String,
    gate: String,
    kind: String,
    head_sha: String,
    summary: String,
    url: Option<String>,
    branch: String,
    pull_request: Option<u64>,
    detail: Option<Detail>,
    session: Option<String>,
}

fn tool_result(text: String, structured: Value, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "structuredContent": structured,
        "isError": is_error})
}

fn refusal_result(failure: Failure) -> Value {
    match failure {
        Failure::Hub(error) => hub_failure(&error),
        Failure::Refused(_, code, message) => tool_result(
            format!("AgentSCM: {message} ({code})"),
            json!({"error": code, "message": message}),
            true,
        ),
    }
}

fn gate_line(gate: &supported::GateView) -> String {
    let state = match gate.state {
        GateState::Done => "done",
        GateState::Current => "current",
        GateState::NotReached => "not reached",
    };
    let label = if gate.assurance == Some(Assurance::SelfReported) {
        " [self-reported]"
    } else {
        ""
    };
    format!("- {}: {state}{label}", gate.id)
}

pub fn render_run(view: &RunView) -> String {
    let pr = view
        .pull_request
        .map_or(String::new(), |n| format!(" (PR #{n})"));
    let head: String = view.head_sha.chars().take(7).collect();
    let mut lines = vec![format!(
        "{} v{} on {}{pr}, head {head}: {}",
        view.skill.name,
        view.skill.workflow_version,
        view.branch,
        word(&view.state),
    )];
    lines.extend(view.gates.iter().map(gate_line));
    lines.push(format!("Next: {}", view.next_action));
    lines.push(if view.blocks_finish {
        "Finishing is blocked until this gate has evidence.".into()
    } else {
        "Finishing is not blocked.".into()
    });
    lines.join("\n")
}

fn render_library(lib: &Value, pins: &[PinStatus]) -> String {
    let list = |v: &Value| {
        let items: Vec<&str> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if items.is_empty() {
            "none".to_owned()
        } else {
            items.join("; ")
        }
    };
    let mut lines = vec![format!(
        "Adoption source: {}",
        lib["adoption"]["source"].as_str().unwrap_or("none")
    )];
    for entry in lib["catalog"].as_array().into_iter().flatten() {
        let name = entry["name"].as_str().unwrap_or("");
        let sha = entry["sha256"].as_str().unwrap_or("");
        let pin = pins.iter().find(|p| {
            p.pin.name == name && p.pin.sha256 == sha && p.status == AdoptionStatus::Adopted
        });
        lines.push(format!(
            "{name} v{} ({}): {}",
            entry["workflow_version"],
            if pin.is_some() {
                "adopted"
            } else {
                "available"
            },
            entry["summary"].as_str().unwrap_or("")
        ));
        let a = &entry["applicability"];
        lines.push(format!("  Follow when: {}", list(&a["triggers"])));
        lines.push(format!("  Prerequisites: {}", list(&a["prerequisites"])));
        lines.push(format!("  Not for: {}", list(&a["not_for"])));
        if let Some(pin) = pin {
            let params = supported::catalog_entry_by_sha256(sha)
                .and_then(|e| e.manifest.effective_params(pin.pin.params.as_ref()).ok())
                .unwrap_or_default();
            let text: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
            lines.push(format!("  Parameters: {}", text.join(", ")));
        }
    }
    for p in pins.iter().filter(|p| p.status != AdoptionStatus::Adopted) {
        lines.push(format!(
            "{} v{} is pinned but not usable: {}",
            p.pin.name,
            p.pin.workflow_version,
            word(&p.status)
        ));
    }
    lines.join("\n")
}

/// Whether `name` is one of the skill tools.
pub(super) fn is_tool(name: &str) -> bool {
    matches!(
        name,
        "agentscm_skill_list" | "agentscm_skill_status" | "agentscm_skill_evidence"
    )
}

fn other_repository() -> Value {
    tool_result(
        "AgentSCM: this token is for another repository.".into(),
        json!({"error": "other_repository"}),
        true,
    )
}

/// Runs a skill tool for `principal`; `arguments` is the raw tool arguments.
pub(super) async fn mcp_call(
    state: &HubState,
    principal: &Principal,
    name: &str,
    arguments: Option<&Value>,
) -> Value {
    let store = state.store.clone();
    match name {
        "agentscm_skill_list" => {
            let ListArgs { repo } = match try_args(arguments) {
                Ok(a) => a,
                Err(e) => return invalid_args(name, &e),
            };
            if repo != principal.repository {
                return other_repository();
            }
            let read = blocking(state, move || {
                let pins = pins_of(store.skill_adoption(&repo)?.as_ref());
                Ok((library(&*store, Some(&repo))?, pins))
            })
            .await;
            match read {
                Ok((lib, pins)) => tool_result(render_library(&lib, &pins), lib, false),
                Err(e) => hub_failure(&e),
            }
        }
        "agentscm_skill_status" => {
            let StatusArgs { repo, branch } = match try_args(arguments) {
                Ok(a) => a,
                Err(e) => return invalid_args(name, &e),
            };
            if repo != principal.repository {
                return other_repository();
            }
            let found = blocking(state, move || {
                views(&*store, &repo, Some(&branch), None, MAX_RUNS)
            })
            .await;
            match found {
                Ok(runs) if runs.is_empty() => tool_result(
                    "No skill run exists for this branch yet.\nNext: record the first gate with agentscm_skill_evidence (see agentscm_skill_list for the adopted skills).\nFinishing is not blocked by a run.".into(),
                    json!({"runs": []}),
                    false,
                ),
                Ok(runs) => tool_result(
                    runs.iter().map(render_run).collect::<Vec<_>>().join("\n\n"),
                    json!({"runs": runs}),
                    false,
                ),
                Err(e) => hub_failure(&e),
            }
        }
        _ => {
            let a: EvidenceArgs = match try_args(arguments) {
                Ok(a) => a,
                Err(e) => return invalid_args(name, &e),
            };
            if a.repo != principal.repository {
                return other_repository();
            }
            let Some(kind) = unword::<EvidenceKind>(&a.kind) else {
                return invalid_args(name, "kind is not a known evidence kind");
            };
            let evidence = Evidence {
                skill: Some(a.skill),
                gate: a.gate,
                kind,
                head_sha: a.head_sha,
                assurance: Assurance::SelfReported,
                summary: a.summary,
                url: a.url,
                branch: Some(a.branch),
                pull_request: a.pull_request,
                detail: a.detail,
            };
            let (token, agent, repo, session) = (
                principal.token_id.clone(),
                principal.name.clone(),
                a.repo,
                a.session,
            );
            let out = blocking(state, move || {
                Ok(record(
                    &*store,
                    Recorder {
                        repo: &repo,
                        token: &token,
                        agent: &agent,
                        session: session.as_deref(),
                    },
                    evidence,
                ))
            })
            .await;
            match out {
                Ok(Ok(view)) => tool_result(render_run(&view), json!({"run": view}), false),
                Ok(Err(failure)) => refusal_result(failure),
                Err(e) => hub_failure(&e),
            }
        }
    }
}
