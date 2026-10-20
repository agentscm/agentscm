//! `POST /v1/hub/hook/{agent}`: the endpoint an agent's hooks call. Every answer is HTTP 200
//! with JSON in that agent's own output format. The only refusal is the first attempt, per
//! session and file, to edit a file another agent holds (before the edit, for the agents whose
//! hooks can refuse); it never asks or blocks otherwise. Failures (bad token, wrong repository,
//! bad or oversized body, busy store, timeout) never refuse: they say that coordination is
//! unknown, once per session.
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Json, Path, Query, Request, State};
use axum::http::HeaderMap;
use axum::routing::post;
use serde_json::{Value, json};
use tokio::time::{Instant, timeout_at};

use super::advice::unknown;
use super::coordination::{CoordinationInboxQuery, CoordinationStatus};
use super::http::{ApiError, blocking};
use super::intent::spec_key;
use super::session::{
    SessionCall, announce_for, check_name, count_fail_open, done_for, others_for, record_hook_ms,
};
use super::store::{Principal, agent_token_digest, unix_now};
use super::work::{
    Briefing, CheckpointKind, WorkCheckpoint, WorkOverview, WorkOverviewQuery, WorkState,
};
use super::{HubError, HubState};

/// Write payloads carry file contents.
const HOOK_BODY_BYTES: usize = 1024 * 1024;
/// For the whole request, body included.
const BUDGET: Duration = Duration::from_secs(5);
const MAX_TASK_BYTES: usize = 500;
const MAX_AGENT_BYTES: usize = 32;
const ROOT_HEADER: &str = "x-agentscm-root";
const MAX_ROOT_BYTES: usize = 1024;
const DENIED: &str = "token not accepted";
/// Sessions that were already told coordination is unknown; cleared when it grows past the bound.
static NOTED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
const MAX_NOTED: usize = 10_000;
/// Tools that change a file; names are compared lowercase.
const EDIT_TOOLS: [&str; 8] = [
    "edit",
    "write",
    "multiedit",
    "notebookedit",
    "create",
    "str_replace_editor",
    "apply_patch",
    "delete",
];
const PATH_FIELDS: [&str; 4] = ["file_path", "notebook_path", "path", "filePath"];
/// Cursor events whose output must be a valid permission answer or the action is blocked.
const CURSOR_PERMISSION_EVENTS: [&str; 6] = [
    "preToolUse",
    "beforeShellExecution",
    "beforeMCPExecution",
    "beforeReadFile",
    "beforeTabFileRead",
    "subagentStart",
];

pub fn routes() -> Router<HubState> {
    Router::new().route(
        "/v1/hub/hook/{agent}",
        post(hook).layer(DefaultBodyLimit::disable()),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Agent {
    Claude,
    Codex,
    Cursor,
    Copilot,
    Generic,
}

impl Agent {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "cursor" => Self::Cursor,
            "copilot" => Self::Copilot,
            "generic" => Self::Generic,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Copilot => "copilot",
            Self::Generic => "generic",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ignore,
    Start,
    Check,
    Done,
    Fail(&'static str),
}

/// Where and how the agent takes context back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// `{}`: the agent has no output channel for this event.
    Silent,
    /// Cursor's `{"permission":"allow"}`, which is all a permission hook may say.
    Allow,
    /// `hookSpecificOutput.additionalContext` for the named event.
    Hook(&'static str),
    /// `{"additional_context"}` (Cursor).
    Snake,
    /// `{"additionalContext"}` (Copilot CLI).
    Camel,
    /// Copilot CLI's preToolUse: `{}`, or the camelCase refusal; it has no context field.
    Pre,
    Generic,
    /// The body was not usable, so the event is not known.
    Unreadable,
}

#[derive(Debug, PartialEq, Eq)]
struct Call {
    agent: Agent,
    kind: Kind,
    shape: Shape,
    via: String,
    session: Option<String>,
    task: Option<String>,
    /// Repo-relative, valid, deduplicated.
    paths: Vec<String>,
    /// Runs before the edit, in an agent whose hook can refuse it.
    deny: bool,
}

struct Outcome {
    text: String,
    /// `clean`, `fyi`, `stop` or `unknown`, for the generic format.
    status: &'static str,
    /// Why the edit is refused.
    deny: Option<String>,
}

impl Outcome {
    fn clean() -> Self {
        Self {
            text: String::new(),
            status: "clean",
            deny: None,
        }
    }

    fn unknown(reason: &str) -> Self {
        Self {
            text: unknown(reason),
            status: "unknown",
            deny: None,
        }
    }

    fn advice(text: String, stop: bool) -> Self {
        let status = match (stop, text.is_empty()) {
            (true, _) => "stop",
            (false, true) => "clean",
            (false, false) => "fyi",
        };
        Self {
            text,
            status,
            deny: None,
        }
    }
}

async fn hook(
    State(state): State<HubState>,
    Path(name): Path<String>,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let agent = Agent::from_name(&name).ok_or_else(ApiError::not_found)?;
    let repo = query.ok().and_then(|Query(mut query)| query.remove("repo"));
    let (parts, body) = request.into_parts();
    let token = super::http::bearer(&parts).ok().map(str::to_owned);
    let started = Instant::now();
    let deadline = started + BUDGET;
    let call = match timeout_at(deadline, to_bytes(body, HOOK_BODY_BYTES)).await {
        Ok(Ok(bytes)) => Call::parse(agent, &bytes, root_header(&parts.headers)),
        Ok(Err(_)) => Call::failed(agent, "hook payload too large"),
        Err(_) => Call::failed(agent, "hub timed out"),
    };
    // Only a hash of the token is kept, to tell its sessions apart.
    let who = token.as_deref().map_or_else(
        || "anon".to_owned(),
        |token| {
            let mut hasher = DefaultHasher::new();
            token.hash(&mut hasher);
            format!("{:x}", hasher.finish())
        },
    );
    let mut outcome =
        match timeout_at(deadline, call.run(&state, token.clone(), repo.clone())).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(reason)) => Outcome::unknown(reason),
            Err(_) => Outcome::unknown("hub timed out"),
        };
    if matches!(call.kind, Kind::Start | Kind::Check)
        && let Some(token) = token
    {
        let delivery = async {
            let principal = authenticate(&state, Some(token), repo).await?;
            let store = state.store.clone();
            let requested = call.session.clone();
            let id = principal.token_id.clone();
            let (session, page) = blocking(&state, move || {
                let now = unix_now();
                let session = store.session_name_for(&id, requested.as_deref(), now)?;
                let page = store.coordination_inbox(
                    &id,
                    &CoordinationInboxQuery {
                        session: Some(session.clone()),
                        ..Default::default()
                    },
                    now,
                )?;
                Ok((session, page))
            })
            .await
            .map_err(reason)?;
            Ok::<_, &'static str>((principal, session, page))
        };
        match timeout_at(deadline, delivery).await {
            Ok(Ok((principal, session, page))) => {
                for request in page.requests.iter().take(5) {
                    if matches!(
                        request.status,
                        CoordinationStatus::Resolved | CoordinationStatus::Withdrawn
                    ) {
                        continue;
                    }
                    outcome.text.push_str(&format!("\nAgentSCM coordination request {}: {} (third-party report). Inspect agentscm_coordination_inbox, acknowledge receipt, then report a split/join/handover/decline. Reading this hook does not acknowledge receipt.", request.id, bounded_hook_text(&request.proposal.title, 120)));
                    let reply_needed = request.targets.iter().any(|target| {
                        target.lead.token == principal.token_id
                            && target.lead.session == session
                            && !request.responses.iter().any(|reply| {
                                reply.target_work_id == target.work_id
                                    && reply.target_generation == target.generation
                                    && reply.responder.token == principal.token_id
                                    && reply.responder.session == session
                            })
                    });
                    if reply_needed && call.deny && matches!(call.kind, Kind::Check) {
                        outcome.deny = Some(format!(
                            "Coordination request {} needs your response before continuing edits. Read agentscm_coordination_inbox and report the proposed split, join, handover or decline.",
                            request.id
                        ));
                        outcome.status = "stop";
                    } else if outcome.status == "clean" {
                        outcome.status = "fyi";
                    }
                }
                if !page.complete || page.requests.len() > 5 {
                    outcome.text.push_str("\nCoordination inbox is partial in this hook. Page agentscm_coordination_inbox before relying on coverage.");
                }
            }
            _ if outcome.status != "unknown" => {
                outcome.text.push_str("\nAgentSCM coordination inbox delivery is unknown; inspect agentscm_coordination_inbox before assuming coverage.");
            }
            _ => {}
        }
    }
    if outcome.status == "unknown" {
        count_fail_open();
        // Said once per session; later failures are silent no-ops.
        if let Some(session) = &call.session
            && !first_note(format!("{who}\0{}\0{session}", call.agent.name()))
        {
            outcome.text.clear();
        }
    }
    record_hook_ms(u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX));
    Ok(Json(call.render(&outcome)))
}

