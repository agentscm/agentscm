//! Sessions: one agent conversation of a token, keyed by (token, session id). It owns at
//! most one intent, to which each call adds paths; every call is also a heartbeat. The
//! backends run the SQL; the shared decisions and the async entry points are here.
use std::collections::{BTreeSet, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::advice;
use super::handoff::HandoffNote;
use super::http::blocking;
use super::intent::{
    Announced, Intent, MAX_LIVE_INTENTS, MAX_TASK_BYTES, NOTICES, NewIntent, Overlap, display_all,
    key_of, spec_key, text,
};
use super::store::{Event, HubEvent, Principal, new_id, unix_now};
use super::sync::{open_pulls, overlapping_pulls};
use super::work::WorkItem;
use super::{HubError, HubState, Result};

pub const SESSION_TTL_SECONDS: i64 = 1200;
pub const DEFAULT_SESSION: &str = "default";
const MAX_SESSION_BYTES: usize = 128;
/// Session rows untouched this long are pruned; their intents expired long before.
pub(super) const STALE_SESSION_SECONDS: i64 = 7 * 86_400;
const DEFAULT_TASK: &str = "task unknown";
/// Files one session holds; a call's own paths always stay.
pub const MAX_SESSION_PATHS: usize = 50;
/// An intent never outlives this since it was created, however often its session is touched.
pub const HOLD_CAP_SECONDS: i64 = 1800;
/// Refusals per session within `DENY_WINDOW_SECONDS`; past it edits pass with the normal advice.
const DENY_LIMIT: usize = 3;
const DENY_WINDOW_SECONDS: i64 = 600;
/// Bounds the stored refused paths of a session.
const MAX_DENIED: usize = 200;
const MAX_VIA_AGENT_BYTES: usize = 32;
pub(super) const NOTICES_PER_TOUCH: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCall {
    /// None: the token's default session, which is its most recently updated session whose `via`
    /// contains a `hook:` mode and was updated within `SESSION_TTL_SECONDS` (so an MCP call joins
    /// the hook-connected session of the same agent); if none, `DEFAULT_SESSION`.
    pub session: Option<String>,
    /// `mcp` or `hook:<agent>`, the latter with 1-32 of [a-z0-9-].
    pub via: String,
    /// Replaces the session's task when Some (1-500 bytes, no control characters).
    pub task: Option<String>,
    /// Files to add (repo-relative; no `dir/**`). May be empty: a pure heartbeat.
    pub paths: Vec<String>,
    /// The caller can refuse the edit (a hook before the edit): a contested file's first
    /// attempt is refused instead of held.
    pub deny: bool,
    /// The caller can show the reply's text to the agent; handoffs are delivered only then.
    pub report: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Touched {
    pub session: String,
    pub via: Vec<String>,
    /// None while the session holds no paths.
    pub intent: Option<Intent>,
    /// Display specs this call added that the session's intent did not already hold (live or expired).
    pub added: Vec<String>,
    /// Overlaps from the announce whose `your_paths` intersect `added`, with `your_paths` cut to `added`.
    pub overlaps: Vec<Overlap>,
    /// `intent_notice` / `pr_notice` events addressed to the session's intent after its cursor
    /// (oldest first, at most 100); the cursor moves past them.
    pub notices: Vec<HubEvent>,
    /// Overlaps (their exact file is one of `your_paths`) that made this call refuse its paths;
    /// they were neither held nor added. Empty when the call was not refused.
    pub denied: Vec<Overlap>,
    /// Files refused earlier that this call repeats: let through, held, no overlap reported.
    pub repeated: Vec<String>,
    /// Contested files passed because the session was refused `DENY_LIMIT` times recently.
    pub rate_limited: bool,
    /// Handoff requests to this session and answers to its requests, each told once; none
    /// while the call was refused.
    pub handoffs: Vec<HandoffNote>,
    /// Refused files this call asked the holder to hand over (best effort).
    pub requested: Vec<String>,
}

pub struct Advice {
    pub touched: Touched,
    /// Hub-rendered warning text; empty when nothing overlaps.
    pub text: String,
    /// Some overlap is an exact file another agent is already editing.
    pub stop: bool,
    /// The refusal reason when the call's edit was refused (then `text` is empty).
    pub deny: Option<String>,
}

/// A call after validation: `keys` are the sorted, deduplicated index keys of its paths.
#[derive(Clone)]
pub(super) struct Checked {
    pub session: Option<String>,
    pub via: String,
    pub task: Option<String>,
    pub keys: Vec<String>,
    pub deny: bool,
    pub report: bool,
}

/// The stored session row.
pub(super) struct Row {
    pub intent: Option<String>,
    pub task: Option<String>,
    /// Canonical JSON array.
    pub via: String,
    pub cursor: i64,
    /// JSON array of the paths whose first contested edit was refused.
    pub denied: String,
    /// JSON array of unix seconds of recent refusals.
    pub deny_times: String,
}

/// The session's current intent when it is live: its id, index keys and creation time.
pub(super) struct Held {
    pub id: String,
    pub keys: Vec<String>,
    pub created_at: i64,
}

/// What `touch_session` writes, decided from the row, the held intent and the call.
pub(super) struct Step {
    /// None while the session would hold no paths.
    pub new: Option<NewIntent>,
    pub keys: Vec<String>,
    /// Display specs of the call that the held intent lacked.
    pub added: Vec<String>,
    pub task: Option<String>,
    pub via: Vec<String>,
    /// The row existed and `via` gained a mode.
    pub gained: bool,
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

pub(super) fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_SESSION_BYTES || name.chars().any(char::is_control) {
        return Err(invalid(
            "session must be 1-128 bytes without control characters",
        ));
    }
    Ok(())
}

pub(super) fn check(call: &SessionCall) -> Result<Checked> {
    let via_ok = call.via == "mcp"
        || call.via.strip_prefix("hook:").is_some_and(|agent| {
            (1..=MAX_VIA_AGENT_BYTES).contains(&agent.len())
                && agent
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        });
    if !via_ok {
        return Err(invalid("via must be mcp or hook:<agent>"));
    }
    if let Some(session) = &call.session {
        check_name(session)?;
    }
    if let Some(task) = &call.task {
        text("task", task, MAX_TASK_BYTES)?;
    }
    if call.paths.len() > MAX_SESSION_PATHS {
        return Err(invalid("a call takes at most 50 paths"));
    }
    if call.paths.iter().any(|spec| spec.ends_with("/**")) {
        return Err(invalid("sessions hold files, not directories"));
    }
    let mut keys = call
        .paths
        .iter()
        .map(|spec| spec_key(spec))
        .collect::<Result<Vec<_>>>()?;
    keys.retain(|key| !never_hold(key));
    keys.sort();
    keys.dedup();
    Ok(Checked {
        session: call.session.clone(),
        via: call.via.clone(),
        task: call.task.clone(),
        keys,
        deny: call.deny,
        report: call.report,
    })
}

const LOCK_NAMES: [&str; 18] = [
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "flake.lock",
    "CHANGELOG.md",
    "CHANGELOG",
    "CHANGES.md",
    "HISTORY.md",
];
const GENERATED_SUFFIXES: [&str; 5] = [".lock", ".min.js", ".min.css", ".pb.go", "_pb2.py"];
/// Skipped at any depth.
const SKIPPED_DIRS: [&str; 3] = ["node_modules", ".git", "__pycache__"];
/// Skipped only at the top: `src/build/` is often real source.
const SKIPPED_ROOTS: [&str; 4] = ["dist", "build", "target", "vendor"];

/// Files that are regenerated, shared by design or never worth a warning: never held, refused or warned about.
pub fn never_hold(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    LOCK_NAMES.contains(&name)
        || GENERATED_SUFFIXES.iter().any(|end| name.ends_with(end))
        || name.contains(".generated.")
        || path.split('/').any(|part| SKIPPED_DIRS.contains(&part))
        || path
            .split_once('/')
            .is_some_and(|(root, _)| SKIPPED_ROOTS.contains(&root))
}

/// `Some` unless the intent was released or has expired (then the call starts a fresh one).
pub(super) fn held(
    id: &str,
    released: bool,
    expires_at: i64,
    created_at: i64,
    now: i64,
    paths_json: &str,
) -> Result<Option<Held>> {
    if released || expires_at <= now {
        return Ok(None);
    }
    let paths: Vec<String> = serde_json::from_str(paths_json)
        .map_err(|error| HubError::Unavailable(error.to_string()))?;
    Ok(Some(Held {
        id: id.to_owned(),
        keys: paths.iter().map(|spec| key_of(spec).to_owned()).collect(),
        created_at,
    }))
}

/// Why an unreleased intent of the session is no longer live: `cap` when it reached its hold
/// cap, else `idle`; None while live or released.
pub(super) fn expiry_cause(
    released: bool,
    expires_at: i64,
    created_at: i64,
    now: i64,
) -> Option<&'static str> {
    if released || expires_at > now {
        return None;
    }
    Some(if expires_at >= created_at + HOLD_CAP_SECONDS {
        "cap"
    } else {
        "idle"
    })
}

pub(super) fn plan(
    row: Option<&Row>,
    held: Option<&Held>,
    call: &Checked,
    now: i64,
) -> Result<Step> {
    let old: BTreeSet<&String> = held.iter().flat_map(|held| &held.keys).collect();
    let added: Vec<&String> = call.keys.iter().filter(|key| !old.contains(key)).collect();
    // shortcut: past 50 files old ones are dropped, so teammates are warned about fewer of this session's paths; raise MAX_SESSION_PATHS or reject when that matters
    let mut union: BTreeSet<&String> = call.keys.iter().collect();
    for key in &old {
        if union.len() >= MAX_SESSION_PATHS {
            break;
        }
        union.insert(key);
    }
    let task = call
        .task
        .clone()
        .or_else(|| row.and_then(|row| row.task.clone()));
    let keys: Vec<String> = union.into_iter().cloned().collect();
    let new = if keys.is_empty() {
        None
    } else {
        Some(NewIntent {
            id: match held {
                Some(held) => held.id.clone(),
                None => new_id("int")?,
            },
            task: task.clone().unwrap_or_else(|| DEFAULT_TASK.to_owned()),
            branch: None,
            base: None,
            machine: None,
            paths: display_all(&keys),
            ttl_seconds: held.map_or(SESSION_TTL_SECONDS, |held| {
                // At least 1: an intent older than the cap (from an earlier version) lapses next.
                SESSION_TTL_SECONDS
                    .min(held.created_at + HOLD_CAP_SECONDS - now)
                    .max(1)
            }),
        })
    };
    let mut via: Vec<String> = match row {
        Some(row) => serde_json::from_str(&row.via)
            .map_err(|error| HubError::Unavailable(error.to_string()))?,
        None => Vec::new(),
    };
    let before = via.len();
    via.push(call.via.clone());
    via.sort();
    via.dedup();
    Ok(Step {
        gained: row.is_some() && via.len() > before,
        new,
        added: display_all(added),
        keys,
        task,
        via,
    })
}

/// The overlaps that involve a spec this call added, with `your_paths` cut to those.
fn only_added(overlaps: Vec<Overlap>, added: &[String]) -> Vec<Overlap> {
    overlaps
        .into_iter()
        .filter_map(|mut overlap| {
            overlap.your_paths.retain(|spec| added.contains(spec));
            (!overlap.your_paths.is_empty()).then_some(overlap)
        })
        .collect()
}

fn mcp_only(via: &[String]) -> bool {
    via.len() == 1 && via[0] == "mcp"
}

/// An MCP-only session has no session id (Claude Code gives MCP servers none), so one of the
/// same token may be the same agent as a hook session; two hook sessions never are.
pub(super) fn same_agent(token: &str, via: &[String], other: &Intent) -> bool {
    other.token == token && (mcp_only(via) || mcp_only(&other.via))
}

/// The other agents' intents that hold, as the very same file, one of `keys`: one overlap each,
/// `their_paths` and `your_paths` being those files. Sorted by intent id.
fn contested(token: &str, via: &[String], keys: &[String], intents: Vec<Intent>) -> Vec<Overlap> {
    let mut found: Vec<Overlap> = intents
        .into_iter()
        // Only session intents refuse; one announced through the intent API stays a note.
        .filter(|intent| !intent.via.is_empty() && !same_agent(token, via, intent))
        .filter_map(|intent| {
            let both: Vec<String> = keys
                .iter()
                .filter(|key| intent.paths.contains(key))
                .cloned()
                .collect();
            (!both.is_empty()).then(|| Overlap {
                intent,
                their_paths: both.clone(),
                your_paths: both,
            })
        })
        .collect();
    found.sort_by(|a, b| a.intent.id.cmp(&b.intent.id));
    found
}

/// What to do with a call's new files, given the session's refusals so far.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Decision {
    /// Contested files to refuse now; when non-empty the whole call is refused.
    pub deny: Vec<String>,
    /// Files refused before that this call repeats.
    pub repeat: Vec<String>,
    /// Contested files that pass because the session was refused too often recently.
    pub rate_limited: bool,
}

