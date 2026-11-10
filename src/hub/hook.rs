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
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::time::{Instant, timeout_at};

use super::advice::{agent_name, unknown};
use super::agent_rules::NO_BYPASS;
use super::catchup::{CatchupPreview, CatchupPreviewQuery};
use super::coordination::CoordinationInbox;
use super::coordination::{CoordinationInboxQuery, CoordinationRequest, CoordinationStatus};
use super::error_envelope::{self, ErrorCode, ErrorEnvelope};
use super::http::{ApiError, blocking};
use super::intent::spec_key;
use super::session::{
    SessionCall, Touched, announce_reading, check_name, count_fail_open, done_for,
    done_pausing_for, others_for, record_hook_ms,
};
use super::session_waiting::WaitReason;
use super::store::{Principal, Store, agent_token_digest, unix_now};
use super::work::{Briefing, WorkItem, WorkOverview, WorkOverviewQuery, WorkState};
use super::work_lifecycle::{CallPhase, EDIT_UNLINKED, WORK_UNDECLARED};
use super::{HubError, HubState};

/// Write payloads carry file contents.
const HOOK_BODY_BYTES: usize = 1024 * 1024;
/// Claude Code keeps at most this many characters (UTF-16 units) of a hook's added context.
pub const CONTEXT_LIMIT: usize = 10_000;
/// The Hub's own start context stays within this, leaving room for the hook client's local note.
pub const START_CONTEXT: usize = 9_000;
const SHORTENED: &str = "\nAgentSCM: the rest of this context was cut to fit the agent's context limit; read agentscm_work_status, agentscm_work_briefing and agentscm_work_overview for it.";
/// For the whole request, body included.
const BUDGET: Duration = Duration::from_secs(5);
const MAX_TASK_BYTES: usize = 500;
const MAX_AGENT_BYTES: usize = 32;
const ROOT_HEADER: &str = "x-agentscm-root";
const MAX_ROOT_BYTES: usize = 1024;
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
    /// The person typed a prompt in this session: delivers pending requests, writes nothing.
    Prompt,
    Check,
    /// Any other tool call: renews the lead session's lease, writes nothing else, says nothing.
    /// A shell call's start and end keep the lease live while it runs.
    Activity(CallPhase),
    /// The session stopped to wait for its person, or on its own background work; its next call
    /// ends the wait. Writes nothing to the store and says nothing.
    Waiting(WaitReason),
    /// A subagent started or stopped: only its session's roster follows it. It renews nothing
    /// and ends no wait, because a host also reports the agents it runs for itself, such as
    /// one that suggests the next prompt after a turn has ended.
    Subagent,
    Done,
    Fail(&'static str),
}

/// Claude Code's tool that puts a multiple-choice question to the person.
const ASK_USER_QUESTION: &str = "AskUserQuestion";
const MAX_ASKER_BYTES: usize = 128;

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
    /// Made by a subagent (Claude Code sends `agent_id`), whose context ends when it returns.
    child: bool,
    /// That subagent's id, when it is a usable one, to tell its wait from its siblings' work.
    asker: Option<String>,
    /// What a wait asks of its person, as the hook client excerpted it. Kept only on a wait.
    asks: Option<String>,
    /// This client's hook can carry added context at a prompt or checked tool call (Claude
    /// Code, Codex, Cursor, Copilot's PascalCase hooks); Copilot CLI's and generic hooks cannot.
    context_hook: bool,
    /// The host's own notice that it shows a permission prompt to a person, which confirms a
    /// request to answer that prompt from the Hub (`permission_answer`).
    prompt_shown: bool,
    /// The type of the subagent that made the call (Claude Code sends `agent_type`).
    agent_type: Option<String>,
    /// That subagent stopped.
    stopped: bool,
    /// The subagent this call started, as the hook client reported it.
    launched: Option<Launched>,
}

/// A subagent a session started, from the hook client's `subagent` object on the call that
/// started it: its id and type, a short excerpt of what it was started to do, and whether the
/// call returned while it still runs.
#[derive(Debug, PartialEq, Eq)]
struct Launched {
    id: String,
    agent_type: Option<String>,
    does: String,
    running: bool,
}

/// A subagent's id or type as a host sends it: short and on one line, or ignored.
fn subagent_name(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| {
            (1..=MAX_ASKER_BYTES).contains(&value.len()) && !value.chars().any(char::is_control)
        })
        .map(str::to_owned)
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

    #[cfg(test)]
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
) -> Result<Response, ApiError> {
    let onboarding_epoch = state.onboarding.epoch();
    let agent = Agent::from_name(&name).ok_or_else(ApiError::not_found)?;
    let repo = query.ok().and_then(|Query(mut query)| query.remove("repo"));
    let (parts, body) = request.into_parts();
    let token = super::http::bearer(&parts).ok().map(str::to_owned);
    let measured = (token.clone(), repo.clone());
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
    let mut delivered = None;
    let (mut outcome, mut failure) = match timeout_at(
        deadline,
        call.run(
            &state,
            token.clone(),
            repo.clone(),
            &parts.headers,
            onboarding_epoch,
            &mut delivered,
        ),
    )
    .await
    {
        Ok(Ok(outcome)) => (outcome, None),
        Ok(Err(error)) => (
            Outcome {
                text: error.hook_note(),
                status: "unknown",
                deny: None,
            },
            Some(error),
        ),
        Err(_) => {
            let error = error_envelope::from_code(ErrorCode::Unavailable);
            (
                Outcome {
                    text: error.hook_note(),
                    status: "unknown",
                    deny: None,
                },
                Some(error),
            )
        }
    };
    // The session is moving again, so its person no longer holds it up. Only a session that
    // waits costs the authentication; another token's call cannot end it.
    if let Some((native, all)) = call.ends_wait()
        && state.onboarding.waiting().has_session(native)
        && let Ok(Ok(principal)) =
            timeout_at(deadline, authenticate(&state, token.clone(), repo.clone())).await
    {
        state
            .onboarding
            .waiting()
            .clear(&principal.token_id, native, call.asker.as_deref(), all);
        if all {
            state
                .onboarding
                .subagents()
                .answered(&principal.token_id, native);
        }
    }
    if matches!(call.kind, Kind::Start | Kind::Prompt | Kind::Check)
        && let Some(token) = token
    {
        let delivery = async {
            // The touch already read the inbox; only a call that did not touch reads it here.
            if let Some((principal, session, page)) = delivered {
                return Ok((principal, session, page.map_err(reason)?));
            }
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
            Ok::<_, ErrorEnvelope>((principal, session, page))
        };
        match timeout_at(deadline, delivery).await {
            Ok(Ok((principal, session, page))) => {
                let open = |request: &&CoordinationRequest| {
                    !matches!(
                        request.status,
                        CoordinationStatus::Resolved | CoordinationStatus::Withdrawn
                    )
                };
                // A subagent's context ends when it returns, so a request shown there is lost
                // with it: point it at its lead instead, and never hold its edits for a reply.
                if call.child {
                    let waiting = page.requests.iter().filter(open).count();
                    if waiting > 0 {
                        outcome.text.push_str(&format!("\nAgentSCM: {waiting} coordination request(s) wait for your lead session. Do not answer them here; finish your task and mention them in your report."));
                        if outcome.status == "clean" {
                            outcome.status = "fyi";
                        }
                    }
                } else {
                    let mut shown = 0;
                    for request in page.requests.iter().filter(open).take(5) {
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
                        // A prompt repeats every turn, so it carries only what this session must answer.
                        if call.kind == Kind::Prompt && !reply_needed {
                            continue;
                        }
                        shown += 1;
                        outcome.text.push_str(&format!("\nAgentSCM coordination request {}: {} (third-party report). Inspect agentscm_coordination_inbox, acknowledge receipt, then report a split/join/handover/decline. Reading this hook does not acknowledge receipt.", request.id, bounded_hook_text(&request.proposal.title, 120)));
                        if reply_needed && call.deny && matches!(call.kind, Kind::Check) {
                            outcome.deny = Some(format!(
                                "Coordination request {} needs your response before continuing edits. Read agentscm_coordination_inbox and report the proposed split, join, handover or decline. {NO_BYPASS}",
                                request.id
                            ));
                            outcome.status = "stop";
                        } else if outcome.status == "clean" {
                            outcome.status = "fyi";
                        }
                    }
                    if (!page.complete || page.requests.len() > 5)
                        && (call.kind != Kind::Prompt || shown > 0)
                    {
                        outcome.text.push_str("\nCoordination inbox is partial in this hook. Page agentscm_coordination_inbox before relying on coverage.");
                    }
                }
                // What the person sent this session from the Hub page, where this reply reaches
                // the model: a prompt or a checked tool call that is not refused. The start
                // context is cut to fit, so a message there waits for the next call instead.
                let context = matches!(call.kind, Kind::Prompt | Kind::Check)
                    && matches!(call.shape, Shape::Hook(_) | Shape::Snake | Shape::Camel)
                    && outcome.deny.is_none()
                    && !call.child;
                super::agent_messages::note_hook(
                    &principal.token_id,
                    &session,
                    &call.via,
                    !call.child,
                    call.context_hook,
                    unix_now(),
                );
                // An unknown reply loses its text after the session's first, so it takes nothing.
                // A take that finishes past the deadline is offered again later (REOFFER_SECS).
                if context && outcome.status != "unknown" {
                    let (store, via) = (state.store.clone(), call.via.clone());
                    let taken = blocking(&state, move || {
                        super::agent_messages::take(&*store, &principal, &session, &via, unix_now())
                    });
                    if let Ok(Ok(Some(text))) = timeout_at(deadline, taken).await {
                        outcome.text.push('\n');
                        outcome.text.push_str(&text);
                        if outcome.status == "clean" {
                            outcome.status = "fyi";
                        }
                    }
                }
            }
            failed if outcome.status != "unknown" => {
                let error = match failed {
                    Ok(Err(error)) => error,
                    _ => error_envelope::from_code(ErrorCode::Unavailable),
                };
                outcome.text = format!("{}\n{}", error.hook_note(), outcome.text);
                outcome.status = "unknown";
                failure = Some(error);
            }
            _ => {}
        }
    }
    if outcome.status == "unknown" {
        count_fail_open();
        // Said once per session; later failures are silent no-ops.
        if call.kind != Kind::Start
            && let Some(session) = &call.session
            && !first_note(format!(
                "{who}\0{}\0{session}\0{}",
                call.agent.name(),
                failure
                    .as_ref()
                    .map_or("unavailable", |error| error.code.as_str())
            ))
        {
            outcome.text.clear();
        }
    }
    record_hook_ms(u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX));
    let rendered = call.render(&outcome);
    let sample = super::hook_overhead::sample(
        call.session.as_deref(),
        call.agent.name(),
        call.kind == Kind::Start,
        outcome.deny.is_some(),
        &rendered,
        &parts.headers,
    );
    super::hook_overhead::record(&state, measured.0.as_deref(), measured.1, sample);
    let mut response = Json(rendered).into_response();
    if let Some(reference) = failure.and_then(|error| error.correlation_id) {
        response.headers_mut().insert(
            "x-agentscm-correlation-id",
            reference.parse().expect("Hub correlation header"),
        );
    }
    Ok(response)
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