fn first_note(key: String) -> bool {
    let mut noted = NOTED.lock().unwrap_or_else(PoisonError::into_inner);
    if noted.len() > MAX_NOTED {
        noted.clear();
    }
    noted.insert(key)
}

/// `X-Agentscm-Root`: the repository root the hook runs under; ignored unless an absolute
/// path of at most 1024 bytes without control characters.
fn root_header(headers: &HeaderMap) -> Option<&str> {
    let root = headers.get(ROOT_HEADER)?.to_str().ok()?;
    (is_absolute(root) && root.len() <= MAX_ROOT_BYTES && !root.chars().any(char::is_control))
        .then_some(root)
}

fn reason(error: HubError) -> &'static str {
    match error {
        HubError::Busy => "hub busy",
        HubError::WorkCapacity => "work storage capacity reached",
        HubError::WorkOverlap(_) => "declared work scope overlaps another assignment",
        HubError::WorkCoordinationRequired { .. } => {
            "work scope needs coordination; inspect the agent inbox"
        }
        HubError::WorkOverlapIncomplete => {
            "work overlap coverage is incomplete; no scope was claimed"
        }
        HubError::Unauthorized => DENIED,
        HubError::Invalid(_) => "invalid hook payload",
        HubError::Unavailable(detail) => {
            eprintln!("hub error: {detail}");
            "hub unavailable"
        }
    }
}

async fn authenticate(
    state: &HubState,
    token: Option<String>,
    repo: Option<String>,
) -> Result<Principal, &'static str> {
    let digest = agent_token_digest(&token.ok_or(DENIED)?).map_err(|_| DENIED)?;
    let store = state.store.clone();
    let principal = blocking(state, move || store.authenticate(&digest, unix_now()))
        .await
        .map_err(reason)?
        .filter(|principal| !principal.read_only) // a read-only token is as good as an unknown one
        .ok_or(DENIED)?;
    if repo.is_some_and(|repo| repo != principal.repository) {
        return Err("this token is for another repository");
    }
    Ok(principal)
}

impl Call {
    fn new(agent: Agent, kind: Kind, shape: Shape) -> Self {
        Self {
            agent,
            kind,
            shape,
            via: format!("hook:{}", agent.name()),
            session: None,
            task: None,
            paths: Vec::new(),
            deny: false,
        }
    }

    fn failed(agent: Agent, reason: &'static str) -> Self {
        Self::new(agent, Kind::Fail(reason), Shape::Unreadable)
    }

    /// Ignored unless it passes the hub's session-name check.
    fn session(mut self, id: Option<&str>) -> Self {
        self.session = id.filter(|id| check_name(id).is_ok()).map(str::to_owned);
        self
    }