/// `added` are the call's new files, `contested` those of them another agent holds that were
/// not refused before (the others are repeats).
pub(super) fn decide(
    denied: &[String],
    times: &[i64],
    added: &[String],
    contested: &[String],
    now: i64,
) -> Decision {
    let repeat: Vec<String> = added
        .iter()
        .filter(|key| denied.contains(key))
        .cloned()
        .collect();
    let recent = times.iter().filter(|at| **at > now - DENY_WINDOW_SECONDS);
    let limited = recent.count() >= DENY_LIMIT;
    if contested.is_empty() || limited {
        return Decision {
            deny: Vec::new(),
            repeat,
            rate_limited: limited && !contested.is_empty(),
        };
    }
    // A refused call applies nothing, so its repeats stay refused.
    Decision {
        deny: contested.to_vec(),
        repeat: Vec::new(),
        rate_limited: false,
    }
}

/// A touch's plan after the warn-once check.
pub(super) struct Guarded {
    pub step: Step,
    denied: Vec<Overlap>,
    repeated: Vec<String>,
    rate_limited: bool,
    /// The session row's new `denied` and `deny_times`.
    pub denied_json: String,
    pub times_json: String,
}

fn list<T: DeserializeOwned>(json: &str) -> Result<Vec<T>> {
    serde_json::from_str(json).map_err(|error| HubError::Unavailable(error.to_string()))
}