fn reason(error: HubError) -> ErrorEnvelope {
    error_envelope::from_hub_error(&error)
}

/// The inbox a touch read for this hook: who called, the touched session and its first page.
type Delivered = (Principal, String, Result<CoordinationInbox, HubError>);

/// Reads the touched session's coordination inbox inside the touch's own store hop.
fn read_inbox(
    token: String,
) -> impl FnOnce(&dyn Store, &Touched) -> (String, Result<CoordinationInbox, HubError>) + Send + 'static
{
    move |store, touched| {
        let query = CoordinationInboxQuery {
            session: Some(touched.session.clone()),
            ..Default::default()
        };
        let page = store.coordination_inbox(&token, &query, unix_now());
        (touched.session.clone(), page)
    }
}

async fn authenticate(
    state: &HubState,
    token: Option<String>,
    repo: Option<String>,
) -> Result<Principal, ErrorEnvelope> {
    let digest = agent_token_digest(
        &token.ok_or_else(|| error_envelope::from_code(ErrorCode::Unauthorized))?,
    )
    .map_err(|_| error_envelope::from_code(ErrorCode::Unauthorized))?;
    let store = state.store.clone();
    let principal = blocking(state, move || store.authenticate(&digest, unix_now()))
        .await
        .map_err(reason)?
        .filter(|principal| !principal.read_only) // a read-only token is as good as an unknown one
        .ok_or_else(|| error_envelope::from_code(ErrorCode::Unauthorized))?;
    if repo.is_some_and(|repo| repo != principal.repository) {
        return Err(error_envelope::from_code(ErrorCode::WrongRepository));
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
            child: false,
            asker: None,
            asks: None,
            context_hook: false,
            prompt_shown: false,
            agent_type: None,
            stopped: false,
            launched: None,
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

    fn child(mut self, child: bool) -> Self {
        self.child = child;
        self
    }

    /// Bounded and cleaned when the wait is recorded.
    fn asks(mut self, text: Option<&str>) -> Self {
        self.asks = text.map(str::to_owned);
        self
    }

    fn asker(mut self, id: Option<&str>) -> Self {
        self.asker = subagent_name(id);
        self
    }

    fn agent_type(mut self, name: Option<&str>) -> Self {
        self.agent_type = subagent_name(name);
        self
    }

    /// The hook client's `subagent` object; ignored unless it names the subagent and what for.
    fn launched(mut self, subagent: &Value) -> Self {
        self.launched = subagent_name(str_at(subagent, "id"))
            .zip(str_at(subagent, "does"))
            .map(|(id, does)| Launched {
                id,
                agent_type: subagent_name(str_at(subagent, "type")),
                does: does.to_owned(),
                running: subagent["running"] == true,
            });
        self
    }

    /// Keeps the roster of the session's subagents: the one that made this call, and the one
    /// this call started. Best effort and silent.
    fn note_subagents(&self, state: &HubState, principal: &Principal) {
        let Some(native) = self.session.as_deref() else {
            return;
        };
        let (roster, now) = (state.onboarding.subagents(), unix_now());
        match self.asker.as_deref() {
            Some(id) if self.stopped => roster.ended(principal, native, id, now),
            Some(id) => {
                let waits = matches!(self.kind, Kind::Waiting(_));
                roster.seen(
                    principal,
                    native,
                    id,
                    self.agent_type.as_deref(),
                    waits,
                    now,
                );
            }
            None => {}
        }
        if let Some(sub) = &self.launched {
            let running = sub.agent_type.as_deref().filter(|_| sub.running);
            roster.described(principal, native, &sub.id, running, &sub.does, now);
        }
    }

    /// The wait this call ends, if any: the session's, and whether the call ends every wait
    /// there (the person typed, or the session started or ended) or only one its caller asked.
    fn ends_wait(&self) -> Option<(&str, bool)> {
        let all = match self.kind {
            Kind::Start | Kind::Prompt | Kind::Done => true,
            Kind::Check | Kind::Activity(_) => false,
            Kind::Ignore | Kind::Waiting(_) | Kind::Subagent | Kind::Fail(_) => return None,
        };
        Some((self.session.as_deref()?, all))
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
        let (mut call, context_hook) = match agent {
            Agent::Claude | Agent::Codex => (pascal(agent, &v, &scope), true),
            Agent::Copilot if v.get("hook_event_name").is_some() => {
                (pascal(agent, &v, &scope), true)
            }
            Agent::Copilot => (copilot(&v, &scope), false),
            Agent::Cursor => (cursor(&v, &scope), true),
            Agent::Generic => (generic(&v, &scope), false),
        };
        call.context_hook = context_hook;
        call
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
        headers: &HeaderMap,
        onboarding_epoch: u64,
        delivered: &mut Option<Delivered>,
    ) -> Result<Outcome, ErrorEnvelope> {
        let client = super::onboarding::client(headers);
        let completion_reporting = super::insights::completion_enabled(headers);
        let call = |paths: Vec<String>| SessionCall {
            session: self.session.clone(),
            via: self.via.clone(),
            task: self.task.clone(),
            paths,
            deny: self.deny,
            report: self.reports(),
        };
        match self.kind {
            Kind::Fail(reason) => Err(error_envelope::from_code(match reason {
                "hub timed out" => ErrorCode::RequestTimeout,
                "hook payload too large" => ErrorCode::RequestTooLarge,
                _ => ErrorCode::InvalidRequest,
            })),
            // The coordination delivery after `run` reads the inbox; a prompt touches nothing.
            Kind::Ignore | Kind::Prompt => Ok(Outcome::clean()),
            Kind::Subagent => {
                if let Ok(principal) = authenticate(state, token, repo).await {
                    self.note_subagents(state, &principal);
                }
                Ok(Outcome::clean())
            }
            Kind::Activity(phase) => {
                // Best effort and silent: the touch renews the session's own leases, and a
                // failure must not disturb the agent. An unnamed session renews nothing.
                if self.session.is_some() {
                    let principal = authenticate(state, token, repo).await?;
                    self.note_subagents(state, &principal);
                    let store = state.store.clone();
                    let heartbeat = call(Vec::new());
                    let heartbeat = SessionCall {
                        task: None,
                        deny: false,
                        report: false,
                        ..heartbeat
                    };
                    if let Err(error) = blocking(state, move || {
                        store.session_activity(&principal.token_id, &heartbeat, phase, unix_now())
                    })
                    .await
                    {
                        return Err(reason(error));
                    }
                }
                Ok(Outcome::clean())
            }
            Kind::Waiting(reason) => {
                // Best effort and silent, like activity: a failure must not delay the prompt the
                // person is about to see. A wait no session names could never be ended, so it is
                // not kept.
                if let Some(native) = self.session.clone()
                    && let Ok(principal) = authenticate(state, token, repo).await
                {
                    self.note_subagents(state, &principal);
                    let store = state.store.clone();
                    let id = principal.token_id.clone();
                    let requested = native.clone();
                    let recorded = blocking(state, move || {
                        store.session_name_for(&id, Some(&requested), unix_now())
                    })
                    .await
                    .and_then(|session| {
                        state.onboarding.waiting().record(
                            &principal,
                            &native,
                            &session,
                            self.via.strip_prefix("hook:").unwrap_or(""),
                            reason,
                            self.asker.as_deref(),
                            self.asks.as_deref(),
                            unix_now(),
                        )
                    });
                    if let Err(error) = recorded {
                        eprintln!("hub: waiting hook not recorded: {error}");
                    } else if self.prompt_shown {
                        let requests = state.onboarding.permissions();
                        requests.confirm(&principal.token_id, &native, unix_now());
                    }
                }
                Ok(Outcome::clean())
            }
            Kind::Check if self.paths.is_empty() => Ok(Outcome::clean()),
            Kind::Check => {
                let principal = authenticate(state, token, repo).await?;
                self.note_subagents(state, &principal);
                let store = state.store.clone();
                let token = principal.token_id.clone();
                let requested = self.session.clone();
                let paths = self.paths.clone();
                let (resolved, coverage) = blocking(state, move || {
                    let now = unix_now();
                    let session = store.session_name_for(&token, requested.as_deref(), now)?;
                    let coverage = store.work_scope_check(&token, &session, &paths, now)?;
                    Ok((session, coverage))
                })
                .await
                .map_err(reason)?;
                if let Some(item) = &coverage.item
                    && !coverage.uncovered_scope.is_empty()
                {
                    let lead =
                        item.lead_token == principal.token_id && item.lead_session == resolved;
                    let text = self.unlinked_refusal(Some((item, lead)));
                    return Ok(Outcome {
                        deny: self.deny.then(|| text.clone()),
                        text,
                        status: "stop",
                    });
                }
                let touched = announce_reading(
                    state,
                    &principal,
                    call(self.paths.clone()),
                    read_inbox(principal.token_id.clone()),
                )
                .await;
                let advice = match touched {
                    Ok((advice, (session, page))) => {
                        *delivered = Some((principal.clone(), session, page));
                        advice
                    }
                    Err(error)
                        if matches!(&error, HubError::Invalid(_))
                            || matches!(&error, HubError::Rejected(rejection) if rejection.code() == ErrorCode::LeaseLapsed) =>
                    {
                        let envelope = reason(error);
                        let text = if envelope.code == "lease_lapsed" {
                            let detail = &envelope.message;
                            let child = if self.child {
                                " You are a subagent: do not start or take over Work from here; tell your parent, which holds the claim."
                            } else {
                                ""
                            };
                            format!(
                                "AgentSCM: {detail}{child} {NO_BYPASS} No new file intent was announced."
                            )
                        } else {
                            self.unlinked_refusal(None)
                        };
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
                // A session that declared no Work is invisible to its person and the team, and
                // its waits never reach them. Its first edit of a file that can be held is refused
                // once with the start call, or noted beside the edit where the host cannot refuse;
                // a repeat passes, so nothing is locked. A subagent works under its parent.
                let holdable: Vec<String> = self
                    .paths
                    .iter()
                    .filter(|path| !super::session::never_hold(path))
                    .cloned()
                    .collect();
                if coverage.item.is_none()
                    && !self.child
                    && !holdable.is_empty()
                    && let Some(native) = self.session.as_deref()
                    && state.onboarding.start_nudges().first(
                        &principal.repository,
                        &principal.token_id,
                        native,
                        unix_now(),
                    )
                {
                    super::session::count_undeclared();
                    let why = self.undeclared_text(&principal.repository, &holdable);
                    outcome = if self.deny {
                        // One refusal: any note on a file another session holds rides along, and
                        // that note's own warn-once is spent, so the repeat passes both.
                        let mut text = format!(
                            "AgentSCM: {WORK_UNDECLARED}: this edit was not applied because {why} Then make the edit again. If it is not part of a task, repeat it unchanged: this refusal happens once per session. {NO_BYPASS}"
                        );
                        // A holder's refusal is in `deny`; a plain note is in `text`.
                        let prior = outcome.deny.as_deref().unwrap_or(&outcome.text);
                        if !prior.is_empty() {
                            text = format!("{text}\n{prior}");
                        }
                        Outcome {
                            deny: Some(text.clone()),
                            text,
                            status: "stop",
                        }
                    } else {
                        let note = format!("AgentSCM: {WORK_UNDECLARED}: {why}");
                        let text = if outcome.text.is_empty() {
                            note
                        } else {
                            format!("{}\n{note}", outcome.text)
                        };
                        Outcome { text, ..outcome }
                    };
                }
                if self.agent != Agent::Generic || self.via == "hook:gemini" {
                    state.onboarding.native(
                        &principal,
                        client,
                        self.via.strip_prefix("hook:").unwrap_or(""),
                        self.session.as_deref(),
                        false,
                        onboarding_epoch,
                    );
                }
                Ok(outcome)
            }
            Kind::Start => {
                let principal = authenticate(state, token, repo).await?;
                if self.agent != Agent::Generic || self.via == "hook:gemini" {
                    state.onboarding.restart(
                        &principal,
                        client,
                        self.via.strip_prefix("hook:").unwrap_or(""),
                        self.session.as_deref(),
                        onboarding_epoch,
                    );
                }
                let store = state.store.clone();
                let token = principal.token_id.clone();
                let requested = self.session.clone();
                let briefing = blocking(state, move || {
                    let now = unix_now();
                    let subscriber = store.session_name_for(&token, requested.as_deref(), now)?;
                    let briefing =
                        store.work_briefing_preview_for(&token, Some(&subscriber), 20, now)?;
                    let overview =
                        store.work_overview(&token, &WorkOverviewQuery::default(), now)?;
                    let subscriber = super::catchup::agent_subscriber(&subscriber)?;
                    let catchup = match store.work_catchup_preview(
                        &token,
                        &CatchupPreviewQuery { subscriber },
                        now,
                    ) {
                        Ok(page) => Some(page),
                        Err(error) => {
                            crate::hub::error_envelope::log_internal(&error);
                            None
                        }
                    };
                    Ok((briefing, overview, catchup))
                })
                .await;
                let inbox = read_inbox(principal.token_id.clone());
                let (advice, (session, page)) =
                    announce_reading(state, &principal, call(Vec::new()), inbox)
                        .await
                        .map_err(reason)?;
                *delivered = Some((principal.clone(), session, page));
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
                let store = state.store.clone();
                let setup = state.onboarding.clone();
                let scope = principal.clone();
                // Bound optional enrichment after the committed touch; a slow read cannot consume
                // the native client's whole startup deadline or hide that touch's outcome.
                let current_status = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    blocking(state, move || {
                        super::work_http::actionable_status::read(
                            &*store,
                            &setup,
                            &scope,
                            unix_now(),
                        )
                    }),
                );
                // The operator's context source is asked while the status is read, never after
                // it, and not for a subagent's session.
                let session_name = advice.touched.session.as_str();
                let operator_context = async {
                    match &state.context_source {
                        Some(source) if !self.child => {
                            let moment = super::context_source::Moment::SessionStart;
                            source.lines(moment, &principal, session_name).await
                        }
                        _ => Vec::new(),
                    }
                };
                let (current_status, operator_context) =
                    tokio::join!(current_status, operator_context);
                // Most useful first: a host's context limit cuts the end (see `START_CONTEXT`).
                text.push('\n');
                match current_status {
                    Ok(Ok(facts)) => text.push_str(&super::work_http::actionable_status::render_compact_text(&facts)),
                    _ => text.push_str("Current actionable status unavailable; retry agentscm_work_status. Missing coverage does not mean clear status."),
                }
                if let Some(block) = super::context_source::render(&operator_context) {
                    text.push('\n');
                    text.push_str(&block);
                }
                text.push('\n');
                text.push_str(&match briefing {
                    Ok((briefing, overview, catchup)) => format!(
                        "{}\n{}\n{}",
                        render_briefing(&briefing),
                        render_overview(&overview),
                        render_catchup(catchup.as_ref())
                    ),
                    Err(_) => format!(
                        "{} Call agentscm_work_briefing before planning meaningful work. Once the user intent is known, call agentscm_start with a stable work_id, canonical forge source or request_key before taking coordinated paths.",
                        unknown("shared work could not be listed")
                    ),
                });
                // Only while a requested briefing waits for an agent.
                if !principal.read_only
                    && super::narrative::pending(state, &principal)
                        .is_ok_and(|requests| !requests.is_empty())
                {
                    text.push('\n');
                    text.push_str(super::mcp::NARRATIVE_GUIDANCE);
                }
                Ok(Outcome {
                    text,
                    status,
                    deny: None,
                })
            }
            Kind::Done => {
                let principal = authenticate(state, token, repo).await?;
                state.onboarding.native(
                    &principal,
                    client,
                    self.via.strip_prefix("hook:").unwrap_or(""),
                    self.session.as_deref(),
                    true,
                    onboarding_epoch,
                );
                // A call that names no session pauses no Work, since the session it resolves to
                // is not the call's own.
                match self.session.clone() {
                    Some(session) => {
                        let roster = state.onboarding.subagents();
                        roster.forget(&principal.token_id, &session);
                        done_pausing_for(
                            state,
                            &principal,
                            Some(session),
                            &self.via,
                            completion_reporting,
                        )
                        .await
                        .map_err(reason)?;
                    }
                    None => {
                        done_for(state, &principal, None).await.map_err(reason)?;
                    }
                }
                Ok(Outcome::clean())
            }
        }
    }

    /// Why a session that declared no Work in `repository` is told to, with the calls that
    /// declare it through MCP or, when its tools are bound to another repository, the CLI.
    fn undeclared_text(&self, repository: &str, paths: &[String]) -> String {
        let session = self.session.as_deref().unwrap_or_default();
        let named = serde_json::to_string(session).unwrap_or_default();
        let paths = serde_json::to_string(paths).unwrap_or_default();
        format!(
            "this session has declared no Work in {repository}, so your person and other agents cannot see what it is doing, and when it stops to wait on your person they are not told. Declare it: call agentscm_start with session {named}, a short task title and paths {paths}. If your AgentSCM tools are bound to another repository, that call records the Work there instead: run `agentscm work start` inside this file's checkout with {{\"title\":\"<short task title>\",\"session\":{named},\"scope\":{paths}}} on standard input. When the task is done, call agentscm_done with outcome complete."
        )
    }

    /// The refusal of an edit the Hub cannot link to a current assignment of this session, with
    /// the call that links it for the caller's situation: `work` is the session's Work, if any,
    /// and whether this session leads it. It holds for a repeat of the edit.
    fn unlinked_refusal(&self, work: Option<(&WorkItem, bool)>) -> String {
        let session = self
            .session
            .as_deref()
            .filter(|id| check_name(id).is_ok())
            .map(|id| serde_json::to_string(id).unwrap_or_default());
        let named = session
            .as_deref()
            .unwrap_or("set to your native session ID");
        let paths = serde_json::to_string(&self.paths).unwrap_or_default();
        let recovery = match work {
            _ if self.child => format!(
                "You are a subagent: AgentSCM checks your edits under your parent's session{}. Do not start, join or take over Work from here. Finish without this edit and tell your parent it was refused, so the session holding the claim can start or update its Work, or make the edit itself.",
                session
                    .as_deref()
                    .map(|id| format!(" {id}"))
                    .unwrap_or_default()
            ),
            Some((item, true)) if matches!(item.state, WorkState::Active | WorkState::Blocked) => {
                format!(
                    "These files are outside the scope of {id}, which this session leads. Record them first: call agentscm_work_checkpoint with work_id {id}, session {named}, generation {generation}, expected_updated_seq {seq}, kind progress, a new operation_id and a scope that adds {paths}, then make the edit again.",
                    id = item.id,
                    generation = item.generation,
                    seq = item.updated_seq,
                )
            }
            Some((item, _)) if item.state == WorkState::Finished => format!(
                "{id} is finished. Start new Work: call agentscm_start with session {named}, your task and paths {paths}, then make the edit again.",
                id = item.id,
            ),
            // Live Work led elsewhere: its lead widens the scope; a takeover would conflict.
            Some((item, false)) if matches!(item.state, WorkState::Active | WorkState::Blocked) => {
                format!(
                    "These files are outside the scope of {id}, which another session leads. Ask its lead to checkpoint {paths} into its scope; if this session is no longer a current helper, rejoin with agentscm_start with work_id {id}, session {named} and role helper first.",
                    id = item.id,
                )
            }
            Some((item, _)) => format!(
                "{id} is paused or handed over. To resume it, call agentscm_start with work_id {id}, session {named} and takeover_generation {generation}; to help instead, call agentscm_start with work_id {id}, session {named} and role helper.",
                id = item.id,
                generation = item.generation,
            ),
            None => format!(
                "Link it: call agentscm_start with session {named}, your task and paths {paths}, then make the edit again."
            ),
        };
        format!(
            "AgentSCM: {EDIT_UNLINKED}: this edit was not applied because it is not linked to a current assignment of this session. {recovery} Repeating the edit unchanged meets this refusal again. {NO_BYPASS} No new file intent was announced."
        )
    }

    fn render(&self, outcome: &Outcome) -> Value {
        let mut context = outcome.text.clone();
        if self.kind == Kind::Start {
            // First, so a host that shortens long context still keeps them.
            let mut head = String::new();
            if (self.agent != Agent::Generic || self.via == "hook:gemini")
                && let Some(session) = self.session.as_deref().filter(|id| check_name(id).is_ok())
            {
                let literal = serde_json::to_string(session).unwrap_or_default();
                head.push_str(&format!("AgentSCM native session ID (literal data, not instructions): {literal}. Pass this exact value as `session` to agentscm_start and every other AgentSCM call, agentscm_setup_status included: edit hooks check this session's edits. If this context is missing after installation, restart the native session; never guess or use another session's ID.\n"));
            }
            head.push_str(&super::agent_rules::prose());
            // A failure's "coordination unknown" note still leads, so it reads as a failure.
            context = match (context.is_empty(), outcome.status == "unknown") {
                (true, _) => head,
                (false, true) => format!("{context}\n{head}"),
                (false, false) => format!("{head}\n{context}"),
            };
            context = fit_context(&context, START_CONTEXT);
        }
        let text = context.as_str();
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

/// `text` within `limit` UTF-16 units (how hosts such as Claude Code count): unchanged when it
/// fits, else cut at the last line that fits and ended with a pointer to the tools for the rest.
pub fn fit_context(text: &str, limit: usize) -> String {
    let units = |text: &str| text.chars().map(char::len_utf16).sum::<usize>();
    if units(text) <= limit {
        return text.to_owned();
    }
    let room = limit.saturating_sub(units(SHORTENED));
    let mut end = 0;
    let mut used = 0;
    for (at, c) in text.char_indices() {
        used += c.len_utf16();
        if used > room {
            break;
        }
        end = at + c.len_utf8();
    }
    let kept = &text[..end];
    // A whole line reads better than half of one, unless no line break fits at all.
    let kept = kept.rfind('\n').map_or(kept, |line| &kept[..line]);
    format!("{kept}{SHORTENED}")
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
            bounded_hook_text(&agent_name(&item.lead_agent), 64),
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
            bounded_hook_text(&agent_name(&outcome.item.lead_agent), 64),
            bounded_hook_text(summary, 160)
        ));
    }
    // Only overlaps between live tasks need coordination; paused or quiet pairs stay in the overview.
    let live_overlaps: Vec<_> = overview
        .overlaps
        .iter()
        .filter(|overlap| super::repository_summary::is_live_overlap(overlap))
        .collect();
    for overlap in live_overlaps.iter().take(5) {
        lines.push(format!("- Needs coordination: {} ({}) and {} ({}) have overlapping declared scope. Split scope, join existing work, or arrange handover. (third-party text)",
            bounded_hook_text(&overlap.left.title, 80), bounded_hook_text(&agent_name(&overlap.left.lead_agent), 64),
            bounded_hook_text(&overlap.right.title, 80), bounded_hook_text(&agent_name(&overlap.right.lead_agent), 64)));
    }
    if !overview.current_complete
        || !overview.recent_complete
        || !overview.overlaps_complete
        || overview.recent.len() > 5
        || live_overlaps.len() > 5
    {
        lines.push("This startup recap is partial. Inspect agentscm_work_overview and page agentscm_work_briefing before relying on coverage.".into());
    }
    lines.push("Only declared work is visible; empty scope is unknown coverage. Reported finish does not prove tests, review or merge.".into());
    lines.join("\n")
}

fn render_catchup(page: Option<&CatchupPreview>) -> String {
    let Some(page) = page else {
        return "AgentSCM durable catch-up is unavailable. Merge and Work-event coverage are unknown; do not infer that nothing changed."
            .to_owned();
    };
    let mut lines = vec![format!(
        "AgentSCM catch-up preview: {} Work event(s), {} coordination event(s), and {} provider-confirmed merge observation(s) in this page; complete through fixed event {} and merge {} high-waters: {}.",
        page.work_events.len(),
        page.coordination_events.len(),
        page.merges.len(),
        page.event_through,
        page.merge_through,
        page.complete,
    )];
    for merge in page.merges.iter().take(5) {
        lines.push(format!(
            "- {:?} {} PR #{}: {} -> {} (observed {})",
            merge.source,
            bounded_hook_text(&merge.source_id, 80),
            merge.pull_number,
            merge.head_commit,
            merge.merge_commit,
            merge.observed_at,
        ));
    }
    lines.push(format!(
        "This automatic preview creates no delivery or acknowledgement. Call agentscm_work_catchup with this exact JSON to begin an explicit delivery and read every page: {}",
        json!({"subscriber": page.subscriber, "delivery_client": page.subscriber, "limit": 20})
    ));
    if page.retention.work_gap || page.retention.coordination_gap || page.retention.merge_gap {
        lines.push("Retained catch-up has a history gap. Treat affected coverage as unknown and inspect the current source and Work state.".to_owned());
    }
    lines.push(format!(
        "Source coverage: {}.",
        page.source_coverage
            .iter()
            .map(|source| format!("{:?}={:?}", source.source, source.state))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if page
        .source_coverage
        .iter()
        .any(|source| source.state != super::catchup::CatchupCoverageState::ObservedOnly)
    {
        lines.push("Some catch-up sources are incomplete or unavailable; affected coverage is unknown. Do not infer that no recent merges were recorded.".to_owned());
    }
    lines.push("Merge observations are exact typed records, not proof of arbitrary Git ancestry. For Git, check `git merge-base --is-ancestor <merge_commit> <source_revision>`; missing objects or an unavailable check leave the relation unknown. Inspect exact source and declared scope before claiming or editing.".to_owned());
    lines.join("\n")
}

fn bounded_hook_text(text: &str, max: usize) -> String {
    super::advice::sanitize(text, max)
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

/// The agents' shell tools, whose single call can run longer than a lease.
pub fn is_shell_tool(tool: &str) -> bool {
    [
        "bash",
        "shell",
        "exec_command",
        "local_shell",
        "run_shell_command",
        "run_terminal_cmd",
        "run_in_terminal",
    ]
    .contains(&tool.to_ascii_lowercase().as_str())
}

/// The activity phase of a tool event: a shell call starts before and ends after it runs.
fn phase(tool: &str, before: bool) -> CallPhase {
    match (is_shell_tool(tool), before) {
        (false, _) => CallPhase::Tick,
        (true, true) => CallPhase::Start,
        (true, false) => CallPhase::End,
    }
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
    let call = |kind, shape| {
        Call::new(agent, kind, shape)
            .session(str_at(v, "session_id"))
            .child(v.get("agent_id").is_some())
            .asker(str_at(v, "agent_id"))
            .agent_type(str_at(v, "agent_type"))
    };
    let tool = str_at(v, "tool_name").unwrap_or_default();
    let waiting = |reason| call(Kind::Waiting(reason), Shape::Silent).asks(str_at(v, "asks"));
    match str_at(v, "hook_event_name") {
        // The start prompt is never read: it would become a team-visible task.
        Some("SessionStart") => call(Kind::Start, Shape::Hook("SessionStart")),
        // Fires only where a person types, so it reaches the session they are talking to. The
        // prompt itself is never read; the client does not forward it.
        Some("UserPromptSubmit") => call(Kind::Prompt, Shape::Hook("UserPromptSubmit")),
        // Only the facts of the wait are read: never the message, tool input or question. A
        // question raises its own permission request, which must not turn its wait into a
        // permission wait. Answering a permission prompt from the Hub is in `permission_answer`.
        Some("PreToolUse" | "PermissionRequest") if tool == ASK_USER_QUESTION => {
            waiting(WaitReason::Question)
        }
        Some("PermissionRequest") => waiting(WaitReason::Permission),
        Some("Notification") => match notified(str_at(v, "notification_type")) {
            Some(reason) => {
                let mut call = waiting(reason);
                call.prompt_shown = reason == WaitReason::Permission;
                call
            }
            None => call(Kind::Ignore, Shape::Silent),
        },
        // Paused on its own background work, which will resume it, not on the person: nobody
        // is asked, but the Work it leads is not quiet.
        // The client sends the count; a legacy hook sends Claude Code's own list.
        Some("Stop")
            if v["background_tasks"].as_u64().is_some_and(|n| n > 0)
                || v["background_tasks"]
                    .as_array()
                    .is_some_and(|tasks| !tasks.is_empty()) =>
        {
            call(Kind::Waiting(WaitReason::Background), Shape::Silent)
        }
        Some("Stop") => waiting(WaitReason::Idle),
        Some("PreToolUse") if is_edit(tool) => {
            let input = v.get("tool_input").unwrap_or(&Value::Null);
            call(Kind::Check, Shape::Hook("PreToolUse"))
                .deny()
                .paths(edit_paths(tool, input), scope)
        }
        // The call that started a subagent says which one and what for; nothing else of a tool
        // call's input or result is read.
        Some(event @ ("PreToolUse" | "PostToolUse")) => call(
            Kind::Activity(phase(tool, event == "PreToolUse")),
            Shape::Silent,
        )
        .launched(&v["subagent"]),
        Some(event @ ("SubagentStart" | "SubagentStop")) if v.get("agent_id").is_some() => {
            let mut call = call(Kind::Subagent, Shape::Silent);
            call.stopped = event == "SubagentStop";
            call
        }
        Some("SessionEnd") => call(Kind::Done, Shape::Silent),
        _ => call(Kind::Ignore, Shape::Silent),
    }
}

/// Copilot CLI and cloud agent: camelCase fields and no event name, so the event is read from
/// the fields. Edits are checked on preToolUse, which can refuse but has no context field;
/// every other tool event, postToolUse (it has a `toolResult`) included, is activity.
fn copilot(v: &Value, scope: &Scope) -> Call {
    let call = |kind, shape| {
        Call::new(Agent::Copilot, kind, shape)
            .session(str_at(v, "sessionId"))
            .child(v.get("agentId").is_some())
            .asker(str_at(v, "agentId"))
    };
    if v.get("toolName").is_none() {
        // notification and agentStop; a subagent's own stop (subagentStop) is not a wait.
        let reason = match notified(str_at(v, "notification_type")) {
            Some(reason) => Some(reason),
            None if v.get("stopReason").is_some() && v.get("agentId").is_none() => {
                Some(WaitReason::Idle)
            }
            None => None,
        };
        if let Some(reason) = reason {
            return call(Kind::Waiting(reason), Shape::Silent);
        }
        if v.get("notification_type").is_some() || v.get("stopReason").is_some() {
            return call(Kind::Ignore, Shape::Silent);
        }
    }
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
        Some(tool) => call(
            Kind::Activity(phase(tool, v.get("toolResult").is_none())),
            Shape::Silent,
        ),
        None if v.get("reason").is_some() => call(Kind::Done, Shape::Silent),
        None if v.get("source").is_some() || v.get("initialPrompt").is_some() => {
            call(Kind::Start, Shape::Camel)
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
        // Cursor reports no tool start, so its calls only renew.
        _ if CURSOR_PERMISSION_EVENTS.contains(&event) => {
            call(Kind::Activity(CallPhase::Tick), Shape::Allow)
        }
        "postToolUse" if is_edit(tool) => {
            let input = v.get("tool_input").unwrap_or(&Value::Null);
            call(Kind::Check, Shape::Snake).paths(edit_paths(tool, input), scope)
        }
        "postToolUse" => call(Kind::Activity(CallPhase::Tick), Shape::Silent),
        // Cursor reports no permission prompt or question, only the end of a turn.
        "stop" => call(Kind::Waiting(WaitReason::Idle), Shape::Silent),
        "sessionEnd" => call(Kind::Done, Shape::Silent),
        _ => call(Kind::Ignore, Shape::Silent),
    }
}

/// The wait a host notification reports. Claude Code and Copilot CLI share these names; other
/// notifications (a sign-in, a finished background shell) are not waits.
fn notified(kind: Option<&str>) -> Option<WaitReason> {
    Some(match kind? {
        "permission_prompt" => WaitReason::Permission,
        "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input" => {
            WaitReason::Question
        }
        "idle_prompt" | "agent_idle" => WaitReason::Idle,
        _ => return None,
    })
}

/// `{agent?, session?, event: start|check|activity|waiting|done, phase?: start|end,
/// reason?: permission|question|idle, paths?, task?, cwd?}`. A `waiting` event with a reason
/// this Hub does not know is ignored.
fn generic(v: &Value, scope: &Scope) -> Call {
    let kind = match str_at(v, "event") {
        Some("start") => Kind::Start,
        Some("check") => Kind::Check,
        Some("activity") => Kind::Activity(match str_at(v, "phase") {
            Some("start") => CallPhase::Start,
            Some("end") => CallPhase::End,
            _ => CallPhase::Tick,
        }),
        Some("waiting") => match str_at(v, "reason").and_then(WaitReason::from_code) {
            Some(reason) => Kind::Waiting(reason),
            None => Kind::Ignore,
        },
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
        .asks(str_at(v, "asks"))
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
    use crate::hub::work::{ClaimStatus, WorkItem, WorkOverlapPair, WorkSource};

    fn overlap_side(title: &str, state: WorkState, claim_status: ClaimStatus) -> WorkItem {
        WorkItem {
            initial_issue_choice: Default::default(),
            issue_link: None,
            id: format!("wrk_{title}"),
            repository: "example-org/api".into(),
            owner: "usr_1".into(),
            owner_login: "example-user".into(),
            source: WorkSource::Native {
                request_key: title.into(),
            },
            title: title.into(),
            description: None,
            state,
            claim_status,
            last_checkin_at: 1_000,
            reported_at: None,
            generation: 1,
            lead_token: "tok".into(),
            lead_agent: "agent".into(),
            lead_session: "session".into(),
            participants: vec![],
            scope: vec![],
            branch: None,
            worktree: None,
            next_action: None,
            current: None,
            blocker: None,
            depends_on: vec![],
            related_work: vec![],
            created_at: 1_000,
            updated_at: 1_000,
            created_seq: 1,
            updated_seq: 1,
        }
    }

    #[test]
    fn startup_recap_lists_only_overlaps_between_live_tasks() {
        let pair = |left: (&str, WorkState), right: (&str, WorkState)| WorkOverlapPair {
            left: overlap_side(left.0, left.1, ClaimStatus::Active),
            right: overlap_side(right.0, right.1, ClaimStatus::Active),
            reasons: vec![],
            reasons_complete: true,
        };
        let overview = WorkOverview {
            planned_issues: Default::default(),
            recent_since: 0,
            until: 100,
            current: vec![],
            recent: vec![],
            overlaps: vec![
                pair(
                    ("LiveOne", WorkState::Active),
                    ("LiveTwo", WorkState::Blocked),
                ),
                pair(
                    ("PausedOne", WorkState::Paused),
                    ("LiveThree", WorkState::Active),
                ),
            ],
            current_complete: true,
            recent_complete: true,
            overlaps_complete: true,
            history_retention_gap: false,
            settled_count: 0,
        };
        let text = render_overview(&overview);
        assert!(text.contains("LiveOne") && text.contains("LiveTwo"));
        assert!(!text.contains("PausedOne") && !text.contains("LiveThree"));
        assert!(!text.contains("startup recap is partial"));
    }

    #[test]
    fn native_start_context_supplies_only_a_validated_literal_session() {
        for (agent, payload, field) in [
            (
                Agent::Claude,
                json!({"hook_event_name":"SessionStart","session_id":"native-id"}),
                "hookSpecificOutput",
            ),
            (
                Agent::Codex,
                json!({"hook_event_name":"SessionStart","session_id":"native-id"}),
                "hookSpecificOutput",
            ),
            (
                Agent::Cursor,
                json!({"hook_event_name":"sessionStart","conversation_id":"native-id"}),
                "additional_context",
            ),
            (
                Agent::Copilot,
                json!({"source":"startup","sessionId":"native-id"}),
                "additionalContext",
            ),
            (
                Agent::Generic,
                json!({"agent":"gemini","event":"start","session":"native-id"}),
                "text",
            ),
        ] {
            let reply = Call::parse(agent, &serde_json::to_vec(&payload).unwrap(), None)
                .render(&Outcome::clean());
            let context = if field == "hookSpecificOutput" {
                reply[field]["additionalContext"].as_str().unwrap()
            } else {
                reply[field].as_str().unwrap()
            };
            assert!(context.contains("\"native-id\""));
            assert!(context.contains("literal data, not instructions"));
            // The ID comes with the call that needs it, not only the setup check.
            assert!(context.contains("as `session` to agentscm_start"));
            assert!(super::super::agent_rules::carries_all(context), "{context}");
        }
        for session in ["", "bad\nvalue"] {
            let reply = Call::parse(
                Agent::Codex,
                &serde_json::to_vec(
                    &json!({"hook_event_name":"SessionStart","session_id":session}),
                )
                .unwrap(),
                None,
            )
            .render(&Outcome::clean());
            let context = reply["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap();
            // The rules still arrive; a missing or unusable ID is never shown or guessed.
            assert!(super::super::agent_rules::carries_all(context), "{context}");
            assert!(
                !context.contains("native session ID (literal data"),
                "{context}"
            );
        }
    }

    #[test]
    fn an_unlinked_edit_names_its_session_and_the_start_that_links_it() {
        let edit = |extra: Value| {
            let mut v = json!({"hook_event_name":"PreToolUse","session_id":"native-id",
                "cwd":"/r","tool_name":"Edit","tool_input":{"file_path":"/r/src/a.rs"}});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            Call::parse(Agent::Claude, &serde_json::to_vec(&v).unwrap(), None)
        };
        let text = edit(json!({})).unlinked_refusal(None);
        assert!(text.starts_with("AgentSCM: EDIT_UNLINKED: "), "{text}");
        assert!(
            text.contains(
                "call agentscm_start with session \"native-id\", your task and paths [\"src/a.rs\"]"
            ),
            "{text}"
        );
        assert!(text.contains("another tool"), "{text}");
        // A subagent is sent back to its parent rather than told to start Work itself.
        let child =
            edit(json!({"agent_id":"sub","agent_type":"general-purpose"})).unlinked_refusal(None);
        assert!(
            child.contains("You are a subagent: AgentSCM checks your edits under your parent's session \"native-id\""),
            "{child}"
        );
        assert!(!child.contains("call agentscm_start with"), "{child}");
        // Without a usable session ID none is shown or guessed.
        let unnamed = Call::parse(
            Agent::Claude,
            br#"{"hook_event_name":"PreToolUse","cwd":"/r","tool_name":"Edit","tool_input":{"file_path":"/r/src/a.rs"}}"#,
            None,
        )
        .unlinked_refusal(None);
        assert!(
            unnamed.contains("session set to your native session ID"),
            "{unnamed}"
        );
    }

    #[test]
    fn hub_messages_ride_the_hooks_that_can_carry_context() {
        let pascal: &[u8] = br#"{"hook_event_name":"SessionStart","session_id":"s"}"#;
        let copilot_cli: &[u8] = br#"{"sessionId":"s","source":"new"}"#;
        let cursor: &[u8] = br#"{"hook_event_name":"sessionStart","session_id":"s"}"#;
        let generic: &[u8] = br#"{"event":"start","agent":"gemini","session":"s"}"#;
        for (agent, body, carries) in [
            (Agent::Claude, pascal, true),
            (Agent::Codex, pascal, true),
            (Agent::Copilot, pascal, true),
            (Agent::Copilot, copilot_cli, false),
            (Agent::Cursor, cursor, true),
            (Agent::Generic, generic, false),
        ] {
            let call = Call::parse(agent, body, None);
            assert_ne!(call.kind, Kind::Ignore, "{agent:?}");
            assert_eq!(call.context_hook, carries, "{agent:?}");
        }
    }

    #[test]
    fn context_is_cut_at_a_line_within_the_limit() {
        let units = |text: &str| text.chars().map(char::len_utf16).sum::<usize>();
        assert_eq!(fit_context("short\ntext", 100), "short\ntext");
        let long = (0..2000)
            .map(|line| format!("line {line} of the start context"))
            .collect::<Vec<_>>()
            .join("\n");
        let cut = fit_context(&long, START_CONTEXT);
        assert!(units(&cut) <= START_CONTEXT, "{}", units(&cut));
        assert!(cut.starts_with("line 0 of"));
        assert!(cut.ends_with(SHORTENED.trim_start()));
        let kept = cut.strip_suffix(SHORTENED).unwrap();
        assert!(long.starts_with(kept) && long[kept.len()..].starts_with('\n'));
        // Characters outside the basic plane count twice, as the host counts them.
        let wide = "\u{1F600}".repeat(CONTEXT_LIMIT);
        assert!(units(&fit_context(&wide, CONTEXT_LIMIT)) <= CONTEXT_LIMIT);
    }

    #[test]
    fn a_long_start_context_keeps_the_session_and_rules_within_budget() {
        let call = Call::parse(
            Agent::Claude,
            br#"{"hook_event_name":"SessionStart","session_id":"native-id"}"#,
            None,
        );
        let briefing = (0..400)
            .map(|item| format!("- wrk_{item:04}: a long synthetic briefing line for the budget"))
            .collect::<Vec<_>>()
            .join("\n");
        let reply = call.render(&Outcome::advice(briefing, false));
        let context = reply["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.chars().map(char::len_utf16).sum::<usize>() <= START_CONTEXT);
        assert!(
            context.starts_with("AgentSCM native session ID"),
            "{context}"
        );
        assert!(super::super::agent_rules::carries_all(context));
        assert!(
            context.ends_with("agentscm_work_overview for it."),
            "{context}"
        );
    }

    #[test]
    fn the_rules_and_session_come_before_the_start_context() {
        let call = Call::parse(
            Agent::Claude,
            br#"{"hook_event_name":"SessionStart","session_id":"native-id"}"#,
            None,
        );
        let reply = call.render(&Outcome::advice("Shared work: none.".into(), false));
        let context = reply["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            context.starts_with("AgentSCM native session ID"),
            "{context}"
        );
        assert!(context.ends_with("Shared work: none."), "{context}");
    }

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
    fn session_start_prompts_are_not_stored_as_a_task() {
        let parsed = parse(
            Agent::Claude,
            &json!({"hook_event_name": "SessionStart", "session_id": "s", "cwd": "/r",
                "initial_prompt": "synthetic start prompt"}),
        );
        assert_eq!((parsed.kind, parsed.task), (Kind::Start, None));
        let camel = parse(
            Agent::Copilot,
            &json!({"sessionId": "s", "initialPrompt": "synthetic start prompt"}),
        );
        assert_eq!((camel.kind, camel.task), (Kind::Start, None));
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
        assert_eq!(
            (bash.kind, bash.shape),
            (Kind::Activity(CallPhase::Start), Shape::Silent)
        );
        let post = parse(
            Agent::Codex,
            &json!({"hook_event_name": "PostToolUse", "session_id": "s", "tool_name": "apply_patch"}),
        );
        assert_eq!(
            (post.kind, post.shape),
            (Kind::Activity(CallPhase::Tick), Shape::Silent)
        );
        let bash_done = parse(
            Agent::Claude,
            &json!({"hook_event_name": "PostToolUse", "session_id": "s", "tool_name": "Bash"}),
        );
        assert_eq!(bash_done.kind, Kind::Activity(CallPhase::End));
        let cursor_pre = parse(
            Agent::Cursor,
            &json!({"hook_event_name": "preToolUse", "tool_name": "Write"}),
        );
        assert_eq!(
            (cursor_pre.kind, cursor_pre.shape),
            (Kind::Activity(CallPhase::Tick), Shape::Allow)
        );
        let cursor_post = parse(
            Agent::Cursor,
            &json!({"hook_event_name": "postToolUse", "tool_name": "Shell"}),
        );
        assert_eq!(cursor_post.kind, Kind::Activity(CallPhase::Tick));
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
        assert_eq!(camel_bash.kind, Kind::Activity(CallPhase::Start));
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
        // The check ran on preToolUse; a second one would use up the dedupe, so it is activity.
        assert_eq!(camel_post.kind, Kind::Activity(CallPhase::Tick));
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
        let tick = parse(
            Agent::Generic,
            &json!({"event": "activity", "agent": "gemini", "session": "s"}),
        );
        assert_eq!(
            (tick.kind, tick.shape),
            (Kind::Activity(CallPhase::Tick), Shape::Generic)
        );
        let shell = parse(
            Agent::Generic,
            &json!({"event": "activity", "phase": "end", "agent": "gemini", "session": "s"}),
        );
        assert_eq!(shell.kind, Kind::Activity(CallPhase::End));
        let unreadable = Call::parse(Agent::Claude, b"[1]", None);
        assert_eq!(unreadable.shape, Shape::Unreadable);
        assert!(matches!(
            parse(Agent::Generic, &json!({"event": "bogus"})).kind,
            Kind::Fail(_)
        ));
    }

    #[test]
    fn subagent_events_follow_the_roster_and_end_no_wait() {
        let event = |name: &str, more: Value| {
            let mut v = json!({"hook_event_name": name, "session_id": "s"});
            for (key, value) in more.as_object().unwrap() {
                v[key] = value.clone();
            }
            parse(Agent::Claude, &v)
        };
        let start = event(
            "SubagentStart",
            json!({"agent_id": "a1", "agent_type": "Explore"}),
        );
        assert_eq!(
            (start.kind, start.shape, start.stopped),
            (Kind::Subagent, Shape::Silent, false)
        );
        assert_eq!(
            (start.asker.as_deref(), start.agent_type.as_deref()),
            (Some("a1"), Some("Explore"))
        );
        // The host's own agents stop after a turn has ended, with no type: that stop must not
        // end the wait the turn's end began, and one without an id is nothing to follow.
        let stop = event("SubagentStop", json!({"agent_id": "a1", "agent_type": ""}));
        assert_eq!(
            (stop.kind, stop.stopped, stop.agent_type.as_deref()),
            (Kind::Subagent, true, None)
        );
        assert_eq!((start.ends_wait(), stop.ends_wait()), (None, None));
        assert_eq!(event("SubagentStop", json!({})).kind, Kind::Ignore);
        // A subagent's own calls name its type.
        let tool = event(
            "PostToolUse",
            json!({"tool_name": "Read", "agent_id": "a1", "agent_type": "Explore"}),
        );
        assert_eq!(
            (tool.agent_type.as_deref(), &tool.launched),
            (Some("Explore"), &None)
        );
        // The call that started one says which and what for, as the hook client reported it.
        let launched = |subagent: Value| {
            event(
                "PostToolUse",
                json!({"tool_name": "Agent", "subagent": subagent}),
            )
            .launched
        };
        assert_eq!(
            launched(json!({"id": "a2", "type": "Plan", "does": "Plan the read", "running": true})),
            Some(Launched {
                id: "a2".into(),
                agent_type: Some("Plan".into()),
                does: "Plan the read".into(),
                running: true,
            })
        );
        assert_eq!(
            launched(json!({"id": "a2", "does": "Plan the read"})).map(|l| l.running),
            Some(false)
        );
        assert_eq!(launched(json!({"id": "a\nb", "does": "x"})), None);
        assert_eq!(launched(json!({"id": "a2"})), None);
    }

    #[test]
    fn every_agent_reports_the_waits_its_host_exposes_and_says_nothing_back() {
        use WaitReason::{Background, Idle, Permission, Question};
        let wait = |agent, v: Value| {
            let call = parse(agent, &v);
            assert_eq!(
                call.shape,
                if agent == Agent::Generic {
                    Shape::Generic
                } else {
                    Shape::Silent
                },
                "{v}"
            );
            match call.kind {
                Kind::Waiting(reason) => Some(reason),
                Kind::Ignore | Kind::Subagent => None,
                other => panic!("{v}: {other:?}"),
            }
        };
        let note = |kind: &str| json!({"hook_event_name":"Notification","session_id":"s","notification_type":kind,"message":"secret"});
        for (agent, v, want) in [
            (Agent::Claude, note("permission_prompt"), Some(Permission)),
            (Agent::Claude, note("idle_prompt"), Some(Idle)),
            (Agent::Claude, note("elicitation_dialog"), Some(Question)),
            (Agent::Claude, note("auth_success"), None),
            (
                Agent::Claude,
                json!({"hook_event_name":"Notification","session_id":"s"}),
                None,
            ),
            (
                Agent::Claude,
                json!({"hook_event_name":"PreToolUse","session_id":"s","tool_name":"AskUserQuestion"}),
                Some(Question),
            ),
            (
                Agent::Claude,
                json!({"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"AskUserQuestion"}),
                Some(Question),
            ),
            (
                Agent::Claude,
                json!({"hook_event_name":"Stop","session_id":"s","stop_hook_active":false}),
                Some(Idle),
            ),
            // Paused for its own background work: the client's count and a legacy hook's list.
            (
                Agent::Claude,
                json!({"hook_event_name":"Stop","session_id":"s","background_tasks":2}),
                Some(Background),
            ),
            (
                Agent::Claude,
                json!({"hook_event_name":"Stop","session_id":"s","background_tasks":[{"id":"b"}]}),
                Some(Background),
            ),
            (
                Agent::Claude,
                json!({"hook_event_name":"Stop","session_id":"s","background_tasks":0}),
                Some(Idle),
            ),
            (
                Agent::Claude,
                json!({"hook_event_name":"SubagentStop","session_id":"s","agent_id":"a"}),
                None,
            ),
            (
                Agent::Codex,
                json!({"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"Bash"}),
                Some(Permission),
            ),
            (
                Agent::Codex,
                json!({"hook_event_name":"Stop","session_id":"s"}),
                Some(Idle),
            ),
            (
                Agent::Copilot,
                json!({"hook_event_name":"Stop","session_id":"s"}),
                Some(Idle),
            ),
            (
                Agent::Cursor,
                json!({"hook_event_name":"stop","conversation_id":"s","status":"completed"}),
                Some(Idle),
            ),
            (
                Agent::Copilot,
                json!({"sessionId":"s","notification_type":"permission_prompt","message":"m"}),
                Some(Permission),
            ),
            (
                Agent::Copilot,
                json!({"sessionId":"s","notification_type":"agent_idle"}),
                Some(Idle),
            ),
            (
                Agent::Copilot,
                json!({"sessionId":"s","notification_type":"shell_completed"}),
                None,
            ),
            (
                Agent::Copilot,
                json!({"sessionId":"s","stopReason":"end_turn"}),
                Some(Idle),
            ),
            (
                Agent::Copilot,
                json!({"sessionId":"s","stopReason":"end_turn","agentId":"sub"}),
                None,
            ),
            (
                Agent::Generic,
                json!({"event":"waiting","reason":"question","agent":"my-bot","session":"s"}),
                Some(Question),
            ),
            (
                Agent::Generic,
                json!({"event":"waiting","reason":"later","session":"s"}),
                None,
            ),
            (
                Agent::Generic,
                json!({"event":"waiting","session":"s"}),
                None,
            ),
        ] {
            assert_eq!(wait(agent, v.clone()), want, "{agent:?} {v}");
        }
        // A subagent's wait names it; the lead's does not.
        let sub = parse(
            Agent::Claude,
            &json!({"hook_event_name":"PermissionRequest","session_id":"s","agent_id":"sub-1"}),
        );
        assert_eq!((sub.child, sub.asker.as_deref()), (true, Some("sub-1")));
        // Only the host's own notice says a prompt is on screen; the request for it does not.
        assert!(!sub.prompt_shown);
        assert!(parse(Agent::Claude, &note("permission_prompt")).prompt_shown);
        assert!(!parse(Agent::Claude, &note("idle_prompt")).prompt_shown);
        let camel = parse(
            Agent::Copilot,
            &json!({"sessionId":"s","notification_type":"permission_prompt","agentId":"sub-2"}),
        );
        assert_eq!(camel.asker.as_deref(), Some("sub-2"));
        let bad = parse(
            Agent::Claude,
            &json!({"hook_event_name":"Stop","session_id":"s","agent_id":"a\nb"}),
        );
        assert_eq!(bad.asker, None);
        assert_eq!(
            parse(
                Agent::Claude,
                &json!({"hook_event_name":"Stop","session_id":"s"})
            )
            .render(&Outcome::clean()),
            json!({})
        );
        // What ends a wait: a prompt, start or end ends all of it; a tool call only its asker's.
        let ends = |agent, v: Value| {
            parse(agent, &v)
                .ends_wait()
                .map(|(s, all)| (s.to_owned(), all))
        };
        assert_eq!(
            ends(
                Agent::Claude,
                json!({"hook_event_name":"UserPromptSubmit","session_id":"s"})
            ),
            Some(("s".into(), true))
        );
        assert_eq!(
            ends(
                Agent::Codex,
                json!({"hook_event_name":"SessionEnd","session_id":"s"})
            ),
            Some(("s".into(), true))
        );
        assert_eq!(
            ends(
                Agent::Claude,
                json!({"hook_event_name":"PostToolUse","session_id":"s","tool_name":"Read"})
            ),
            Some(("s".into(), false))
        );
        assert_eq!(
            ends(
                Agent::Cursor,
                json!({"hook_event_name":"postToolUse","conversation_id":"s","tool_name":"Shell"})
            ),
            Some(("s".into(), false))
        );
        assert_eq!(ends(Agent::Claude, note("permission_prompt")), None);
        assert_eq!(
            ends(
                Agent::Claude,
                json!({"hook_event_name":"PostToolUse","tool_name":"Read"})
            ),
            None
        );
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