    /// The call may be refused: the agent has not run the edit yet and its hook can stop it.
    fn deny(mut self) -> Self {
        self.deny = true;
        self
    }

    fn task(mut self, task: Option<&str>) -> Self {
        let clean: String = task
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let clean = clean.trim();
        let end = (0..=MAX_TASK_BYTES.min(clean.len()))
            .rev()
            .find(|&end| clean.is_char_boundary(end))
            .unwrap_or(0);
        self.task = Some(clean[..end].trim_end().to_owned()).filter(|task| !task.is_empty());
        self
    }

    /// Makes `paths` repo-relative against the first of `scope.roots` that holds them; drops the rest.
    fn paths<'a>(mut self, paths: impl IntoIterator<Item = &'a str>, scope: &Scope) -> Self {
        let paths: BTreeSet<String> = paths
            .into_iter()
            .filter_map(|path| match scope.cwd {
                Some(cwd) if !is_absolute(path) => relative(&format!("{cwd}/{path}"), &scope.roots),
                _ => relative(path, &scope.roots),
            })
            // Hooks claim files, never directory specs.
            .filter(|path| !path.contains('*') && spec_key(path).is_ok())
            .collect();
        self.paths = paths
            .into_iter()
            .take(super::session::MAX_SESSION_PATHS)
            .collect();
        self
    }

    fn parse(agent: Agent, body: &[u8], root: Option<&str>) -> Self {
        let v: Value = match serde_json::from_slice(body) {
            Ok(v @ Value::Object(_)) => v,
            _ => return Self::failed(agent, "hook payload not understood"),
        };
        let scope = scope(&v, root);
        match agent {
            Agent::Claude | Agent::Codex => pascal(agent, &v, &scope),
            Agent::Copilot if v.get("hook_event_name").is_some() => pascal(agent, &v, &scope),
            Agent::Copilot => copilot(&v, &scope),
            Agent::Cursor => cursor(&v, &scope),
            Agent::Generic => generic(&v, &scope),
        }
    }

    /// Whether the reply can carry text to the agent, so a handoff can be delivered with it.
    fn reports(&self) -> bool {
        !matches!(self.shape, Shape::Silent | Shape::Pre | Shape::Allow)
    }

    async fn run(
        &self,
        state: &HubState,
        token: Option<String>,
        repo: Option<String>,
    ) -> Result<Outcome, &'static str> {
        let call = |paths: Vec<String>| SessionCall {
            session: self.session.clone(),
            via: self.via.clone(),
            task: self.task.clone(),
            paths,
            deny: self.deny,
            report: self.reports(),
        };
        match self.kind {
            Kind::Fail(reason) => Err(reason),
            Kind::Ignore => Ok(Outcome::clean()),
            Kind::Check if self.paths.is_empty() => Ok(Outcome::clean()),
            Kind::Check => {
                let principal = authenticate(state, token, repo).await?;
                let store = state.store.clone();
                let token = principal.token_id.clone();
                let requested = self.session.clone();
                let paths = self.paths.clone();
                let coverage = blocking(state, move || {
                    let now = unix_now();
                    let session = store.session_name_for(&token, requested.as_deref(), now)?;
                    store.work_scope_check(&token, &session, &paths, now)
                })
                .await
                .map_err(reason)?;
                if coverage.item.is_some() && !coverage.uncovered_scope.is_empty() {
                    let text = "AgentSCM: these files are not covered by a current unfinished assignment. Start or explicitly rejoin the work, or ask its lead to checkpoint the expanded scope and resolve overlaps before editing. No new file intent was announced.".to_owned();
                    return Ok(Outcome {
                        deny: self.deny.then(|| text.clone()),
                        text,
                        status: "stop",
                    });
                }
                let advice = match announce_for(state, &principal, call(self.paths.clone())).await {
                    Ok(advice) => advice,
                    Err(HubError::Invalid(_)) => {
                        let text = "AgentSCM: this edit could not be linked to a current assignment. Use the assigned session, or explicitly start or join work with this conversation's session ID before editing. No new file intent was announced.".to_owned();
                        return Ok(Outcome {
                            deny: self.deny.then(|| text.clone()),
                            text,
                            status: "stop",
                        });
                    }
                    Err(error) => return Err(reason(error)),
                };
                let mut outcome = Outcome::advice(advice.text, advice.stop);
                outcome.deny = advice.deny;
                Ok(outcome)
            }
            Kind::Start => {
                let principal = authenticate(state, token, repo).await?;
                let store = state.store.clone();
                let token = principal.token_id.clone();
                let requested = self.session.clone();
                let briefing = blocking(state, move || {
                    let now = unix_now();
                    let subscriber = store.session_name_for(&token, requested.as_deref(), now)?;
                    Ok((
                        store.work_briefing_preview_for(&token, Some(&subscriber), 20, now)?,
                        store.work_overview(&token, &WorkOverviewQuery::default(), now)?,
                    ))
                })
                .await;
                let advice = announce_for(state, &principal, call(Vec::new()))
                    .await
                    .map_err(reason)?;
                let own = advice.touched.intent.as_ref().map(|i| i.id.as_str());
                // The touch is committed; a failed listing must not report the whole call as unknown.
                let listing = others_for(state, &principal, own)
                    .await
                    .unwrap_or_else(|_| unknown("other agents could not be listed"));
                let status = Outcome::advice(advice.text.clone(), advice.stop).status;
                let mut text = if advice.text.is_empty() {
                    listing
                } else {
                    format!("{}\n{listing}", advice.text)
                };
                text.push('\n');
                text.push_str(&match briefing {
                    Ok((briefing, overview)) => format!("{}\n{}", render_briefing(&briefing), render_overview(&overview)),
                    Err(_) => format!(
                        "{} Call agentscm_work_briefing before planning meaningful work. Once the user intent is known, call agentscm_start with a stable work_id, canonical forge source or request_key before taking coordinated paths.",
                        unknown("shared work could not be listed")
                    ),
                });
                Ok(Outcome {
                    text,
                    status,
                    deny: None,
                })
            }
            Kind::Done => {
                let principal = authenticate(state, token, repo).await?;
                done_for(state, &principal, self.session.clone())
                    .await
                    .map_err(reason)?;
                if let Some(session) = self.session.clone() {
                    pause_known_work(state, &principal, session, &self.via).await;
                }
                Ok(Outcome::clean())
            }
        }
    }

    fn render(&self, outcome: &Outcome) -> Value {
        let text = outcome.text.as_str();
        if let Some(reason) = &outcome.deny {
            let mut out = json!({"permissionDecision": "deny", "permissionDecisionReason": reason});
            match (self.shape, self.agent) {
                (Shape::Hook(event), agent) => {
                    out["hookEventName"] = json!(event);
                    // VS Code shows the reason to the user and the context to the model.
                    if agent == Agent::Copilot {
                        out["additionalContext"] = json!(reason);
                    }
                    let mut top = json!({"hookSpecificOutput": out});
                    // Copilot CLI documents only the top-level form of the same payload.
                    if agent == Agent::Copilot {
                        top["permissionDecision"] = json!("deny");
                        top["permissionDecisionReason"] = json!(reason);
                    }
                    return top;
                }
                (Shape::Pre, _) => return out,
                _ => {}
            }
        }
        match (self.shape, self.agent) {
            (Shape::Allow, _) => json!({"permission": "allow"}),
            (Shape::Generic, _) | (Shape::Unreadable, Agent::Generic) => {
                json!({"status": outcome.status, "text": text})
            }
            // The body may have been a permission event, which must get exactly this.
            (Shape::Unreadable, Agent::Cursor) => json!({"permission": "allow"}),
            (Shape::Unreadable, _) if !text.is_empty() => json!({"systemMessage": text}),
            (_, _) if text.is_empty() => json!({}),
            (Shape::Hook(event), _) => {
                json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})
            }
            (Shape::Snake, _) => json!({"additional_context": text}),
            (Shape::Camel, _) => json!({"additionalContext": text}),
            (Shape::Silent | Shape::Pre | Shape::Unreadable, _) => json!({}),
        }
    }
}