/// Warn-once for a deny-capable call: a contested file's first attempt is refused (the call then
/// only heartbeats the held intent), a repeat of a refused file passes. `load` returns the live
/// intents of other holders overlapping the keys it is given.
pub(super) fn guard(
    token: &str,
    row: Option<&Row>,
    held: Option<&Held>,
    step: Step,
    call: &Checked,
    now: i64,
    load: impl FnOnce(&[String]) -> Result<Vec<Intent>>,
) -> Result<Guarded> {
    let mut denied: Vec<String> = row.map_or(Ok(Vec::new()), |row| list(&row.denied))?;
    let mut times: Vec<i64> = row.map_or(Ok(Vec::new()), |row| list(&row.deny_times))?;
    times.retain(|at| *at > now - DENY_WINDOW_SECONDS);
    let mut out = Guarded {
        step,
        denied: Vec::new(),
        repeated: Vec::new(),
        rate_limited: false,
        denied_json: String::new(),
        times_json: String::new(),
    };
    if call.deny && !out.step.added.is_empty() {
        // Files hold no directories, so a display spec is its key.
        let fresh: Vec<String> = out
            .step
            .added
            .iter()
            .filter(|key| !denied.contains(key))
            .cloned()
            .collect();
        let found = if fresh.is_empty() {
            Vec::new()
        } else {
            contested(token, &out.step.via, &fresh, load(&fresh)?)
        };
        let keys: BTreeSet<String> = found
            .iter()
            .flat_map(|overlap| overlap.your_paths.clone())
            .collect();
        let keys: Vec<String> = keys.into_iter().collect();
        let decision = decide(&denied, &times, &out.step.added, &keys, now);
        if decision.deny.is_empty() {
            out.repeated = decision.repeat;
            out.rate_limited = decision.rate_limited;
        } else {
            out.step = plan(
                row,
                held,
                &Checked {
                    keys: Vec::new(),
                    ..call.clone()
                },
                now,
            )?;
            out.denied = found;
            denied.extend(decision.deny);
            if denied.len() > MAX_DENIED {
                denied.drain(..denied.len() - MAX_DENIED);
            }
            times.push(now);
        }
    }
    out.denied_json = json!(denied).to_string();
    out.times_json = json!(times).to_string();
    Ok(out)
}

