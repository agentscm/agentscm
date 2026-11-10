//! Supported skills on the hub (Experimental): the library view, repository adoption, and
//! evidence recorded per run, over HTTP and MCP.
//!
//! A run is one (repository, branch, skill). Evidence an agent records is labelled
//! `self_reported`; the hub only checks the ordering rules in `skill_workflow::supported::admit`.
//! On a GitHub connection the hub also records what it reads from the forge (green checks, an
//! approval, the merge) as `observed`, and, where the operator turns it on, posts the run as the
//! `agentscm/skill/<name>` commit status. Adoption comes from the lock file on the forge's default
//! branch when the repository is connected, else from what a checkout reported (a connected
//! repository refuses checkout reports).

use std::collections::{BTreeMap, BTreeSet};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, put};
use serde::Deserialize;
use serde_json::{Value, json};

use super::connection::{ConnectedRepository, Connection};
use super::forge_api::ForgeApi;
use super::github::{CommitStatus, GhError, StatusState};
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::login::HumanSession;
use super::mcp::{hub_failure, invalid_args, try_args};
use super::store::{Store, new_id, unix_now};
use super::viewer::Viewer;
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
    /// The `repo_local` skills' bytes (JSON `[LocalBundle]`), read with the lock.
    pub local_skills: Option<String>,
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
    /// The recording token's id; with the session it decides review independence.
    pub token_id: String,
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
            token_id: s.token_id.clone(),
            session: s.session.clone(),
            agent: s.agent.clone(),
            recorded_at: s.recorded_at as i64,
        }
    }

    fn to_stored(&self) -> Option<StoredEvidence> {
        Some(StoredEvidence {
            id: self.id.clone(),
            token_id: self.token_id.clone(),
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
            Self::Refused(status, code, message) => ApiError::new(status, code)
                .with_message(message)
                .into_response(),
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

/// A repository's resolved pins with the repository skills they may name.
#[derive(Default)]
struct Adopted {
    pins: Vec<PinStatus>,
    local: Vec<supported::LocalSkill>,
}

impl Adopted {
    /// The embedded or repository skill a pin or run names by digest.
    fn entry(&self, sha256: &str) -> Option<&supported::CatalogEntry> {
        supported::entry_for(sha256, &self.local)
    }
}

fn local_of(adoption: &SkillAdoption) -> Vec<supported::LocalSkill> {
    adoption
        .local_skills
        .as_deref()
        .and_then(|text| serde_json::from_str::<Vec<supported::LocalBundle>>(text).ok())
        .map(|bundles| supported::local_skills(&bundles))
        .unwrap_or_default()
}

fn pins_of(adoption: Option<&SkillAdoption>) -> Adopted {
    let Some(adoption) = adoption else {
        return Adopted::default();
    };
    let local = local_of(adoption);
    let pins = SkillLock::parse(adoption.lock_json.as_bytes())
        .map(|lock| supported::resolve_with(&lock, &local))
        .unwrap_or_default();
    Adopted { pins, local }
}

/// Pins as JSON; an adopted pin also carries its walkthrough with the pin's own settings.
fn pins_json(pins: &Adopted) -> Value {
    let rows: Vec<Value> = pins
        .pins
        .iter()
        .map(|p| {
            let mut v = json!(p);
            if let (AdoptionStatus::Adopted, Some(e)) = (&p.status, pins.entry(&p.pin.sha256))
                && let Ok(params) = e.manifest.effective_params(p.pin.params.as_ref())
            {
                v["walkthrough"] = json!(supported::walkthrough(&e.manifest, Some(&params)));
            }
            v
        })
        .collect();
    json!(rows)
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
    pins: &Adopted,
) -> Result<Option<RunView>> {
    let Some(entry) = pins.entry(&head.skill_sha256) else {
        return Ok(None);
    };
    let stored: Vec<StoredEvidence> = store
        .skill_evidence(repo, &head.run_id)?
        .iter()
        .filter_map(EvidenceRow::to_stored)
        .collect();
    let overrides = adopted_pin(&pins.pins, &head.skill)
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

pub(super) fn views(
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

/// Runs as the owner `viewer` may see them: evidence another owner's token recorded names its
/// session by digest and its label masked.
fn shown_runs(store: &dyn Store, viewer: &str, runs: &mut [RunView]) -> Result<()> {
    let mine: BTreeSet<String> = store
        .list_agent_tokens(viewer)?
        .into_iter()
        .map(|token| token.id)
        .collect();
    for run in runs {
        let gates = run.gates.iter_mut().flat_map(|gate| &mut gate.evidence);
        for e in gates.chain(&mut run.stale_evidence) {
            if e.evidence.assurance == Assurance::Observed {
                continue; // recorded by the hub from the forge, not by an agent token
            }
            let owner = mine.contains(&e.token_id).then_some(viewer);
            Viewer(viewer).agent(owner, &e.token_id, &mut e.agent, &mut e.session);
        }
    }
    Ok(())
}

fn enforce_stop(view: &RunView) -> bool {
    view.current_gate.as_deref().is_some_and(|id| {
        view.gates
            .iter()
            .any(|g| g.id == id && g.enforce.contains(&supported::Enforce::Stop))
    })
}

fn library(store: &dyn Store, repo: Option<&str>) -> Result<Value> {
    let mut catalog = json!(supported::catalog());
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
        let pins = pins_of(stored.as_ref());
        adoption["pins"] = pins_json(&pins);
        // This repository's own skills join the embedded ones, marked by `first_party: false`.
        if let Some(rows) = catalog.as_array_mut() {
            let adopted: BTreeSet<&str> = pins
                .pins
                .iter()
                .filter(|p| p.status == AdoptionStatus::Adopted)
                .map(|p| p.pin.sha256.as_str())
                .collect();
            let local = pins
                .local
                .iter()
                .filter(|l| adopted.contains(l.entry.sha256.as_str()));
            rows.extend(local.map(|l| {
                let mut row = json!(l.entry);
                row["path"] = json!(l.path);
                row
            }));
        }
        for view in views(store, repo, None, None, MAX_RUNS)? {
            let c = counts.entry(view.skill.name.clone()).or_insert_with(|| {
                json!({"open": 0, "at_stop_gate": 0, "completed": 0, "merged_observed": 0,
                        "merged_with_skips": 0, "merged_unobserved": 0})
            });
            let bump = |c: &mut Value, key: &str| c[key] = json!(c[key].as_u64().unwrap_or(0) + 1);
            if let Some(check) = &view.merge_check {
                bump(c, "merged_observed");
                if check.skipped_any() {
                    bump(c, "merged_with_skips");
                }
                if !check.unobserved.is_empty() {
                    bump(c, "merged_unobserved");
                }
            }
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

/// Most bytes the stored repository skills take, as the `local_skills` column allows.
const MAX_LOCAL_JSON: usize = 2 * 1024 * 1024;

/// The stored form of repository skills: only the bundles that validate and that a
/// `repo_local` pin names, at most `MAX_LOCAL_SKILLS` and `MAX_LOCAL_JSON` bytes once encoded
/// (the last bundles are left out, so their pins are unknown), or `None` when there are none.
fn stored_local(lock: &SkillLock, bundles: Vec<supported::LocalBundle>) -> Option<String> {
    let wanted: BTreeSet<&str> = lock
        .skills
        .iter()
        .filter_map(|p| p.source.repo_local_path())
        .collect();
    let mut kept: Vec<supported::LocalBundle> = bundles
        .into_iter()
        .filter(|b| wanted.contains(b.path.as_str()) && b.skill().is_ok())
        .take(supported::MAX_LOCAL_SKILLS)
        .collect();
    // Escaping can grow the text; never hand the store a value its bound refuses.
    loop {
        let text = serde_json::to_string(&kept).ok()?;
        if kept.is_empty() {
            return None;
        }
        if text.len() <= MAX_LOCAL_JSON {
            return Some(text);
        }
        kept.pop();
    }
}

/// Stores a lock a checkout reported; the forge's own reading, when there is one, still wins.
fn report_adoption(store: &dyn Store, repo: &str, token: &str, body: &[u8]) -> Out<Value> {
    let lock =
        SkillLock::parse(body).map_err(|e| bad_request(format!("invalid lock file: {e}")))?;
    // A checkout's files are whatever its branch says: a repository skill's definition and
    // procedure come only from the default branch, read by the Hub itself.
    if lock
        .skills
        .iter()
        .any(|p| p.source.repo_local_path().is_some())
    {
        return Err(bad_request(
            "repository (repo_local) skills need a forge connection: the Hub reads them from the default branch",
        ));
    }
    let lock_json = serde_json::to_string(&lock).map_err(|_| bad_request("unserializable lock"))?;
    let applied = store.skill_adoption_put(&SkillAdoption {
        repository: repo.to_owned(),
        source: "reported_by_checkout".into(),
        lock_json: lock_json.clone(),
        default_branch: None,
        observed_at: unix_now(),
        reported_by: Some(token.to_owned()),
        local_skills: None,
    })?;
    Ok(json!({
        "source": applied.source,
        "applied": applied.lock_json == lock_json && applied.source == "reported_by_checkout",
        "observed_at": applied.observed_at,
        "default_branch": applied.default_branch,
        "pins": pins_of(Some(&applied)).pins,
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
    let pin = adopted_pin(&pins.pins, &skill).ok_or_else(|| {
        conflict(
            "not_adopted",
            format!("skill {skill} is not adopted in this repository"),
        )
    })?;
    let entry = pins
        .entry(&pin.pin.sha256)
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
    let admitted = supported::admit(
        &entry.manifest,
        &existing,
        &evidence,
        &supported::Who {
            token: who.token,
            session: &session,
        },
    )
    .map_err(|r| {
        if r.code == "not_independent" {
            let policy = super::value_metrics::identity(&[&serde_json::to_string(&pin.pin.params).unwrap_or_default()]);
            let (work_id,generation)=value_work_binding(store,who.token,&session,&evidence.head_sha,now);
            let observation = super::value_metrics::ValueObservation {work_id,generation,
                identity: super::value_metrics::identity(&[&run_id, &evidence.gate, &evidence.head_sha, &entry.sha256, &policy, "not_independent"]),
                kind:"issue".into(), state:"unresolved".into(), branch:branch.clone(),
                source:evidence.head_sha.clone(),recipe:entry.sha256.clone(),policy,
                gate:evidence.gate.clone(),evidence_id:None,
                reason:"Review independence could not be established for this exact source and configured skill".into(),
            };
            super::value_metrics::observe(store,who.token,&observation,now);
        }
        if r.code == "invalid" {
            bad_request(r.message)
        } else {
            conflict(r.code, r.message)
        }
    });
    admitted?;
    evidence.skill = Some(skill.clone());
    let stored = StoredEvidence {
        id: new_id("evd")?,
        token_id: who.token.to_owned(),
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
    let view = run_view(store, who.repo, &head, &pins)?.ok_or_else(not_found)?;
    record_value_followups(store, who.token, &view, &stored, &pin.pin.params, now);
    Ok(view)
}

fn value_work_binding(
    store: &dyn Store,
    token: &str,
    session: &str,
    source: &str,
    now: i64,
) -> (Option<String>, Option<i64>) {
    let item = store
        .capture_work_for_session(token, session, now)
        .ok()
        .flatten();
    match item.filter(|w| {
        w.current.as_ref().is_some_and(|c| {
            c.source_refs
                .iter()
                .any(|r| matches!(r,super::work::SourceRef::Commit{oid,..} if oid==source))
        })
    }) {
        Some(item) => (Some(item.id), Some(item.generation)),
        None => (None, None),
    }
}

fn record_value_followups(
    store: &dyn Store,
    token: &str,
    view: &RunView,
    stored: &StoredEvidence,
    params: &Option<supported::Params>,
    now: i64,
) {
    let policy =
        super::value_metrics::identity(&[&serde_json::to_string(params).unwrap_or_default()]);
    for gate in &view.gates {
        let (work_id, generation) =
            value_work_binding(store, token, &stored.session, &view.head_sha, now);
        let observation=super::value_metrics::ValueObservation {work_id,generation,
            identity:super::value_metrics::identity(&[&view.run_id,&gate.id,&view.head_sha,&view.skill.sha256,&policy]),
            kind:"followup".into(),state:if gate.state==GateState::Done {"current"} else {"pending"}.into(),
            branch:view.branch.clone(),source:view.head_sha.clone(),recipe:view.skill.sha256.clone(),policy:policy.clone(),
            gate:gate.id.clone(),evidence_id:gate.evidence.last().map(|e|e.id.clone()),
            reason:"Configured supported-skill gate; evidence is self-reported, not independent verification".into(),
        };
        super::value_metrics::observe(store, token, &observation, now);
    }
    if stored.evidence.kind == EvidenceKind::IndependentReview {
        let (work_id, generation) = value_work_binding(
            store,
            token,
            &stored.session,
            &stored.evidence.head_sha,
            now,
        );
        let observation=super::value_metrics::ValueObservation {work_id,generation,
            identity:super::value_metrics::identity(&[&view.run_id,&stored.evidence.gate,&stored.evidence.head_sha,&view.skill.sha256,&policy,"not_independent"]),
            kind:"issue".into(),state:"resolved".into(),branch:view.branch.clone(),source:stored.evidence.head_sha.clone(),
            recipe:view.skill.sha256.clone(),policy,gate:stored.evidence.gate.clone(),evidence_id:Some(stored.id.clone()),
            reason:"A subsequent admitted independent review reports this same source and configured skill; original refusal retained".into(),
        };
        super::value_metrics::observe(store, token, &observation, now);
    }
}

// ---- forge read -----------------------------------------------------------------------------

/// Reads the lock file from the repository's default branch and makes it the adoption. With a
/// forge connected only that lock counts: no file (or an invalid one) clears every adoption, and
/// a failed read still drops checkout reports. Errors are logged, never fatal: a skills read must
/// not fail a repository sync.
pub(super) async fn sync_adoption(
    state: &HubState,
    api: ForgeApi<'_>,
    repo: &ConnectedRepository,
    token: &str,
) {
    let read = async {
        let branch = api.default_branch(token, repo).await?;
        // The lock and the skill files it names are read at one commit, so a merge between
        // the reads cannot pair a pin with other bytes.
        let at = api
            .ref_sha(token, repo, &branch)
            .await?
            .unwrap_or_else(|| branch.clone());
        let file = api
            .file(token, repo, supported::LOCK_PATH, &at, MAX_LOCK_BYTES)
            .await?;
        Ok::<_, GhError>((branch, at, file))
    }
    .await;
    let (branch, at, file) = match read {
        Ok(read) => read,
        Err(error) => {
            crate::hub::error_envelope::log_internal(&error);
            let (store, name) = (state.store.clone(), repo.full_name.clone());
            let cleared = blocking(state, move || {
                store.skill_adoption_clear(&name, Some("reported_by_checkout"))
            })
            .await;
            if let Err(error) = cleared {
                crate::hub::error_envelope::log_internal(&error);
            }
            return;
        }
    };
    let lock = file.and_then(|bytes| SkillLock::parse(&bytes).ok());
    // Repository skills are GitHub only: GitLab's file read returns JSON re-encoded, so a
    // manifest's bytes, and its digest, never match the pin there.
    let mut bundles = Vec::new();
    let paths = lock
        .iter()
        .flat_map(|l| &l.skills)
        .filter_map(|p| p.source.repo_local_path())
        .filter(|_| matches!(api, ForgeApi::Github(_)))
        .take(supported::MAX_LOCAL_SKILLS);
    for path in paths {
        match read_bundle(api, token, repo, &at, path).await {
            Ok(Some(bundle)) => bundles.push(bundle),
            Ok(None) => {}
            // A failed read keeps the last definition that resolved: a forge error must never
            // drop a gate. Only a read that finds the file gone, too large or invalid changes it.
            Err(error) => return crate::hub::error_envelope::log_internal(&error),
        }
    }
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
                local_skills: stored_local(&lock, bundles),
            })
            .map(|_| ()),
        None => store.skill_adoption_clear(&name, None),
    })
    .await;
    if let Err(error) = done {
        crate::hub::error_envelope::log_internal(&error);
    }
}

/// One repository skill's manifest at `path` and its procedure, read at `branch` (a commit);
/// `None` when a file is missing, too large or the manifest is invalid (the pin is then
/// unknown); an error when the forge could not answer.
async fn read_bundle(
    api: ForgeApi<'_>,
    token: &str,
    repo: &ConnectedRepository,
    branch: &str,
    path: &str,
) -> std::result::Result<Option<supported::LocalBundle>, GhError> {
    let max = supported::MAX_LOCAL_FILE_BYTES;
    let text = |bytes: Vec<u8>| String::from_utf8(bytes).ok();
    // GitHub reports a file over `max` as a failed read; that is an answer, not an outage.
    let api_file = |path: String| async move {
        match api.file(token, repo, &path, branch, max).await {
            Err(GhError::Failed(detail)) if detail.starts_with("file is larger than") => Ok(None),
            read => read,
        }
    };
    let Some(manifest) = api_file(path.to_owned()).await?.and_then(text) else {
        return Ok(None);
    };
    let Ok(entrypoint) = supported::local_procedure_path(manifest.as_bytes()) else {
        return Ok(None);
    };
    let Some(procedure) = api_file(entrypoint).await?.and_then(text) else {
        return Ok(None);
    };
    Ok(Some(supported::LocalBundle {
        path: path.to_owned(),
        manifest,
        procedure,
    }))
}

// ---- forge observation ----------------------------------------------------------------------

/// Commit status context prefix: a skill's runs post `agentscm/skill/<name>`.
pub const STATUS_PREFIX: &str = "agentscm/skill/";
/// Token id, session and agent of evidence the hub records from the forge; no agent token
/// has this id, so it never counts as an author for review independence.
const FORGE: &str = "forge";
/// Longest status description; the provider snapshot keeps 120 characters, so a status read
/// back compares equal to the one posted.
const MAX_STATUS_CHARS: usize = 120;

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn lower_sha(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

/// Label names from a GitHub pull (objects with `name`) or a stored observation (strings).
fn labels_of(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().or_else(|| l["name"].as_str()))
        .map(str::to_owned)
        .collect()
}

/// Whether provider time `at` is at or before `cutoff` (the merge time); no cutoff admits all.
fn by(at: &Value, cutoff: Option<i64>) -> bool {
    cutoff.is_none_or(|cutoff| {
        at.as_str()
            .and_then(super::github::parse_rfc3339)
            .is_some_and(|at| at <= cutoff)
    })
}

/// What GitHub shows for one head, read fresh in one pass: the latest check run per name, the
/// combined status (latest per context) and every review of the pull request.
struct ForgeRead {
    checks: Value,
    combined: Value,
    reviews: Vec<Value>,
}

/// The listed entries of a reply whose `total_count` says they are all of them.
fn complete<'a>(body: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    let rows = body[key].as_array()?;
    (body["total_count"].as_u64() == Some(rows.len() as u64)).then_some(rows)
}

impl ForgeRead {
    /// Green CI: every check run and every status was listed, at least one exists, every check
    /// run concluded success, neutral or skipped and every status other than the skill
    /// statuses is success, all by `cutoff`.
    fn ci_passed(&self, cutoff: Option<i64>) -> bool {
        let (Some(runs), Some(statuses)) = (
            complete(&self.checks, "check_runs"),
            complete(&self.combined, "statuses"),
        ) else {
            return false;
        };
        let statuses: Vec<&Value> = statuses
            .iter()
            .filter(|s| {
                !s["context"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with(STATUS_PREFIX)
            })
            .collect();
        (!runs.is_empty() || !statuses.is_empty())
            && runs.iter().all(|r| {
                r["status"] == "completed"
                    && matches!(
                        r["conclusion"].as_str(),
                        Some("success" | "neutral" | "skipped")
                    )
                    && by(&r["completed_at"], cutoff)
            })
            && statuses
                .iter()
                .all(|s| s["state"] == "success" && by(&s["updated_at"], cutoff))
    }

    /// Reviewers whose latest decisive review by `cutoff` approves `head`, when no reviewer's
    /// latest requests changes. Bots never count. Whether a reviewer may approve is checked
    /// apart (`approver`), against the repository permission, for every candidate in turn.
    fn approvers(&self, head: &str, cutoff: Option<i64>) -> Vec<String> {
        let mut latest: BTreeMap<&str, &Value> = BTreeMap::new();
        // Reviews list oldest first; a comment does not change a reviewer's decision.
        for r in self
            .reviews
            .iter()
            .filter(|r| by(&r["submitted_at"], cutoff))
        {
            if let (Some(who), Some("APPROVED" | "CHANGES_REQUESTED" | "DISMISSED")) =
                (r["user"]["login"].as_str(), r["state"].as_str())
            {
                latest.insert(who, r);
            }
        }
        if latest.values().any(|r| r["state"] == "CHANGES_REQUESTED") {
            return Vec::new();
        }
        latest
            .into_iter()
            .filter(|(_, r)| {
                r["state"] == "APPROVED" && r["commit_id"] == head && r["user"]["type"] == "User"
            })
            .map(|(who, _)| who.to_owned())
            .collect()
    }
}

/// Reads what GitHub shows for `head` of pull request `number`; the caller bounds it.
async fn read_forge(
    state: &HubState,
    repo: &ConnectedRepository,
    token: &str,
    number: u64,
    head: &str,
) -> std::result::Result<ForgeRead, GhError> {
    let api = state.sync.api()?;
    Ok(ForgeRead {
        checks: api.latest_check_runs(token, &repo.full_name, head).await?,
        combined: api.combined_status(token, &repo.full_name, head).await?,
        reviews: api.all_reviews(token, &repo.full_name, number).await?,
    })
}

/// The first of `candidates` who has write access (write, maintain or admin) to the repository.
async fn approver(
    state: &HubState,
    repo: &ConnectedRepository,
    token: &str,
    candidates: Vec<String>,
) -> std::result::Result<Option<String>, GhError> {
    let api = state.sync.api()?;
    for login in candidates {
        let read = match api
            .collaborator_permission(token, &repo.full_name, &login)
            .await
        {
            // A login GitHub's permission read cannot name is not an approver.
            Err(GhError::Failed(detail)) if detail == "unusable login" => continue,
            read => read?,
        };
        let permission = read
            .and_then(|(_, permission)| permission)
            .and_then(|p| p["permission"].as_str().map(str::to_owned));
        if matches!(permission.as_deref(), Some("write" | "maintain" | "admin")) {
            return Ok(Some(login));
        }
    }
    Ok(None)
}

/// One fact the hub read from the forge for a pull request head.
struct Fact {
    kind: EvidenceKind,
    head: String,
    summary: String,
}

/// The pull request the facts are about, as the hub stored or read it.
#[derive(Clone)]
struct ForgePull {
    number: u64,
    branch: String,
    head: String,
    url: Option<String>,
    labels: Vec<String>,
}

impl ForgePull {
    /// A same-repository pull request with a usable branch and head: number, branch, head,
    /// head repository id, link and labels. A fork's branch name could name another run.
    fn read(
        repo: &ConnectedRepository,
        [number, branch, head, head_repo, url, labels]: [&Value; 6],
    ) -> Option<Self> {
        let branch = branch
            .as_str()
            .filter(|b| super::github::valid_ref_name(b))?;
        (head_repo.as_i64() == Some(repo.repository_id)).then_some(())?;
        Some(Self {
            number: number.as_u64().filter(|n| *n > 0)?,
            branch: branch.to_owned(),
            head: lower_sha(head)?,
            url: url.as_str().filter(|u| u.len() <= 2048).map(str::to_owned),
            labels: labels_of(labels),
        })
    }
}

/// The facts one fresh read establishes for `head`.
async fn forge_facts(
    state: &HubState,
    repo: &ConnectedRepository,
    token: &str,
    read: &ForgeRead,
    head: &str,
    cutoff: Option<i64>,
) -> std::result::Result<Vec<Fact>, GhError> {
    let mut facts = Vec::new();
    if read.ci_passed(cutoff) {
        facts.push(Fact {
            kind: EvidenceKind::HostedCi,
            head: head.to_owned(),
            summary: format!("Every GitHub check and status on {} passed.", short(head)),
        });
    }
    // A permission read the token may not make leaves the approval unobserved, not the CI fact.
    let who = approver(state, repo, token, read.approvers(head, cutoff))
        .await
        .unwrap_or_else(|error| {
            crate::hub::error_envelope::log_internal(&error);
            None
        });
    if let Some(who) = who {
        let who: String = who.chars().take(100).collect();
        facts.push(Fact {
            kind: EvidenceKind::HumanApproval,
            head: head.to_owned(),
            summary: format!(
                "{who}, who has write access, approved {} on GitHub.",
                short(head)
            ),
        });
    }
    Ok(facts)
}

/// Records `facts` for every adopted skill whose gates accept them; returns the adopted
/// skills' entries with their runs on the branch after recording.
fn record_observed(
    store: &dyn Store,
    repo: &str,
    pull: &ForgePull,
    facts: &[Fact],
) -> Result<Vec<(supported::CatalogEntry, Option<RunView>)>> {
    let pins = pins_of(store.skill_adoption(repo)?.as_ref());
    let mut out = Vec::new();
    for pin in pins
        .pins
        .iter()
        .filter(|p| p.status == AdoptionStatus::Adopted)
    {
        let Some(entry) = pins.entry(&pin.pin.sha256) else {
            continue;
        };
        let params = entry
            .manifest
            .effective_params(pin.pin.params.as_ref())
            .unwrap_or_default();
        let run_id = run_id_of(repo, &pull.branch, &pin.pin.name);
        let mut existing: Vec<StoredEvidence> = store
            .skill_evidence(repo, &run_id)?
            .iter()
            .filter_map(EvidenceRow::to_stored)
            .collect();
        for fact in facts {
            for gate in &entry.manifest.gates {
                let Some(spec) = gate.evidence.iter().find(|s| s.kind() == fact.kind) else {
                    continue;
                };
                // A required label is recorded only when the pull request carries it; a merge
                // method is never observed, so a required one leaves the gate to a report.
                let label = (fact.kind == EvidenceKind::HostedCi)
                    .then(|| spec.required_value(&params))
                    .flatten()
                    .filter(|l| pull.labels.contains(l));
                let evidence = Evidence {
                    skill: Some(pin.pin.name.clone()),
                    gate: gate.id.clone(),
                    kind: fact.kind,
                    head_sha: fact.head.clone(),
                    assurance: Assurance::Observed,
                    summary: fact.summary.clone(),
                    url: pull.url.clone(),
                    branch: Some(pull.branch.clone()),
                    pull_request: Some(pull.number),
                    detail: label.map(|label| Detail {
                        label: Some(label),
                        ..Detail::default()
                    }),
                };
                if existing.len() >= MAX_RUN_EVIDENCE
                    || !supported::admit_observed(&entry.manifest, &existing, &evidence)
                {
                    continue;
                }
                let stored = StoredEvidence {
                    id: new_id("evd")?,
                    token_id: FORGE.into(),
                    session: FORGE.into(),
                    agent: "GitHub".into(),
                    recorded_at: unix_now() as u64,
                    evidence,
                };
                store.skill_evidence_add(&EvidenceRow::from_stored(
                    repo,
                    &run_id,
                    &pull.branch,
                    &pin.pin.name,
                    &entry.sha256,
                    &stored,
                ))?;
                existing.push(stored);
            }
        }
        let head = RunHead {
            run_id,
            branch: pull.branch.clone(),
            skill: pin.pin.name.clone(),
            skill_sha256: entry.sha256.clone(),
        };
        let view = if existing.is_empty() {
            None
        } else {
            run_view(store, repo, &head, &pins)?
        };
        out.push((entry.clone(), view));
    }
    Ok(out)
}

/// The commit status a run shows on the pull request head: success once every gate before the
/// merge gate is done on that head and each of those gates the forge can show (CI, approval)
/// has observed evidence there that the gate's rule accepts and that this pass's read still
/// shows (`now`), so a later failure or a dismissed approval turns it back; failure while a
/// Stop-enforced gate the agent can do is open; pending otherwise. A report alone never turns
/// the status green. The description is the next action.
fn desired_status(
    manifest: &supported::SupportedManifest,
    view: Option<&RunView>,
    head: &str,
    now: &[EvidenceKind],
) -> (StatusState, String) {
    let text = |s: String| s.chars().take(MAX_STATUS_CHARS).collect::<String>();
    let Some(view) = view else {
        return (
            StatusState::Pending,
            format!("No {} evidence on {} yet.", manifest.name, short(head)),
        );
    };
    if view.head_sha != head {
        return (
            StatusState::Pending,
            text(format!(
                "Evidence is for {}, not the head {}.",
                short(&view.head_sha),
                short(head)
            )),
        );
    }
    let cut = supported::merge_gate_index(manifest).unwrap_or(manifest.gates.len());
    let at = view
        .current_gate
        .as_deref()
        .and_then(|id| manifest.gates.iter().position(|g| g.id == id))
        .unwrap_or(manifest.gates.len());
    if at < cut {
        let state = if view.blocks_finish {
            StatusState::Failure
        } else {
            StatusState::Pending
        };
        return (state, text(view.next_action.clone()));
    }
    let unseen: Vec<&str> = manifest.gates[..cut]
        .iter()
        .filter(|g| {
            let kinds: Vec<EvidenceKind> = g
                .evidence
                .iter()
                .map(|s| s.kind())
                .filter(|k| supported::forge_observable(*k))
                .collect();
            let evidence = view
                .gates
                .iter()
                .find(|v| v.id == g.id)
                .map_or(&[][..], |v| &v.evidence[..]);
            let shown_now = kinds.iter().all(|k| now.contains(k));
            !kinds.is_empty()
                && (!shown_now || !supported::observed_on(g, evidence, head, &view.params))
        })
        .map(|g| g.id.as_str())
        .collect();
    if !unseen.is_empty() {
        return (
            StatusState::Pending,
            text(format!(
                "Waiting for GitHub to show {} on {}.",
                unseen.join(" and "),
                short(head)
            )),
        );
    }
    (
        StatusState::Success,
        format!("Every gate before merge has evidence on {}.", short(head)),
    )
}

/// Posts the status unless the combined status already shows it for the context.
async fn post_status(
    state: &HubState,
    conn: &Connection,
    (repo, token, pull): (&ConnectedRepository, &str, &ForgePull),
    combined: &Value,
    (name, status, description): (&str, StatusState, String),
) {
    let context = format!("{STATUS_PREFIX}{name}");
    let shown = combined["statuses"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["context"] == context.as_str());
    if shown
        .is_some_and(|s| s["state"] == status.as_str() && s["description"] == description.as_str())
    {
        return;
    }
    let posted = async {
        let status = CommitStatus::new(&pull.head, status, &context, &description)
            .map_err(|e| GhError::Failed(e.to_string()))?;
        state
            .sync
            .forge(conn)?
            .post_commit_status(token, repo, &status, &pull.branch)
            .await
    }
    .await;
    match posted {
        Ok(Ok(())) => {}
        Ok(Err(code)) => eprintln!(
            "hub: {} refused the {context} status ({code})",
            repo.full_name
        ),
        Err(error) => crate::hub::error_envelope::log_internal(&error),
    }
}

/// Replaces a still-shown success of a skill status the Hub no longer publishes: the setting
/// is off, or the skill left the lock file.
const RETIRED: &str = "No longer published by the Hub for this repository.";

/// How long one pull request's forge work (reads, permission checks, posts) may take before
/// the pass moves on; a slow forge never stalls the provider-context pass.
const FORGE_SECONDS: u64 = 10;

fn adopted_names(store: &dyn Store, repo: &str) -> Result<Vec<String>> {
    Ok(pins_of(store.skill_adoption(repo)?.as_ref())
        .pins
        .iter()
        .filter(|p| p.status == AdoptionStatus::Adopted)
        .map(|p| p.pin.name.clone())
        .collect())
}

fn adopted(store: &dyn Store, repo: &str) -> Result<bool> {
    Ok(!adopted_names(store, repo)?.is_empty())
}

/// Skill names whose status the combined status shows as `success`.
fn shown_successes(combined: &Value) -> Vec<String> {
    combined["statuses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["state"] == "success")
        .filter_map(|s| s["context"].as_str()?.strip_prefix(STATUS_PREFIX))
        .map(str::to_owned)
        .collect()
}

/// After the provider snapshot pass reaches an open pull request: reads its head's checks,
/// combined status and reviews, records what passed as observed evidence, then posts each
/// adopted skill's status where the repository setting is on, or retires a success the Hub
/// left behind (setting off, or skill no longer adopted). GitHub only, within
/// [`FORGE_SECONDS`]. Failures are logged; a skills step never fails a repository sync, and an
/// unread forge posts nothing.
pub(super) async fn observe_open_pull(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    stored: &Value,
) {
    if conn.provider != "github" {
        return;
    }
    let fields = [
        &stored["number"],
        &stored["head_ref"],
        &stored["head_sha"],
        &stored["head_repository_id"],
        &stored["html_url"],
        &stored["labels"],
    ];
    let Some(pull) = ForgePull::read(repo, fields) else {
        return;
    };
    let (store, name, id) = (
        state.store.clone(),
        repo.full_name.clone(),
        repo.repository_id,
    );
    let read = blocking(state, move || {
        Ok((adopted_names(&*store, &name)?, status_setting(&*store, id)?))
    })
    .await;
    let (names, setting) = match read {
        Ok(read) => read,
        Err(error) => return crate::hub::error_envelope::log_internal(&error),
    };
    // Nothing adopted and never published here: nothing to observe or retire.
    if names.is_empty() && setting.is_none() {
        return;
    }
    let work = open_pull_work(
        state,
        conn,
        (repo, token, &pull),
        &names,
        setting == Some(true),
    );
    let deadline = std::time::Duration::from_secs(FORGE_SECONDS);
    match tokio::time::timeout(deadline, work).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => crate::hub::error_envelope::log_internal(&error),
        Err(_) => crate::hub::error_envelope::log_internal(&"skill forge pass timed out"),
    }
}

async fn open_pull_work(
    state: &HubState,
    conn: &Connection,
    (repo, token, pull): (&ConnectedRepository, &str, &ForgePull),
    names: &[String],
    publish: bool,
) -> std::result::Result<(), GhError> {
    let read = read_forge(state, repo, token, pull.number, &pull.head).await?;
    let mut runs = Vec::new();
    let mut now = Vec::new();
    if !names.is_empty() {
        let facts = forge_facts(state, repo, token, &read, &pull.head, None).await?;
        now = facts.iter().map(|f| f.kind).collect();
        let (store, name, observed) = (state.store.clone(), repo.full_name.clone(), pull.clone());
        runs = blocking(state, move || {
            record_observed(&*store, &name, &observed, &facts)
        })
        .await
        .map_err(|error| GhError::Failed(error.to_string()))?;
    }
    let shown = shown_successes(&read.combined);
    let mut posts = Vec::new();
    for (entry, view) in &runs {
        if publish {
            let (status, text) = desired_status(&entry.manifest, view.as_ref(), &pull.head, &now);
            posts.push((entry.name.clone(), status, text));
        } else if shown.contains(&entry.name) {
            posts.push((entry.name.clone(), StatusState::Error, RETIRED.to_owned()));
        }
    }
    for gone in shown.iter().filter(|n| !names.contains(n)) {
        posts.push((gone.clone(), StatusState::Error, RETIRED.to_owned()));
    }
    for (name, status, text) in posts {
        post_status(
            state,
            conn,
            (repo, token, pull),
            &read.combined,
            (&name, status, text),
        )
        .await;
    }
    Ok(())
}

/// After the forge reports a pull request merged: reads its head's checks, combined status
/// and reviews once more, records what passed by the merge time, then records the merge itself
/// as observed. The run then carries its gate-skip signal (`RunView::merge_check`). GitHub
/// only, and only when that read succeeded within [`FORGE_SECONDS`]: a merge recorded without
/// it would count every forge fact as unobserved. Failures are logged.
pub(super) async fn observe_merge(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    merged: &Value,
) {
    if conn.provider != "github" {
        return;
    }
    let fields = [
        &merged["number"],
        &merged["head"]["ref"],
        &merged["head"]["sha"],
        &merged["head"]["repo"]["id"],
        &merged["html_url"],
        &merged["labels"],
    ];
    let Some(pull) = ForgePull::read(repo, fields) else {
        return;
    };
    let (store, name) = (state.store.clone(), repo.full_name.clone());
    if !blocking(state, move || adopted(&*store, &name))
        .await
        .unwrap_or(false)
    {
        return;
    }
    let cutoff = merged["merged_at"]
        .as_str()
        .and_then(super::github::parse_rfc3339);
    let reads = async {
        let read = read_forge(state, repo, token, pull.number, &pull.head).await?;
        forge_facts(state, repo, token, &read, &pull.head, cutoff).await
    };
    let deadline = std::time::Duration::from_secs(FORGE_SECONDS);
    let mut facts = match tokio::time::timeout(deadline, reads).await {
        Ok(Ok(facts)) => facts,
        Ok(Err(error)) => return crate::hub::error_envelope::log_internal(&error),
        Err(_) => {
            return crate::hub::error_envelope::log_internal(&"skill merge read timed out");
        }
    };
    facts.push(Fact {
        kind: EvidenceKind::Merged,
        head: pull.head.clone(),
        summary: format!(
            "GitHub merged pull request #{} at head {}; the merge method is not observed.",
            pull.number,
            short(&pull.head)
        ),
    });
    let (store, name) = (state.store.clone(), repo.full_name.clone());
    if let Err(error) = blocking(state, move || {
        record_observed(&*store, &name, &pull, &facts).map(|_| ())
    })
    .await
    {
        crate::hub::error_envelope::log_internal(&error);
    }
}

// ---- the skill status setting -----------------------------------------------------------------

/// Per repository, keyed by the forge's repository id so a rename keeps it, in `hub_settings`
/// like conversation sharing. Off unless a repository admin or a Hub admin turns it on.
fn status_key(repository_id: i64) -> String {
    format!("skill_forge_status:repo:{repository_id}")
}

/// `None` when it was never set: the Hub has never posted a skill status there.
fn status_setting(store: &dyn Store, repository_id: i64) -> Result<Option<bool>> {
    Ok(
        match store.get_setting(&status_key(repository_id))?.as_deref() {
            Some("on") => Some(true),
            Some(_) => Some(false),
            None => None,
        },
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusSettingQuery {
    repository: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetStatusSetting {
    repository: String,
    enabled: bool,
}

async fn status_setting_route(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
) -> Out<Json<Value>> {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    let (host, name, token_id) = (
        principal.owner.identity.host.clone(),
        repo.clone(),
        principal.repository_id,
    );
    let enabled = blocking(&state, move || {
        // A token minted before repository ids were kept names its repository by name only.
        let id = match token_id {
            Some(id) => Some(id),
            None => store
                .repository_named(&host, &name)?
                .map(|r| r.repository_id),
        };
        id.map_or(Ok(None), |id| status_setting(&*store, id))
    })
    .await?;
    Ok(Json(
        json!({"repository": repo, "enabled": enabled == Some(true)}),
    ))
}

async fn session_status_setting(
    State(state): State<HubState>,
    human: HumanSession,
    Query(query): Query<StatusSettingQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    let (_, name, id, can_change, _) =
        super::conversation_sharing::session_scope(&state, &human, &query.repository, VIA).await?;
    let store = state.store.clone();
    let enabled = blocking(&state, move || status_setting(&*store, id)).await?;
    Ok(Json(
        json!({"repository": name, "enabled": enabled == Some(true),
        "can_change": can_change}),
    ))
}

async fn session_set_status_setting(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<SetStatusSetting>,
) -> std::result::Result<Json<Value>, ApiError> {
    let (user, name, id, can_change, _) =
        super::conversation_sharing::session_scope(&state, &human, &body.repository, VIA).await?;
    if !can_change {
        return Err(ApiError::forbidden().with_message(
            "Only a repository admin or a Hub admin changes the skill commit status",
        ));
    }
    let store = state.store.clone();
    let value = if body.enabled { "on" } else { "off" };
    blocking(&state, move || {
        store.set_setting(&status_key(id), value, &user)
    })
    .await?;
    Ok(Json(
        json!({"repository": name, "enabled": body.enabled, "can_change": true}),
    ))
}

const VIA: &str = "skill_forge_status";

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
        .route(
            "/v1/hub/repos/{repo}/skills/forge-status",
            get(status_setting_route),
        )
        .route(
            "/v1/hub/session/skill-forge-status",
            get(session_status_setting).put(session_set_status_setting),
        )
}

fn scope(principal: &Principal, repo: &str) -> std::result::Result<(), Failure> {
    if principal.repository != repo {
        return Err(not_found());
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
    let (host, repository_id) = (principal.owner.identity.host, principal.repository_id);
    blocking(&state, move || {
        // The forge's default-branch lock is the only adoption of a connected repository.
        if super::sync::find_repository(&*store, &host, &repo, repository_id)?.is_some() {
            return Ok(Err(Failure::Refused(
                StatusCode::CONFLICT,
                "forge_authoritative",
                format!(
                    "this repository has a forge connection; adoption comes from {} on the default branch",
                    supported::LOCK_PATH
                ),
            )));
        }
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
    let viewer = principal.owner.id.clone();
    let all = blocking(&state, move || {
        let mut all = views(&*store, &repo, None, None, MAX_RUNS)?;
        shown_runs(&*store, &viewer, &mut all)?;
        Ok(all)
    })
    .await?;
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
        let mut found = views(&*store, &repo, Some(&query.branch), None, 1)?;
        shown_runs(&*store, &principal.owner.id, &mut found)?;
        Ok(found)
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
        let mut found = views(&*store, &repo, None, Some(&run_id), 1)?;
        shown_runs(&*store, &principal.owner.id, &mut found)?;
        Ok(found)
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
        let mut out = record(
            &*store,
            Recorder {
                repo: &repo,
                token: &principal.token_id,
                agent: &principal.name,
                session: request.session.as_deref(),
            },
            request.evidence,
        );
        if let Ok(view) = &mut out {
            shown_runs(&*store, &principal.owner.id, std::slice::from_mut(view))?;
        }
        Ok(out)
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
        Failure::Refused(status, code, message) => {
            let mut envelope = ApiError::new(status, code).envelope().clone();
            envelope.message = super::error_envelope::server_message(&message);
            envelope.mcp()
        }
    }
}

fn gate_line(gate: &supported::GateView) -> String {
    let state = match gate.state {
        GateState::Done => "done",
        GateState::Current => "current",
        GateState::NotReached => "not reached",
    };
    let label = match gate.assurance {
        Some(Assurance::SelfReported) => " [self-reported]",
        Some(Assurance::Observed) => " [observed]",
        _ => "",
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
    if let Some(check) = &view.merge_check {
        lines.push(if check.skipped_any() || !check.unobserved.is_empty() {
            let mut gone: Vec<String> = check.skipped.clone();
            gone.extend(
                check
                    .unobserved
                    .iter()
                    .map(|g| format!("{g} (not observed)")),
            );
            if check.merged_head != check.run_head {
                gone.push("evidence for another head".into());
            }
            format!("Merged past: {}.", gone.join(", "))
        } else {
            "Merged with every earlier gate recorded and observed.".into()
        });
    }
    lines.join("\n")
}

fn render_library(lib: &Value, pins: &Adopted) -> String {
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
        let pin = pins.pins.iter().find(|p| {
            p.pin.name == name && p.pin.sha256 == sha && p.status == AdoptionStatus::Adopted
        });
        lines.push(format!(
            "{name} v{} ({}{}): {}",
            entry["workflow_version"],
            if pin.is_some() {
                "adopted"
            } else {
                "available"
            },
            if entry["first_party"] == false {
                ", repository skill"
            } else {
                ""
            },
            entry["summary"].as_str().unwrap_or("")
        ));
        let a = &entry["applicability"];
        lines.push(format!("  Follow when: {}", list(&a["triggers"])));
        lines.push(format!("  Prerequisites: {}", list(&a["prerequisites"])));
        lines.push(format!("  Not for: {}", list(&a["not_for"])));
        let mut steps = entry["walkthrough"].clone();
        if let Some(pin) = pin {
            let params = pins
                .entry(sha)
                .and_then(|e| e.manifest.effective_params(pin.pin.params.as_ref()).ok())
                .unwrap_or_default();
            let text: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
            lines.push(format!("  Parameters: {}", text.join(", ")));
            if let Some(e) = pins.entry(sha) {
                steps = json!(supported::walkthrough(&e.manifest, Some(&params)));
            }
        }
        lines.push("  How it works:".into());
        lines.extend(
            steps
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|l| format!("    {l}")),
        );
        if let (Some(_), Some(procedure)) = (pin, entry["procedure"].as_str()) {
            lines.push("  Procedure:".into());
            lines.extend(procedure.lines().map(|l| format!("    {l}")));
        }
    }
    for p in pins
        .pins
        .iter()
        .filter(|p| p.status != AdoptionStatus::Adopted)
    {
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
    super::error_envelope::from_code(super::error_envelope::ErrorCode::NotFound).mcp()
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
            let viewer = principal.owner.id.clone();
            let found = blocking(state, move || {
                let mut found = views(&*store, &repo, Some(&branch), None, MAX_RUNS)?;
                shown_runs(&*store, &viewer, &mut found)?;
                Ok(found)
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
            let viewer = principal.owner.id.clone();
            let out = blocking(state, move || {
                let mut out = record(
                    &*store,
                    Recorder {
                        repo: &repo,
                        token: &token,
                        agent: &agent,
                        session: session.as_deref(),
                    },
                    evidence,
                );
                if let Ok(view) = &mut out {
                    shown_runs(&*store, &viewer, std::slice::from_mut(view))?;
                }
                Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_repository_skills_fit_the_column() {
        let mut bundle = supported::tests::local_bundle("skills/s.json");
        let skill = bundle.skill().unwrap();
        let lock = SkillLock {
            version: 1,
            skills: vec![supported::SkillPin::repo_local(&skill, None)],
        };
        assert!(stored_local(&lock, vec![bundle.clone()]).is_some());
        // An unpinned path is not kept.
        let other = supported::tests::local_bundle("elsewhere.json");
        assert_eq!(stored_local(&lock, vec![other]), None);
        // Control characters escape six-fold: a valid 64 KiB procedure of them is kept only
        // while the encoded total stays within the column's bound.
        bundle.procedure = "\u{1}".repeat(supported::MAX_LOCAL_FILE_BYTES);
        let many = vec![bundle; 8];
        let text = stored_local(&lock, many).expect("some bundles fit");
        assert!(text.len() <= MAX_LOCAL_JSON);
        let kept: Vec<supported::LocalBundle> = serde_json::from_str(&text).unwrap();
        assert!((1..8).contains(&kept.len()), "{}", kept.len());
    }

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn listed(key: &str, rows: Value) -> Value {
        let n = rows.as_array().unwrap().len();
        json!({"total_count": n, key: rows})
    }

    fn read(runs: Value, statuses: Value, reviews: Value) -> ForgeRead {
        ForgeRead {
            checks: listed("check_runs", runs),
            combined: listed("statuses", statuses),
            reviews: reviews.as_array().unwrap().clone(),
        }
    }

    fn run(conclusion: &str) -> Value {
        json!({"name": "test", "status": "completed", "conclusion": conclusion,
            "completed_at": "2026-10-30T10:01:00Z"})
    }

    fn status(context: &str, state: &str) -> Value {
        json!({"context": context, "state": state, "updated_at": "2026-10-30T10:01:00Z"})
    }

    #[test]
    fn ci_passes_only_on_complete_green_reads_ignoring_skill_statuses() {
        let green = read(
            json!([run("success"), run("skipped")]),
            json!([]),
            json!([]),
        );
        assert!(green.ci_passed(None));
        // Every listed run must be all of them: more than one page is not green.
        let mut partial = read(json!([run("success")]), json!([]), json!([]));
        partial.checks["total_count"] = json!(120);
        assert!(!partial.ci_passed(None));
        assert!(!read(json!([]), json!([]), json!([])).ci_passed(None));
        assert!(!read(json!([run("failure")]), json!([]), json!([])).ci_passed(None));
        // Ten green checks are read in full, past the eight a snapshot keeps.
        let ten: Vec<Value> = (0..10).map(|_| run("success")).collect();
        assert!(read(json!(ten), json!([]), json!([])).ci_passed(None));
        // One entry per context; the skill's own status is left out.
        let statuses = json!([
            status("build", "success"),
            status("agentscm/skill/pr-gate", "failure")
        ]);
        assert!(read(json!([]), statuses, json!([])).ci_passed(None));
        assert!(!read(json!([]), json!([status("build", "failure")]), json!([])).ci_passed(None));
        // A check that finished after the merge does not count for it.
        let before = super::super::github::parse_rfc3339("2026-10-30T10:00:30Z");
        assert!(!green.ci_passed(before));
    }

    #[test]
    fn approvers_need_the_head_a_user_account_and_no_open_change_request() {
        let review = |who: &str, state: &str, sha: &str| {
            json!({"user": {"login": who, "type": "User"}, "state": state, "commit_id": sha,
                "submitted_at": "2026-10-30T10:02:00Z"})
        };
        let other = "b".repeat(40);
        let of = |reviews: Value| read(json!([]), json!([]), reviews).approvers(HEAD, None);
        assert_eq!(of(json!([review("m", "APPROVED", HEAD)])), vec!["m"]);
        assert!(of(json!([review("m", "APPROVED", &other)])).is_empty());
        let mut bot = review("b[bot]", "APPROVED", HEAD);
        bot["user"]["type"] = json!("Bot");
        assert!(of(json!([bot])).is_empty());
        // Oldest first: a later comment keeps the approval, a later dismissal does not.
        assert_eq!(
            of(json!([
                review("m", "APPROVED", HEAD),
                review("m", "COMMENTED", HEAD)
            ])),
            vec!["m"]
        );
        assert!(
            of(json!([
                review("m", "APPROVED", HEAD),
                review("m", "DISMISSED", HEAD)
            ]))
            .is_empty()
        );
        let blocked = json!([
            review("m", "APPROVED", HEAD),
            review("r", "CHANGES_REQUESTED", &other)
        ]);
        assert!(of(blocked).is_empty());
        // An approval after the merge does not count for it.
        let before = super::super::github::parse_rfc3339("2026-10-30T10:00:00Z");
        let late = read(json!([]), json!([]), json!([review("m", "APPROVED", HEAD)]));
        assert!(late.approvers(HEAD, before).is_empty());
    }

    #[test]
    fn the_status_turns_green_only_on_accepted_observed_evidence_still_shown_now() {
        let entry = supported::catalog_entry("pr-gate", 1).unwrap();
        let m = &entry.manifest;
        let (state, text) = desired_status(m, None, HEAD, &[]);
        assert_eq!(state, StatusState::Pending);
        assert_eq!(text, "No pr-gate evidence on aaaaaaa yet.");
        let mut params = m.effective_params(None).unwrap();
        params.insert("ci_label".into(), json!("ci-extended"));
        let identity = RunIdentity {
            run_id: "r".into(),
            repo: "acme/widgets".into(),
            branch: "feat/x".into(),
            pull_request: None,
            skill_sha256: entry.sha256.clone(),
            params,
            updated_at: 0,
        };
        let ev = |gate: &str, kind: EvidenceKind, session: &str, assurance, label: Option<&str>| {
            StoredEvidence {
                id: "e".into(),
                token_id: session.into(),
                session: session.into(),
                agent: "a".into(),
                recorded_at: 1,
                evidence: Evidence {
                    skill: None,
                    gate: gate.into(),
                    kind,
                    head_sha: HEAD.into(),
                    assurance,
                    summary: "s".into(),
                    url: None,
                    branch: None,
                    pull_request: None,
                    detail: label.map(|l| Detail {
                        label: Some(l.into()),
                        ..Detail::default()
                    }),
                },
            }
        };
        let said = Assurance::SelfReported;
        let seen = Assurance::Observed;
        let both = [EvidenceKind::HostedCi, EvidenceKind::HumanApproval];
        let mut evidence = vec![ev("branch", EvidenceKind::TaskBranch, "a", said, None)];
        let status = |evidence: &[StoredEvidence], head: &str, now: &[EvidenceKind]| {
            desired_status(
                m,
                Some(&supported::evaluate(m, &identity, evidence)),
                head,
                now,
            )
        };
        // Tests are Stop-enforced and the agent's to do: failure, naming the next action.
        let (state, text) = status(&evidence, HEAD, &both);
        assert_eq!(state, StatusState::Failure);
        assert_eq!(text, "Run the tests on aaaaaaa and record the result.");
        assert_eq!(
            status(&evidence, &"c".repeat(40), &both).0,
            StatusState::Pending
        );
        evidence.push(ev("tests", EvidenceKind::RevisionTests, "a", said, None));
        evidence.push(ev(
            "review",
            EvidenceKind::IndependentReview,
            "b",
            said,
            None,
        ));
        // Observed CI without the required label, then a report claiming the label: still
        // pending, because the observed fact the gate accepts is absent.
        evidence.push(ev("ci", EvidenceKind::HostedCi, FORGE, seen, None));
        evidence.push(ev(
            "ci",
            EvidenceKind::HostedCi,
            "a",
            said,
            Some("ci-extended"),
        ));
        evidence.push(ev(
            "approval",
            EvidenceKind::HumanApproval,
            FORGE,
            seen,
            None,
        ));
        let (state, text) = status(&evidence, HEAD, &both);
        assert_eq!(state, StatusState::Pending);
        assert_eq!(text, "Waiting for GitHub to show ci on aaaaaaa.");
        evidence.push(ev(
            "ci",
            EvidenceKind::HostedCi,
            FORGE,
            seen,
            Some("ci-extended"),
        ));
        assert_eq!(status(&evidence, HEAD, &both).0, StatusState::Success);
        // A later failure or a dismissed approval: this pass no longer shows it, so pending.
        let (state, text) = status(&evidence, HEAD, &[EvidenceKind::HostedCi]);
        assert_eq!(state, StatusState::Pending);
        assert_eq!(text, "Waiting for GitHub to show approval on aaaaaaa.");
        assert_eq!(status(&evidence, HEAD, &[]).0, StatusState::Pending);
    }
}