fn render_briefing(briefing: &Briefing) -> String {
    const SHOWN_ITEMS: usize = 5;
    const SHOWN_EVENTS: usize = 5;
    let mut lines = vec![format!(
        "AgentSCM shared work: {} unfinished item(s), {} change event(s) in this bounded page.",
        briefing.items.len(),
        briefing.events.len()
    )];
    for item in briefing.items.iter().take(SHOWN_ITEMS) {
        lines.push(format!(
            "- {} by {} [{:?}, claim {:?}]: {} (third-party text)",
            item.id,
            bounded_hook_text(&item.lead_agent, 64),
            item.state,
            item.claim_status,
            bounded_hook_text(&item.title, 120)
        ));
    }
    for event in briefing.events.iter().take(SHOWN_EVENTS) {
        lines.push(format!(
            "- change {} for {}: {} — {} (third-party text)",
            event.seq,
            event.work_id,
            bounded_hook_text(&event.kind, 40),
            bounded_hook_text(
                event
                    .item
                    .current
                    .as_ref()
                    .map_or(&event.item.title, |checkpoint| &checkpoint.summary),
                160
            )
        ));
    }
    if briefing.items.len() > SHOWN_ITEMS
        || briefing.events.len() > SHOWN_EVENTS
        || !briefing.complete
    {
        lines.push(
            "This bounded hook preview omits shared-work records and created no delivery. Call agentscm_work_briefing to begin durable pagination of the full snapshot and delta; acknowledge only after deliberately delivering its complete page sequence."
                .to_owned(),
        );
    }
    lines.push(
        "Once the user intent is known, call agentscm_start with a stable work_id, canonical forge source or request_key before taking coordinated paths. A branch name alone is not a work identity."
            .to_owned(),
    );
    lines.join("\n")
}

fn render_overview(overview: &WorkOverview) -> String {
    let mut lines = vec![format!(
        "AgentSCM startup recap: reported completions and decisions from Unix {} to {} (independent of read acknowledgements).",
        overview.recent_since, overview.until
    )];
    for outcome in overview.recent.iter().take(5) {
        let summary = outcome
            .item
            .current
            .as_ref()
            .map_or(&outcome.item.title, |checkpoint| &checkpoint.summary);
        lines.push(format!(
            "- {:?}: {} by {} — {} (third-party text)",
            outcome.kind,
            bounded_hook_text(&outcome.item.title, 100),
            bounded_hook_text(&outcome.item.lead_agent, 64),
            bounded_hook_text(summary, 160)
        ));
    }
    for overlap in overview.overlaps.iter().take(5) {
        lines.push(format!("- Needs coordination: {} ({}) and {} ({}) have overlapping declared scope. Split scope, join existing work, or arrange handover. (third-party text)",
            bounded_hook_text(&overlap.left.title, 80), bounded_hook_text(&overlap.left.lead_agent, 64),
            bounded_hook_text(&overlap.right.title, 80), bounded_hook_text(&overlap.right.lead_agent, 64)));
    }
    if !overview.current_complete
        || !overview.recent_complete
        || !overview.overlaps_complete
        || overview.recent.len() > 5
        || overview.overlaps.len() > 5
    {
        lines.push("This startup recap is partial. Inspect agentscm_work_overview and page agentscm_work_briefing before relying on coverage.".into());
    }
    lines.push("Only declared work is visible; empty scope is unknown coverage. Give each agent a separate token for independent catch-up. Reported finish does not prove tests, review or merge.".into());
    lines.join("\n")
}

fn bounded_hook_text(text: &str, max: usize) -> String {
    super::advice::sanitize(text, max)
}