impl Guarded {
    /// The call's paths were refused.
    pub(super) fn refused(&self) -> bool {
        !self.denied.is_empty()
    }

    /// The files the refused call was refused for, in key order.
    pub(super) fn denied_keys(&self) -> Vec<String> {
        let keys: BTreeSet<&String> = self.denied.iter().flat_map(|o| &o.your_paths).collect();
        keys.into_iter().cloned().collect()
    }

    /// The events of the check, to append in the touch's transaction.
    pub(super) fn events(&self, token: &str, session: &str) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        if !self.denied.is_empty() {
            events.push(Event::edit_denied(token, session, &self.denied)?);
        }
        if !self.repeated.is_empty() {
            events.push(Event::edit_repeated(token, session, &self.repeated)?);
        }
        Ok(events)
    }

    /// The result of the call; `announced` is the announce of `step.new`, if one was made.
    pub(super) fn touched(
        self,
        token: &str,
        session: &str,
        announced: Option<Announced>,
        notices: Vec<HubEvent>,
        handoffs: Vec<HandoffNote>,
        requested: Vec<String>,
    ) -> Touched {
        let Self {
            step,
            denied,
            repeated,
            rate_limited,
            ..
        } = self;
        let (intent, overlaps) = match announced {
            Some(announced) => {
                let mut intent = announced.intent;
                // Read before the session row was written, so it cannot show this call's modes.
                intent.via.clone_from(&step.via);
                let reported: Vec<String> = step
                    .added
                    .iter()
                    .filter(|spec| !repeated.contains(spec))
                    .cloned()
                    .collect();
                let mut overlaps = only_added(announced.overlaps, &reported);
                overlaps.retain(|overlap| !same_agent(token, &step.via, &overlap.intent));
                (Some(intent), overlaps)
            }
            None => (None, Vec::new()),
        };
        Touched {
            session: session.to_owned(),
            via: step.via,
            intent,
            added: step.added,
            overlaps,
            notices,
            denied,
            repeated,
            rate_limited,
            handoffs,
            requested,
        }
    }
}

impl Event {
    fn session(kind: &'static str, token: &str, session: &str, via: &[String]) -> Result<Self> {
        let data = json!({"token": token, "session": session, "via": via});
        Self::new(kind, token, data)
    }

    pub(super) fn session_started(token: &str, session: &str, via: &[String]) -> Result<Self> {
        Self::session("session_started", token, session, via)
    }

    pub(super) fn session_connected(token: &str, session: &str, via: &[String]) -> Result<Self> {
        Self::session("session_connected", token, session, via)
    }

    pub(super) fn session_ended(token: &str, session: &str) -> Result<Self> {
        let data = json!({"token": token, "session": session});
        Self::new("session_ended", token, data)
    }

    fn edit_denied(token: &str, session: &str, denied: &[Overlap]) -> Result<Self> {
        let paths: BTreeSet<&String> = denied.iter().flat_map(|o| &o.your_paths).collect();
        let holders: Vec<&String> = denied.iter().map(|o| &o.intent.id).collect();
        let data = json!({"token": token, "session": session, "paths": paths, "holders": holders});
        Self::new("edit_denied", token, data)
    }

    fn edit_repeated(token: &str, session: &str, paths: &[String]) -> Result<Self> {
        let data = json!({"token": token, "session": session, "paths": paths});
        Self::new("edit_repeated", token, data)
    }

    pub(super) fn hold_expired(
        token: &str,
        session: &str,
        intent: &str,
        cause: &str,
    ) -> Result<Self> {
        let data = json!({"token": token, "session": session, "intent": intent, "cause": cause});
        Self::new("hold_expired", token, data)
    }
}

/// One code path for MCP and hooks: touch the session, look up overlapping PRs for `added`, render advice.
pub async fn announce_for(
    state: &HubState,
    principal: &Principal,
    call: SessionCall,
) -> Result<Advice> {
    let store = state.store.clone();
    let token = principal.token_id.clone();
    let touched = blocking(state, move || {
        store.touch_session(&token, &call, unix_now())
    })
    .await?;
    NOTICES.notify_waiters();
    count(&DENIED, !touched.denied.is_empty());
    count(&REPEATED, !touched.repeated.is_empty());
    count(&RATE_LIMITED, touched.rate_limited);
    let mut pulls = Vec::new();
    let mut unknown = None;
    if !touched.added.is_empty() {
        // The touch is committed; a failed look at pull requests must not fail it.
        match overlapping_pulls(
            state,
            &principal.owner.identity.host,
            &principal.repository,
            principal.repository_id,
            &touched.added,
        )
        .await
        {
            Ok(found) => pulls = found,
            Err(error) => {
                eprintln!("hub: pull requests for a session touch: {error}");
                unknown = Some(advice::unknown("pull requests could not be read"));
            }
        }
    }
    let (mut text, stop) = advice::warn(&touched, &pulls, unix_now());
    if let Some(unknown) = unknown {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&unknown);
    }
    let deny = (!touched.denied.is_empty()).then(|| {
        advice::deny_reason(
            &touched.denied,
            // Say the holder was asked only when every refused file was.
            (
                Some(&touched.session),
                !touched.requested.is_empty()
                    && touched
                        .denied
                        .iter()
                        .flat_map(|overlap| &overlap.your_paths)
                        .all(|path| touched.requested.contains(path)),
            ),
            unix_now(),
        )
    });
    Ok(Advice {
        touched,
        text,
        stop,
        deny,
    })
}