async fn pause_known_work(state: &HubState, principal: &Principal, session: String, via: &str) {
    let store = state.store.clone();
    let token = principal.token_id.clone();
    let lookup_session = session.clone();
    let (item, session) = match blocking(state, move || {
        let now = unix_now();
        let canonical = store.session_name_for(&token, Some(&lookup_session), now)?;
        Ok((store.work_for_session(&token, &canonical, now)?, canonical))
    })
    .await
    {
        Ok((Some(item), canonical)) => (item, canonical),
        Ok((None, _)) => return,
        Err(error) => {
            eprintln!("hub: work lookup at session end: {error}");
            return;
        }
    };
    if matches!(
        item.state,
        WorkState::Paused | WorkState::Blocked | WorkState::Handover | WorkState::Finished
    ) {
        return;
    }
    let checkpoint = WorkCheckpoint {
        work_id: item.id,
        operation_id: format!("{via}-done-{}-{}", item.generation, item.updated_seq),
        generation: item.generation,
        expected_updated_seq: Some(item.updated_seq),
        session,
        kind: CheckpointKind::Pause,
        summary: "Session ended without a reported final work outcome.".into(),
        next_action: item
            .next_action
            .or_else(|| Some("Resume or deliberately hand over this unfinished work.".into())),
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: Vec::new(),
        depends_on: item.depends_on,
        related_work: item.related_work,
    };
    let store = state.store.clone();
    let token = principal.token_id.clone();
    if let Err(error) = blocking(state, move || {
        store.work_checkpoint(&token, &checkpoint, unix_now())
    })
    .await
    {
        eprintln!("hub: pause checkpoint at session end: {error}");
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

/// What payload paths are made relative to.
struct Scope<'a> {
    roots: Vec<&'a str>,
    /// Set with the root header: relative payload paths are joined to it before the root applies.
    cwd: Option<&'a str>,
}

/// The root header alone, else the working directory, then Cursor's workspace roots.
fn scope<'a>(v: &'a Value, root: Option<&'a str>) -> Scope<'a> {
    if let Some(root) = root {
        return Scope {
            roots: vec![root],
            cwd: str_at(v, "cwd"),
        };
    }
    let roots = v.get("workspace_roots").and_then(Value::as_array);
    Scope {
        roots: str_at(v, "cwd")
            .into_iter()
            .chain(roots.into_iter().flatten().filter_map(Value::as_str))
            .collect(),
        cwd: None,
    }
}

fn is_edit(tool: &str) -> bool {
    EDIT_TOOLS.contains(&tool.to_ascii_lowercase().as_str())
}

/// The files a tool call edits: patch headers for `apply_patch`, else the first path field.
fn edit_paths<'a>(tool: &str, input: &'a Value) -> Vec<&'a str> {
    if tool.eq_ignore_ascii_case("apply_patch")
        && let Some(patch) = input.as_str().or_else(|| str_at(input, "command"))
    {
        return patch_paths(patch);
    }
    PATH_FIELDS
        .iter()
        .find_map(|field| str_at(input, field))
        .into_iter()
        .collect()
}

/// The files named by the headers of a Codex patch.
fn patch_paths(patch: &str) -> Vec<&str> {
    const HEADERS: [&str; 4] = [
        "*** Update File: ",
        "*** Add File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    patch
        .lines()
        .filter_map(|line| HEADERS.iter().find_map(|h| line.strip_prefix(h)))
        .map(str::trim)
        .collect()
}

/// Claude Code, Codex and the PascalCase form of Copilot (VS Code, `.github/hooks`).
fn pascal(agent: Agent, v: &Value, scope: &Scope) -> Call {
    let call = |kind, shape| Call::new(agent, kind, shape).session(str_at(v, "session_id"));
    let tool = str_at(v, "tool_name").unwrap_or_default();
    match str_at(v, "hook_event_name") {
        Some("SessionStart") => {
            call(Kind::Start, Shape::Hook("SessionStart")).task(str_at(v, "initial_prompt"))
        }
        Some("PreToolUse") if is_edit(tool) => {
            let input = v.get("tool_input").unwrap_or(&Value::Null);
            call(Kind::Check, Shape::Hook("PreToolUse"))
                .deny()
                .paths(edit_paths(tool, input), scope)
        }
        Some("SessionEnd") => call(Kind::Done, Shape::Silent),
        _ => call(Kind::Ignore, Shape::Silent),
    }
}

/// Copilot CLI and cloud agent: camelCase fields and no event name, so the event is read from
/// the fields. Edits are checked on preToolUse, which can refuse but has no context field;
/// postToolUse (it has a `toolResult`) is ignored.
fn copilot(v: &Value, scope: &Scope) -> Call {
    let call = |kind, shape| Call::new(Agent::Copilot, kind, shape).session(str_at(v, "sessionId"));
    match str_at(v, "toolName") {
        Some(tool) if v.get("toolResult").is_none() && is_edit(tool) => {
            let args = match v.get("toolArgs") {
                Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
                Some(args) => args.clone(),
                None => Value::Null,
            };
            call(Kind::Check, Shape::Pre)
                .deny()
                .paths(edit_paths(tool, &args), scope)
        }
        Some(_) => call(Kind::Ignore, Shape::Silent),
        None if v.get("reason").is_some() => call(Kind::Done, Shape::Silent),
        None if v.get("source").is_some() || v.get("initialPrompt").is_some() => {
            call(Kind::Start, Shape::Camel).task(str_at(v, "initialPrompt"))
        }
        None => call(Kind::Ignore, Shape::Silent),
    }
}

fn cursor(v: &Value, scope: &Scope) -> Call {
    let event = str_at(v, "hook_event_name").unwrap_or_default();
    let session = str_at(v, "conversation_id").or_else(|| str_at(v, "session_id"));
    let call = |kind, shape| Call::new(Agent::Cursor, kind, shape).session(session);
    let tool = str_at(v, "tool_name").unwrap_or_default();
    match event {
        "sessionStart" => call(Kind::Start, Shape::Snake),
        // No non-blocking context field before a tool runs: the check waits for postToolUse.
        _ if CURSOR_PERMISSION_EVENTS.contains(&event) => call(Kind::Ignore, Shape::Allow),
        "postToolUse" if is_edit(tool) => {
            let input = v.get("tool_input").unwrap_or(&Value::Null);
            call(Kind::Check, Shape::Snake).paths(edit_paths(tool, input), scope)
        }
        "sessionEnd" => call(Kind::Done, Shape::Silent),
        _ => call(Kind::Ignore, Shape::Silent),
    }
}

/// `{agent?, session?, event: start|check|done, paths?, task?, cwd?}`.
fn generic(v: &Value, scope: &Scope) -> Call {
    let kind = match str_at(v, "event") {
        Some("start") => Kind::Start,
        Some("check") => Kind::Check,
        Some("done") => Kind::Done,
        _ => return Call::failed(Agent::Generic, "hook event not recognised"),
    };
    let name = str_at(v, "agent")
        .filter(|name| {
            (1..=MAX_AGENT_BYTES).contains(&name.len())
                && name
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        })
        .unwrap_or("generic");
    let mut call = Call::new(Agent::Generic, kind, Shape::Generic)
        .session(str_at(v, "session"))
        .task(str_at(v, "task"))
        .paths(
            v.get("paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
            scope,
        );
    call.via = format!("hook:{name}");
    call
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/') || path.starts_with('\\') || is_drive(path)
}

fn is_drive(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\')
        || b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// `.` and `..` resolved without touching the filesystem; None when `..` climbs above the start.
fn components(path: &str, mut stack: Vec<String>) -> Option<Vec<String>> {
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop()?;
            }
            part => stack.push(part.to_owned()),
        }
    }
    Some(stack)
}