static DENIED: AtomicU64 = AtomicU64::new(0);
static REPEATED: AtomicU64 = AtomicU64::new(0);
static RATE_LIMITED: AtomicU64 = AtomicU64::new(0);
static FAIL_OPEN: AtomicU64 = AtomicU64::new(0);
const LATENCIES: usize = 1024;
/// Milliseconds of the last `LATENCIES` hook requests.
static HOOK_MS: Mutex<VecDeque<u32>> = Mutex::new(VecDeque::new());

fn count(counter: &AtomicU64, when: bool) {
    if when {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// A hook answered "coordination unknown" (and so let the edit through).
pub(super) fn count_fail_open() {
    count(&FAIL_OPEN, true);
}

pub(super) fn record_hook_ms(ms: u32) {
    let mut times = HOOK_MS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if times.len() == LATENCIES {
        times.pop_front();
    }
    times.push_back(ms);
}

/// Counters of this process since it started, for the health reply.
pub(super) fn coordination() -> Value {
    let mut times: Vec<u32> = HOOK_MS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .copied()
        .collect();
    times.sort_unstable();
    let p95 = times.len().saturating_mul(95).div_ceil(100).checked_sub(1);
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    json!({"denied": load(&DENIED), "repeated": load(&REPEATED),
        "rate_limited": load(&RATE_LIMITED), "fail_open": load(&FAIL_OPEN),
        "hook_p95_ms": p95.map_or(0, |index| times[index])})
}

/// Who else is active (live intents in the token's repository, excluding `own_intent`), rendered.
pub async fn others_for(
    state: &HubState,
    principal: &Principal,
    own_intent: Option<&str>,
) -> Result<String> {
    let store = state.store.clone();
    let repository = principal.repository.clone();
    let intents = blocking(state, move || {
        store.live_intents(&repository, unix_now(), MAX_LIVE_INTENTS)
    })
    .await?;
    Ok(advice::others(
        &intents,
        own_intent,
        &principal.repository,
        unix_now(),
    ))
}

/// Other agents' ongoing work (`current`) and open PR titles that read like `task`: one
/// `task_overlap` event and the rendered warning. Empty when nothing matches.
pub async fn duplicates_for(
    state: &HubState,
    principal: &Principal,
    task: &str,
    current: &[WorkItem],
) -> Result<String> {
    let host = &principal.owner.identity.host;
    // A failed look at pull requests leaves only the agents to compare.
    let pulls = open_pulls(state, host, &principal.repository, principal.repository_id)
        .await
        .unwrap_or_else(|error| {
            eprintln!("hub: open pull requests for a start: {error}");
            Vec::new()
        });
    let same: Vec<&WorkItem> = current
        .iter()
        .filter(|item| item.lead_token != principal.token_id && advice::similar(task, &item.title))
        .collect();
    let prs: Vec<&Value> = pulls
        .iter()
        .filter(|pr| advice::similar(task, pr["title"].as_str().unwrap_or_default()))
        .collect();
    if same.is_empty() && prs.is_empty() {
        return Ok(String::new());
    }
    let work_ids: Vec<&str> = same.iter().map(|item| item.id.as_str()).collect();
    let numbers: Vec<&Value> = prs.iter().map(|pr| &pr["number"]).collect();
    let data = json!({"task": task, "work": work_ids, "pulls": numbers}).to_string();
    let (store, token) = (state.store.clone(), principal.token_id.clone());
    blocking(state, move || {
        store.append_notices("task_overlap", &[(token, data)])
    })
    .await?;
    Ok(advice::duplicates(task, &same, &prs, unix_now()))
}

/// Touch (a heartbeat when `call` has no paths), then the others, open PRs and notices, rendered.
pub async fn status_for(
    state: &HubState,
    principal: &Principal,
    call: SessionCall,
) -> Result<String> {
    let advice = announce_for(state, principal, call).await?;
    let own = advice
        .touched
        .intent
        .as_ref()
        .map(|intent| intent.id.as_str());
    let mut parts = Vec::new();
    if !advice.text.is_empty() {
        parts.push(advice.text.clone());
    }
    // The touch is committed; a failed listing must not turn the status into an error.
    parts.push(match others_for(state, principal, own).await {
        Ok(others) => others,
        Err(error) => {
            eprintln!("hub: other agents for a status: {error}");
            advice::unknown("other agents could not be listed")
        }
    });
    match open_pulls(
        state,
        &principal.owner.identity.host,
        &principal.repository,
        principal.repository_id,
    )
    .await
    {
        Ok(pulls) => parts.push(advice::pulls(&pulls)),
        Err(error) => {
            eprintln!("hub: open pull requests for a status: {error}");
            parts.push(advice::unknown("pull requests could not be read"));
        }
    }
    Ok(parts.join("\n"))
}

/// Release the session's intent and forget the session. False when there was none.
pub async fn done_for(
    state: &HubState,
    principal: &Principal,
    session: Option<String>,
) -> Result<bool> {
    if let Some(session) = &session {
        check_name(session)?;
    }
    let store = state.store.clone();
    let token = principal.token_id.clone();
    blocking(state, move || {
        store.end_session(&token, session.as_deref(), unix_now())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(via: &str, paths: &[&str]) -> SessionCall {
        SessionCall {
            session: None,
            via: via.into(),
            task: None,
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            deny: false,
            report: true,
        }
    }

    #[test]
    fn calls_are_validated() {
        let ok = check(&call("hook:claude-code", &["b.rs", "a.rs", "a.rs"])).unwrap();
        assert_eq!(ok.keys, ["a.rs", "b.rs"]);
        assert!(check(&call("mcp", &[])).is_ok());
        let long = "a".repeat(129);
        let mut bad = vec![
            call("", &[]),
            call("hook:", &[]),
            call("hook:Claude", &[]),
            call("hook:a b", &[]),
            call(&format!("hook:{}", "a".repeat(33)), &[]),
            call("http", &[]),
            call("mcp", &["../x"]),
            call("mcp", &["dir/**"]),
        ];
        bad.push(SessionCall {
            session: Some(long),
            ..call("mcp", &[])
        });
        bad.push(SessionCall {
            session: Some("a\nb".into()),
            ..call("mcp", &[])
        });
        bad.push(SessionCall {
            task: Some(String::new()),
            ..call("mcp", &[])
        });
        let many: Vec<String> = (0..257).map(|i| format!("p{i}")).collect();
        bad.push(SessionCall {
            paths: many,
            ..call("mcp", &[])
        });
        for case in bad {
            assert!(check(&case).is_err(), "{case:?}");
        }
    }

    #[test]
    fn plan_adds_dedupes_and_caps() {
        let held = Held {
            id: "int_x".into(),
            keys: vec!["a.rs".into(), "c.rs".into()],
            created_at: 100,
        };
        let step = plan(
            None,
            Some(&held),
            &check(&call("mcp", &["a.rs", "b.rs"])).unwrap(),
            200,
        )
        .unwrap();
        assert_eq!(step.added, ["b.rs"]);
        let new = step.new.unwrap();
        assert_eq!(new.id, "int_x");
        assert_eq!(new.paths, ["a.rs", "b.rs", "c.rs"]);
        assert_eq!(new.task, DEFAULT_TASK);
        assert_eq!(new.ttl_seconds, SESSION_TTL_SECONDS);
        let none = plan(None, None, &check(&call("mcp", &[])).unwrap(), 200).unwrap();
        assert!(none.new.is_none() && none.added.is_empty());

        // The hold cap shortens the expiry of an old intent, never below a second.
        let beat = check(&call("mcp", &[])).unwrap();
        let ttl = |now| {
            plan(None, Some(&held), &beat, now)
                .unwrap()
                .new
                .unwrap()
                .ttl_seconds
        };
        assert_eq!((ttl(1000), ttl(1800), ttl(5000)), (900, 100, 1));

        // Over the cap every call path stays and old ones are dropped.
        let old: Vec<String> = (0..50).map(|i| format!("old{i:03}")).collect();
        let held = Held {
            id: "int_x".into(),
            keys: old,
            created_at: 100,
        };
        let step = plan(
            None,
            Some(&held),
            &check(&call("mcp", &["n1", "n2"])).unwrap(),
            200,
        )
        .unwrap();
        let paths = step.new.unwrap().paths;
        assert_eq!(paths.len(), MAX_SESSION_PATHS);
        assert!(paths.contains(&"n1".to_owned()) && paths.contains(&"n2".to_owned()));
    }

    #[test]
    fn via_accumulates() {
        let row = Row {
            intent: None,
            task: Some("t".into()),
            via: r#"["hook:claude"]"#.into(),
            cursor: 0,
            denied: "[]".into(),
            deny_times: "[]".into(),
        };
        let step = plan(Some(&row), None, &check(&call("mcp", &[])).unwrap(), 0).unwrap();
        assert_eq!(
            (step.via.as_slice(), step.gained),
            (
                ["hook:claude".to_owned(), "mcp".to_owned()].as_slice(),
                true
            )
        );
        assert_eq!(step.task.as_deref(), Some("t"));
        let again = plan(
            Some(&row),
            None,
            &check(&call("hook:claude", &[])).unwrap(),
            0,
        )
        .unwrap();
        assert!(!again.gained);
    }

    #[test]
    fn never_hold_table() {
        for held in [
            "src/a.rs",
            "docs/changelog.md",
            "src/builder.rs",
            "lock.rs",
            "Cargo.toml",
            "src/generated.rs",
            "src/build/mod.rs",
            "internal/target/rules.go",
            "pkg/vendor/x.go",
        ] {
            assert!(!never_hold(held), "{held}");
        }
        for skipped in [
            "Cargo.lock",
            "web/package-lock.json",
            "npm-shrinkwrap.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "bun.lockb",
            "bun.lock",
            "poetry.lock",
            "Pipfile.lock",
            "uv.lock",
            "Gemfile.lock",
            "composer.lock",
            "go.sum",
            "flake.lock",
            "CHANGELOG.md",
            "sub/CHANGELOG",
            "CHANGES.md",
            "HISTORY.md",
            "x/any.lock",
            "app.min.js",
            "app.min.css",
            "api/x.pb.go",
            "x_pb2.py",
            "src/api.generated.ts",
            "node_modules/a/b.js",
            "dist/b.js",
            "build/x",
            "target/debug/x",
            "vendor/x.go",
            ".git/config",
            "src/__pycache__/x.pyc",
        ] {
            assert!(never_hold(skipped), "{skipped}");
        }
        let kept = check(&call("mcp", &["Cargo.lock", "src/a.rs"])).unwrap();
        assert_eq!(kept.keys, ["src/a.rs"]);
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn decisions_refuse_once_repeat_and_rate_limit() {
        let (a, b) = (strings(&["a.rs"]), strings(&["a.rs", "b.rs"]));
        // Nothing contested: nothing refused, a refused file in the call is a repeat.
        let free = decide(&[], &[], &b, &[], 1000);
        assert_eq!((free.deny.is_empty(), free.repeat.is_empty()), (true, true));
        let again = decide(&a, &[900], &b, &[], 1000);
        assert_eq!((again.deny.is_empty(), again.repeat), (true, a.clone()));
        // A contested file is refused, and then its repeats do not pass in the same call.
        let first = decide(&a, &[], &b, &strings(&["b.rs"]), 1000);
        assert_eq!((first.deny, first.repeat), (strings(&["b.rs"]), vec![]));
        // Three refusals in ten minutes: the fourth passes, the window slides.
        let times = [100, 200, 300];
        let limited = decide(&[], &times, &a, &a, 400);
        assert!(limited.deny.is_empty() && limited.rate_limited);
        let later = decide(&[], &times, &a, &a, 100 + DENY_WINDOW_SECONDS);
        assert_eq!(later.deny, a);
        assert!(!decide(&[], &times, &a, &[], 400).rate_limited);
    }

    #[test]
    fn the_same_agent_is_an_mcp_only_session_of_the_same_token() {
        let theirs = |token: &str, via: &[&str]| {
            let mut intent = crate::hub::advice::test_intent("int_a", "t", &["a.rs"]);
            intent.token = token.into();
            intent.via = strings(via);
            intent
        };
        let hook = strings(&["hook:claude"]);
        assert!(same_agent("tok_1", &hook, &theirs("tok_1", &["mcp"])));
        assert!(same_agent(
            "tok_1",
            &strings(&["mcp"]),
            &theirs("tok_1", &["hook:codex"])
        ));
        assert!(!same_agent(
            "tok_1",
            &hook,
            &theirs("tok_1", &["hook:codex"])
        ));
        assert!(!same_agent(
            "tok_1",
            &hook,
            &theirs("tok_1", &["hook:codex", "mcp"])
        ));
        assert!(!same_agent("tok_1", &hook, &theirs("tok_2", &["mcp"])));
        assert!(!same_agent("tok_1", &hook, &theirs("tok_1", &[])));
        let keys = strings(&["a.rs", "b.rs"]);
        // An intent without a session (the intent API) never refuses.
        let held = vec![
            theirs("tok_2", &[]),
            theirs("tok_1", &["mcp"]),
            theirs("tok_2", &["hook:codex"]),
        ];
        let found = contested("tok_1", &hook, &keys, held);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].your_paths, ["a.rs"]);
    }
}