/// `path` as a repo-relative path, lexically: an absolute path must lie under one of `roots`
/// (a relative one is taken from the first), `.` and `..` are resolved, and a path that
/// leaves the root, or is the root itself, is None. A Windows root (`C:\repo`) also reads
/// `\` as a separator and compares ignoring case.
pub fn relative(path: &str, roots: &[&str]) -> Option<String> {
    let absolute = is_absolute(path);
    let candidates = if absolute {
        roots
    } else {
        &roots[..roots.len().min(1)]
    };
    if candidates.is_empty() && absolute {
        return None;
    }
    let found = candidates.iter().find_map(|root| {
        let windows = is_drive(root);
        let slashed = |text: &str| {
            if windows {
                text.replace('\\', "/")
            } else {
                text.to_owned()
            }
        };
        let base = components(&slashed(root), Vec::new())?;
        let rest = if absolute {
            let full = components(&slashed(path), Vec::new())?;
            let under = full.len() > base.len()
                && full.iter().zip(&base).all(|(a, b)| {
                    if windows {
                        a.eq_ignore_ascii_case(b)
                    } else {
                        a == b
                    }
                });
            under.then(|| full[base.len()..].to_vec())?
        } else {
            components(&slashed(path), Vec::new())?
        };
        Some(rest)
    });
    // A relative path with no root at all is relative to the repository already.
    let rest = match (found, candidates.is_empty()) {
        (Some(rest), _) => rest,
        (None, true) => components(path, Vec::new())?,
        (None, false) => return None,
    };
    (!rest.is_empty()).then(|| rest.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_made_repo_relative_lexically() {
        let cwd = ["/work/repo"];
        let table: [(&str, &[&str], Option<&str>); 18] = [
            ("/work/repo/src/a.rs", &cwd, Some("src/a.rs")),
            ("src/a.rs", &cwd, Some("src/a.rs")),
            ("./src/../src/a.rs", &cwd, Some("src/a.rs")),
            ("/work/repo/src/./x/../a.rs", &cwd, Some("src/a.rs")),
            ("../other/a.rs", &cwd, None),
            ("src/../../a.rs", &cwd, None),
            ("/work/repo/../x/a.rs", &cwd, None),
            ("/elsewhere/a.rs", &cwd, None),
            ("/work/repository/a.rs", &cwd, None),
            ("/work/repo", &cwd, None),
            ("/work/repo/", &cwd, None),
            ("/work/repo/a.rs", &[], None),
            ("a.rs", &[], Some("a.rs")),
            ("C:\\repo\\src\\a.rs", &["C:\\repo"], Some("src/a.rs")),
            ("c:/Repo/src/a.rs", &["C:\\repo"], Some("src/a.rs")),
            ("src\\a.rs", &["C:\\repo"], Some("src/a.rs")),
            ("D:\\repo\\a.rs", &["C:\\repo"], None),
            ("C:\\repo\\..\\x\\a.rs", &["C:\\repo"], None),
        ];
        for (path, roots, want) in table {
            assert_eq!(
                relative(path, roots).as_deref(),
                want,
                "{path} in {roots:?}"
            );
        }
        // Cursor: cwd first, then every workspace root.
        let roots = ["/work/one", "/work/two"];
        assert_eq!(relative("/work/two/a.rs", &roots).as_deref(), Some("a.rs"));
        assert_eq!(relative("a.rs", &roots).as_deref(), Some("a.rs"));
        let v = json!({"cwd": "/work/one", "workspace_roots": ["/work/one", "/work/two"]});
        assert_eq!(
            scope(&v, None).roots,
            ["/work/one", "/work/one", "/work/two"]
        );
    }

    #[test]
    fn the_root_header_replaces_cwd_and_workspace_roots() {
        let body = |path: &str| {
            json!({"hook_event_name": "PreToolUse", "cwd": "/r/sub", "tool_name": "Edit",
                "workspace_roots": ["/elsewhere"], "tool_input": {"file_path": path}})
        };
        let paths = |path: &str, root| {
            Call::parse(Agent::Claude, body(path).to_string().as_bytes(), root).paths
        };
        // Without the header the subdirectory is the root.
        assert_eq!(paths("a.rs", None), ["a.rs"]);
        assert_eq!(paths("a.rs", Some("/r")), ["sub/a.rs"]);
        assert_eq!(paths("/r/src/a.rs", Some("/r")), ["src/a.rs"]);
        assert_eq!(paths("../src/a.rs", Some("/r")), ["src/a.rs"]);
        assert!(paths("../../a.rs", Some("/r")).is_empty());
        assert!(paths("/elsewhere/a.rs", Some("/r")).is_empty());

        let mut headers = HeaderMap::new();
        let get = |value: &[u8], headers: &mut HeaderMap| {
            headers.insert(
                ROOT_HEADER,
                axum::http::HeaderValue::from_bytes(value).unwrap(),
            );
            root_header(headers).map(str::to_owned)
        };
        assert_eq!(get(b"/r", &mut headers).as_deref(), Some("/r"));
        assert_eq!(get(b"C:\\repo", &mut headers).as_deref(), Some("C:\\repo"));
        assert_eq!(get(b"repo", &mut headers), None);
        assert_eq!(get(b"/r\t/x", &mut headers), None);
        assert_eq!(
            get(format!("/{}", "a".repeat(1024)).as_bytes(), &mut headers),
            None
        );
        assert_eq!(root_header(&HeaderMap::new()), None);
    }

    #[test]
    fn paths_the_hub_would_reject_are_dropped() {
        let scope = Scope {
            roots: vec!["/r"],
            cwd: None,
        };
        let call = Call::new(Agent::Claude, Kind::Check, Shape::Silent).paths(
            [
                "/r/a.rs",
                "/r/dir/**",
                "/r/b*.rs",
                "/r/a.rs",
                "/r/z.rs",
                "/elsewhere/c",
            ],
            &scope,
        );
        assert_eq!(call.paths, ["a.rs", "z.rs"]);
    }

    #[test]
    fn codex_patch_headers_name_the_files() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+*** Add File: not/me\n\
            *** Add File: src/b.rs\n+fn b() {}\n*** Delete File: old.rs\n\
            *** Update File: c.rs\n*** Move to: d/c.rs\n*** End Patch\n";
        assert_eq!(
            patch_paths(patch),
            ["src/a.rs", "src/b.rs", "old.rs", "c.rs", "d/c.rs"]
        );
        assert!(patch_paths("*** Begin Patch\n*** End Patch").is_empty());
        let input = json!({"command": patch});
        assert_eq!(edit_paths("apply_patch", &input).len(), 5);
        assert!(edit_paths("apply_patch", &json!({})).is_empty());
        assert_eq!(edit_paths("Write", &json!({"file_path": "x"})), ["x"]);
    }

    #[test]
    fn tasks_are_cleaned_and_bounded() {
        let call = |task| Call::new(Agent::Generic, Kind::Start, Shape::Generic).task(task);
        assert_eq!(call(Some("  a\nb\u{7}c ")).task.as_deref(), Some("a b c"));
        assert_eq!(call(Some("  ")).task, None);
        assert_eq!(call(None).task, None);
        let long = "é".repeat(400);
        let task = call(Some(&long)).task.unwrap();
        assert!(task.len() <= MAX_TASK_BYTES && task.chars().all(|c| c == 'é'));
    }

    fn parse(agent: Agent, v: &Value) -> Call {
        Call::parse(agent, v.to_string().as_bytes(), None)
    }

    #[test]
    fn events_map_to_actions() {
        let claude = parse(
            Agent::Claude,
            &json!({"hook_event_name": "PreToolUse", "session_id": "s", "cwd": "/r",
                "tool_name": "Edit", "tool_input": {"file_path": "/r/a.rs"}}),
        );
        assert_eq!(
            (claude.kind, claude.shape, claude.paths, claude.session),
            (
                Kind::Check,
                Shape::Hook("PreToolUse"),
                vec!["a.rs".to_owned()],
                Some("s".to_owned())
            )
        );
        let bash = parse(
            Agent::Claude,
            &json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "cwd": "/r"}),
        );
        assert_eq!((bash.kind, bash.shape), (Kind::Ignore, Shape::Silent));
        let cursor_pre = parse(
            Agent::Cursor,
            &json!({"hook_event_name": "preToolUse", "tool_name": "Write"}),
        );
        assert_eq!(
            (cursor_pre.kind, cursor_pre.shape),
            (Kind::Ignore, Shape::Allow)
        );
        let camel_pre = parse(
            Agent::Copilot,
            &json!({"sessionId": "s", "cwd": "/r", "toolName": "edit",
                "toolArgs": {"path": "/r/a.rs"}}),
        );
        // Only a reply that can carry text delivers a handoff.
        let pre_tool = Call::new(Agent::Claude, Kind::Check, Shape::Hook("PreToolUse"));
        assert!(!camel_pre.reports() && !cursor_pre.reports() && pre_tool.reports());
        assert_eq!(
            (
                camel_pre.kind,
                camel_pre.shape,
                camel_pre.deny,
                camel_pre.paths
            ),
            (Kind::Check, Shape::Pre, true, vec!["a.rs".to_owned()])
        );
        let camel_bash = parse(
            Agent::Copilot,
            &json!({"sessionId": "s", "toolName": "bash"}),
        );
        assert_eq!(camel_bash.kind, Kind::Ignore);
        assert!(claude.deny && !cursor_pre.deny);
        // Without a tool or reason, a Copilot event is a start only when it says how it began.
        let camel = |v| parse(Agent::Copilot, &v).kind;
        assert_eq!(
            camel(json!({"sessionId": "s", "source": "new"})),
            Kind::Start
        );
        assert_eq!(
            camel(json!({"sessionId": "s", "initialPrompt": "x"})),
            Kind::Start
        );
        assert_eq!(
            camel(json!({"sessionId": "s", "reason": "complete"})),
            Kind::Done
        );
        assert_eq!(
            camel(json!({"sessionId": "s", "timestamp": 1})),
            Kind::Ignore
        );
        // afterFileEdit is not a check: postToolUse is, and a second one would eat its notices.
        let after = parse(
            Agent::Cursor,
            &json!({"hook_event_name": "afterFileEdit", "file_path": "/r/a.rs", "cwd": "/r"}),
        );
        assert_eq!((after.kind, after.shape), (Kind::Ignore, Shape::Silent));
        let camel_post = parse(
            Agent::Copilot,
            &json!({"sessionId": "s", "cwd": "/r", "toolName": "edit", "toolResult": {},
                "toolArgs": {"path": "/r/a.rs"}}),
        );
        // The check ran on preToolUse; a second one would use up the dedupe.
        assert_eq!(camel_post.kind, Kind::Ignore);
        let bad_session = parse(
            Agent::Codex,
            &json!({"hook_event_name": "SessionStart", "session_id": "a\nb"}),
        );
        assert_eq!((bad_session.kind, bad_session.session), (Kind::Start, None));
        let generic = parse(
            Agent::Generic,
            &json!({"event": "check", "agent": "My Bot", "cwd": "/r", "paths": ["/r/a.rs"]}),
        );
        assert_eq!(
            (generic.via.as_str(), generic.paths),
            ("hook:generic", vec!["a.rs".into()])
        );
        let unreadable = Call::parse(Agent::Claude, b"[1]", None);
        assert_eq!(unreadable.shape, Shape::Unreadable);
        assert!(matches!(
            parse(Agent::Generic, &json!({"event": "bogus"})).kind,
            Kind::Fail(_)
        ));
    }

    #[test]
    fn only_a_refusal_decides_and_failures_say_unknown() {
        let stop = Outcome::advice("AgentSCM STOP: x".into(), true);
        let refused = Outcome {
            deny: Some("R".into()),
            ..Outcome::clean()
        };
        let fail = Outcome::unknown("hub busy");
        let call = |agent, shape| Call::new(agent, Kind::Check, shape);
        let hook = call(Agent::Claude, Shape::Hook("PreToolUse"));
        assert_eq!(
            hook.render(&stop),
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
                "additionalContext": "AgentSCM STOP: x"}})
        );
        assert_eq!(hook.render(&Outcome::clean()), json!({}));
        assert!(
            hook.render(&fail)
                .to_string()
                .contains("coordination unknown")
        );
        let deny = |event| {
            json!({"permissionDecision": "deny", "permissionDecisionReason": "R",
            "hookEventName": event})
        };
        for agent in [Agent::Claude, Agent::Codex] {
            assert_eq!(
                call(agent, Shape::Hook("PreToolUse")).render(&refused),
                json!({"hookSpecificOutput": deny("PreToolUse")})
            );
        }
        // VS Code also hands the reason to the model; Copilot CLI reads the top-level pair.
        let mut vscode = deny("PreToolUse");
        vscode["additionalContext"] = json!("R");
        assert_eq!(
            call(Agent::Copilot, Shape::Hook("PreToolUse")).render(&refused),
            json!({"hookSpecificOutput": vscode,
                "permissionDecision": "deny", "permissionDecisionReason": "R"})
        );
        let pre = call(Agent::Copilot, Shape::Pre);
        assert_eq!(
            pre.render(&refused),
            json!({"permissionDecision": "deny", "permissionDecisionReason": "R"})
        );
        // Copilot CLI's preToolUse has no context field; nothing else ever decides.
        assert_eq!(pre.render(&stop), json!({}));
        for shape in [Shape::Allow, Shape::Snake, Shape::Camel, Shape::Silent] {
            assert!(
                !call(Agent::Cursor, shape)
                    .render(&refused)
                    .to_string()
                    .contains("deny")
            );
        }
        let allow = call(Agent::Cursor, Shape::Allow);
        assert_eq!(allow.render(&fail), json!({"permission": "allow"}));
        let silent = call(Agent::Cursor, Shape::Silent);
        assert_eq!(silent.render(&fail), json!({}));
        let generic = call(Agent::Generic, Shape::Generic);
        assert_eq!(generic.render(&stop)["status"], "stop");
        assert_eq!(
            generic.render(&Outcome::clean()),
            json!({"status": "clean", "text": ""})
        );
        assert_eq!(
            call(Agent::Cursor, Shape::Snake).render(&stop)["additional_context"],
            "AgentSCM STOP: x"
        );
        assert_eq!(
            call(Agent::Copilot, Shape::Camel).render(&stop)["additionalContext"],
            "AgentSCM STOP: x"
        );
        for agent in [
            Agent::Claude,
            Agent::Codex,
            Agent::Cursor,
            Agent::Copilot,
            Agent::Generic,
        ] {
            let out = Call::failed(agent, "hook payload too large").render(&fail);
            if agent == Agent::Cursor {
                assert_eq!(out, json!({"permission": "allow"}));
                continue;
            }
            assert!(out.to_string().contains("coordination unknown"), "{out}");
            assert!(!out.to_string().contains("deny"));
            // Said once; the same failure again is the agent's silent no-op.
            let quiet = Outcome {
                text: String::new(),
                ..Outcome::unknown("hub busy")
            };
            let out = Call::failed(agent, "hook payload too large").render(&quiet);
            let want = match agent {
                Agent::Cursor => json!({"permission": "allow"}),
                Agent::Generic => json!({"status": "unknown", "text": ""}),
                _ => json!({}),
            };
            assert_eq!(out, want);
        }
        assert!(first_note("k".into()) && !first_note("k".into()) && first_note("j".into()));
    }
}
