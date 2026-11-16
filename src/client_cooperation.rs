//! Same-binary maintenance: retry reports, observe qualified exits, and refresh dependencies.
use std::path::Path;
use std::time::Duration;

use agentscm::hub::forge_http::{self, send};
use agentscm::hub::reply;
use agentscm::hub::session_lifecycle::{
    EndReason, LifecycleEnd, LifecycleRegister, LifecycleState, LifecycleView, Qualification,
    WorkFence,
};
use agentscm::hub::store::unix_now;
use agentscm::{Error, Result};
use serde_json::{Value, json};

use crate::client_config::{Connection, is_current, onboarding_client};
use crate::client_journal::{self, Kind, Phase, Record};
use crate::client_session::{self, HostIdentity, HostObservation, SessionContext};

const CAPABILITY_SECONDS: i64 = 300;
const RESPONSE_BYTES: usize = 256 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

fn invalid() -> Error {
    Error::Invalid("native cooperation state is unavailable or changed".into())
}

fn binding(connection: &Connection) -> Option<(String, String)> {
    let token = crate::client_cli::read_token(&connection.token_file).ok()?;
    let binding = onboarding_client(&connection.root, &token);
    Some((
        connection
            .owned_token_id
            .clone()
            .unwrap_or_else(|| binding.clone()),
        binding,
    ))
}

fn matches(connection: &Connection, context: &SessionContext) -> bool {
    connection.root == context.root
        && connection.hub == context.hub
        && connection.repo == context.repo
        && binding(connection).is_some_and(|(credential, binding)| {
            credential == context.credential_id && binding == context.binding_id
        })
}

pub(crate) fn transport_context(connection: &Connection, session: &str) -> Option<SessionContext> {
    let config = crate::hub_cli::config_dir().ok()?;
    let context =
        client_session::context(&config, session, &connection.hub, &connection.repo).ok()??;
    let now = unix_now();
    (matches(connection, &context)
        && !context.terminal
        && !context.revoked
        && context.registered
        && context.protocol == 1
        && context.incarnation.is_some()
        && context.protocol_checked_at <= now
        && now - context.protocol_checked_at <= CAPABILITY_SECONDS)
        .then_some(context)
}

/// A protocol-1 wait reply is fresh capability evidence; dispatch still needs the exact
/// saved credential, checkout and incarnation. Historical outcomes may be reported after end.
pub(crate) fn delivery_context(
    connection: &Connection,
    session: &str,
    incarnation: &str,
) -> Option<SessionContext> {
    let saved = crate::client_config::load().ok()?;
    if saved.iter().find(|saved| saved.root == connection.root) != Some(connection)
        || !is_current(connection)
    {
        return None;
    }
    let config = crate::hub_cli::config_dir().ok()?;
    let context =
        client_session::context(&config, session, &connection.hub, &connection.repo).ok()??;
    (matches(connection, &context)
        && !context.revoked
        && context.registered
        && context.protocol == 1
        && context.incarnation.as_deref() == Some(incarnation))
    .then_some(context)
}

/// End transport uses the identity observed at hook entry, never a later start's metadata.
pub(crate) fn end_transport_context(
    connection: &Connection,
    contexts: &[SessionContext],
    complete: bool,
) -> Result<Option<SessionContext>> {
    let mut matching = contexts
        .iter()
        .filter(|context| matches(connection, context));
    let Some(context) = matching.next() else {
        // The caller sends an explicit Hub-side absence fence, never an unfenced end.
        return if complete { Ok(None) } else { Err(invalid()) };
    };
    if matching.next().is_some() || context.revoked {
        return Err(invalid());
    }
    if context.protocol != 1 {
        return Ok(None);
    }
    if !context.registered || context.incarnation.is_none() {
        return Err(invalid());
    }
    Ok(Some(context.clone()))
}

async fn request(
    connection: &Connection,
    route: &str,
    body: Option<&Value>,
) -> Result<(u16, Value)> {
    if !is_current(connection) {
        return Err(invalid());
    }
    let token = crate::client_cli::read_token(&connection.token_file)?;
    let client = forge_http::client().map_err(crate::hub_cli::map_hub_error)?;
    let url = format!("{}{route}", connection.hub);
    let builder = match body {
        Some(body) => client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(body)?),
        None => client.get(url),
    }
    .bearer_auth(token)
    .timeout(REQUEST_TIMEOUT);
    send(builder, RESPONSE_BYTES)
        .await
        .map_err(crate::hub_cli::map_hub_error)
}

fn unsupported(status: u16, reply: &Value) -> bool {
    status == 404
        || status == 405
        || status == 422
        || (status == 400
            && (reply.to_string().contains("unknown field")
                || reply.to_string().contains("unsupported")))
}

fn response_state(context: &mut SessionContext, status: u16, reply: &Value, now: i64) {
    if matches!(status, 401 | 403) {
        context.revoked = true;
    }
    if unsupported(status, reply) {
        context.protocol = 0;
        context.protocol_checked_at = now;
    }
}

async fn negotiate(connection: &Connection, context: &mut SessionContext) -> Result<()> {
    if context.revoked {
        return Err(invalid());
    }
    let now = unix_now();
    if context.protocol_checked_at > 0
        && context.protocol_checked_at <= now
        && now - context.protocol_checked_at < CAPABILITY_SECONDS
    {
        return Ok(());
    }
    let (status, value) = request(connection, "/v1/hub/whoami", None).await?;
    response_state(context, status, &value, now);
    if status != 200
        || value["token"]["repository"].as_str() != Some(connection.repo.as_str())
        || value["token"]["read_only"] != false
    {
        return Err(invalid());
    }
    context.protocol = if value["cooperation_protocol"] == 1 {
        1
    } else {
        0
    };
    context.protocol_checked_at = now;
    Ok(())
}

fn payload(context: &SessionContext, endpoint: &str, body: Value) -> Value {
    let observed = body
        .get("observed_at")
        .cloned()
        .unwrap_or_else(|| json!(context.observed_at));
    json!({"hub": context.hub,"repo":context.repo,"root":context.root,
        "credential_id":context.credential_id,"binding_id":context.binding_id,
        "session":context.session,"incarnation":context.incarnation,
        "start_nonce":context.start_nonce,"endpoint":endpoint,"body":body,"observed_at":observed})
}

/// A retry reuses the original operation, including its observation and exact Work fences.
fn operation(config: &Path, candidate: Record) -> Result<Record> {
    if let Some(original) = client_journal::inspect(config)?
        .records
        .into_iter()
        .find(|record| record.id == candidate.id)
    {
        if original.kind != candidate.kind
            || [
                "hub",
                "repo",
                "root",
                "credential_id",
                "binding_id",
                "session",
                "start_nonce",
                "endpoint",
            ]
            .iter()
            .any(|key| original.payload[*key] != candidate.payload[*key])
        {
            return Err(invalid());
        }
        if end_intent(&original)
            && candidate.kind == Kind::TerminalEnd
            && candidate.payload["body"].is_object()
        {
            if candidate.payload["incarnation"] != original.payload["incarnation"]
                || candidate.payload["body"]["observed_at"] != original.payload["observed_at"]
            {
                return Err(invalid());
            }
            let mut admitted = original.clone();
            admitted.phase = Phase::Admitted;
            admitted.payload["body"] = candidate.payload["body"].clone();
            admitted
                .payload
                .as_object_mut()
                .ok_or_else(invalid)?
                .remove("end_intent");
            return client_journal::compare_and_update(config, &original, &admitted)
                .map_err(Into::into);
        }
        return Ok(original);
    }
    client_journal::put(config, &candidate)?;
    Ok(candidate)
}

fn end_intent(record: &Record) -> bool {
    record.kind == Kind::TerminalEnd
        && record.phase == Phase::Prepared
        && record.payload["end_intent"] == true
        && record.payload["body"].is_null()
}

/// A genuine end is retained before the native hook's network deadline; it grants no new authority.
pub(crate) fn remember_end(
    config: &Path,
    agent: &str,
    fence: &client_session::CleanupFence,
    at: i64,
) -> Result<()> {
    for mut context in fence
        .end_contexts()
        .iter()
        .filter(|context| context.agent == agent && !context.terminal && !context.revoked)
        .cloned()
    {
        context.explicit_end_requested = true;
        observe_exit(&mut context, at);
        let saved = client_session::save_end_context(config, fence, &context)?;
        let current = saved.is_some();
        if let Some(updated) = saved {
            context = updated;
        }
        if context.registered || !current {
            let mut intent = payload(&context, "/v1/hub/cooperation/end", Value::Null);
            intent["observed_at"] = json!(context.exit_observed_at.ok_or_else(invalid)?);
            intent["end_intent"] = json!(true);
            if !current {
                intent["disposition"] = json!("superseded");
            }
            let id = match context.incarnation.as_ref().filter(|_| context.registered) {
                Some(incarnation) => format!("end-{incarnation}"),
                None => format!("end-start-{}", context.start_nonce),
            };
            operation(
                config,
                Record {
                    id,
                    kind: Kind::TerminalEnd,
                    phase: if current {
                        Phase::Prepared
                    } else {
                        Phase::Outcome
                    },
                    payload: intent,
                },
            )?;
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Report {
    Applied,
    Pending,
    RefusedEnd,
    Stalled(&'static str),
}

fn apply_reply(
    context: &mut SessionContext,
    kind: Kind,
    status: u16,
    reply: Value,
    now: i64,
) -> Result<Report> {
    response_state(context, status, &reply, now);
    if context.revoked {
        return Ok(Report::Stalled("revoked"));
    }
    if unsupported(status, &reply) {
        return Ok(Report::Pending);
    }
    if status != 200 {
        let code = reply["code"]
            .as_str()
            .or_else(|| reply["error"]["code"].as_str());
        return Ok(
            if code == Some("invalid_or_changed") && kind == Kind::TerminalEnd {
                Report::RefusedEnd
            } else if code == Some("invalid_or_changed") {
                Report::Stalled("changed")
            } else {
                Report::Pending
            },
        );
    }
    if kind == Kind::Registration {
        let view: LifecycleView = reply::decode(&reply)?;
        if view.session != context.session
            || view.start_nonce != context.start_nonce
            || view.state != LifecycleState::Live
        {
            return Err(invalid());
        }
        context.incarnation = Some(view.incarnation);
        context.registered = true;
        context.qualification = Some(view.qualification);
    } else if kind == Kind::TerminalEnd {
        let view: LifecycleView = reply::decode(&reply["lifecycle"])?;
        if view.session != context.session
            || view.start_nonce != context.start_nonce
            || context.incarnation.as_deref() != Some(view.incarnation.as_str())
            || view.state != LifecycleState::Terminal
        {
            return Err(invalid());
        }
        context.terminal = true;
    }
    Ok(Report::Applied)
}

async fn report(
    connection: &Connection,
    context: &mut SessionContext,
    record: &Record,
) -> Result<Report> {
    if context.revoked {
        return Ok(Report::Stalled("revoked"));
    }
    if record.payload["disposition"].is_string() {
        return Ok(Report::Stalled("retained"));
    }
    let route = match record.kind {
        Kind::Registration => "/v1/hub/cooperation/register",
        Kind::TerminalEnd => "/v1/hub/cooperation/end",
        Kind::Enqueue => "/v1/hub/cooperation/transport",
    };
    if record.payload["endpoint"] != route
        || !matches(connection, context)
        || record.payload["binding_id"] != context.binding_id
        || record.payload["credential_id"] != context.credential_id
        || record.payload["session"] != context.session
    {
        return Ok(Report::Stalled("superseded"));
    }
    let mut body = record.payload["body"].clone();
    if record.kind == Kind::Registration {
        if body["start_nonce"] != context.start_nonce {
            return Ok(Report::Stalled("superseded"));
        }
    } else if body["incarnation"].as_str() != context.incarnation.as_deref() {
        return Ok(Report::Stalled("superseded"));
    }
    if record.kind == Kind::Enqueue && record.phase != Phase::Outcome {
        body["outcome"] = json!("unknown");
    }
    let (status, reply) = request(connection, route, Some(&body)).await?;
    apply_reply(context, record.kind, status, reply, unix_now())
}

fn settle(config: &Path, record: &Record, result: Report) -> Result<()> {
    match result {
        Report::Applied | Report::RefusedEnd => client_journal::remove_if(config, record)?,
        Report::Pending => {}
        Report::Stalled(reason) if !record.payload["disposition"].is_string() => {
            let mut payload = record.payload.clone();
            payload["disposition"] = json!(reason);
            client_journal::update(config, &record.id, Phase::Outcome, payload)?;
        }
        Report::Stalled(_) => {}
    }
    Ok(())
}

fn adopt_host(
    context: &mut SessionContext,
    host: Option<HostIdentity>,
    observer: Option<&HostIdentity>,
) -> bool {
    // A later event from another process must not attach its exit proof to this start.
    if context.host.is_some()
        || observer.is_none()
        || context.registration_host.as_ref() != observer
    {
        return false;
    }
    context.host = host;
    context.host.is_some()
}

/// Only genuine hook events establish or advance a local native session record.
pub(crate) async fn prepare(
    connection: &Connection,
    agent: &str,
    session: &str,
    fresh_start: bool,
    event: &str,
) -> Result<()> {
    let config = crate::hub_cli::config_dir()?;
    let previous = client_session::context(&config, session, &connection.hub, &connection.repo)?;
    let previous_nonce = previous.as_ref().map(|context| context.start_nonce.clone());
    let (credential_id, binding_id) = binding(connection).ok_or_else(invalid)?;
    let mut context = match previous {
        Some(context) if matches(connection, &context) => context,
        _ if !fresh_start => return Ok(()),
        _ => SessionContext {
            session: session.into(),
            start_nonce: String::new(),
            incarnation: None,
            agent: agent.into(),
            hub: connection.hub.clone(),
            repo: connection.repo.clone(),
            root: connection.root.clone(),
            protocol: 0,
            protocol_checked_at: 0,
            terminal: false,
            registered: false,
            qualification: None,
            exit_ineligible: false,
            revoked: false,
            exit_observed_at: None,
            explicit_end_requested: false,
            observed_at: unix_now(),
            host: None,
            registration_host: None,
            credential_id,
            binding_id,
        },
    };
    if context.revoked {
        return Ok(());
    }
    let registration_host = client_session::registration_host();
    if fresh_start {
        context.start_nonce = client_session::nonce()?;
        context.observed_at = unix_now();
        context.terminal = false;
        context.registered = false;
        context.qualification = None;
        context.exit_ineligible = false;
        context.exit_observed_at = None;
        context.explicit_end_requested = false;
        context.host = None;
        context.registration_host = registration_host.clone();
        client_session::begin_context(&config, previous_nonce.as_deref(), &context)?;
    } else if context.terminal || context.explicit_end_requested {
        return Ok(());
    }
    let event_id = format!("{event}-{}", client_session::nonce()?);
    let host = client_session::observe_event(&config, agent, session, &event_id, fresh_start)?;
    let newly_qualified = adopt_host(&mut context, host, registration_host.as_ref());
    let negotiation = negotiate(connection, &mut context).await;
    client_session::save_context(&config, &context)?;
    negotiation?;
    if context.protocol != 1 {
        return Ok(());
    }
    if fresh_start && context.incarnation.is_none() {
        let (status, state) = request(
            connection,
            "/v1/hub/cooperation/state",
            Some(&json!({"session":session})),
        )
        .await?;
        response_state(&mut context, status, &state, unix_now());
        if status == 200 {
            context.incarnation = state["lifecycle"]["incarnation"]
                .as_str()
                .map(str::to_owned);
        }
        client_session::save_context(&config, &context)?;
        if status != 200 {
            return Err(invalid());
        }
    }
    if fresh_start || !context.registered || context.incarnation.is_none() || newly_qualified {
        let request = LifecycleRegister {
            session: session.into(),
            start_nonce: context.start_nonce.clone(),
            operation_id: format!("{}-{}", context.start_nonce, context.host.is_some()),
            observed_at: context.observed_at,
            expected_incarnation: context.incarnation.clone(),
            qualified: context.host.is_some(),
        };
        let record = operation(
            &config,
            Record {
                id: request.operation_id.clone(),
                kind: Kind::Registration,
                phase: Phase::Prepared,
                payload: payload(
                    &context,
                    "/v1/hub/cooperation/register",
                    serde_json::to_value(request)?,
                ),
            },
        )?;
        let result = report(connection, &mut context, &record).await;
        client_session::save_context(&config, &context)?;
        settle(&config, &record, result?)?;
    }
    Ok(())
}

fn observe_exit(context: &mut SessionContext, at: i64) -> bool {
    if context.exit_observed_at.is_none() {
        context.exit_observed_at = Some(at);
        true
    } else {
        false
    }
}

fn settle_terminal_reports(config: &Path, context: &SessionContext) -> Result<()> {
    for record in client_journal::inspect(config)?.records {
        if record.kind == Kind::TerminalEnd
            && record.payload["hub"] == context.hub
            && record.payload["repo"] == context.repo
            && record.payload["root"] == json!(context.root)
            && record.payload["session"] == context.session
            && record.payload["start_nonce"] == context.start_nonce
            && record.payload["incarnation"].as_str() == context.incarnation.as_deref()
        {
            client_journal::remove_if(config, &record)?;
        }
    }
    Ok(())
}

async fn terminal(
    connection: &Connection,
    context: &mut SessionContext,
    reason: EndReason,
) -> Result<()> {
    if context.revoked {
        return Err(invalid());
    }
    let config = crate::hub_cli::config_dir()?;
    observe_exit(context, unix_now());
    client_session::save_context(&config, context)?;
    let (status, state) = request(
        connection,
        "/v1/hub/cooperation/state",
        Some(&json!({"session":context.session})),
    )
    .await?;
    response_state(context, status, &state, unix_now());
    client_session::save_context(&config, context)?;
    if status != 200 {
        return Err(invalid());
    }
    let view: LifecycleView = reply::decode(&state["lifecycle"])?;
    if view.session != context.session {
        return Err(invalid());
    }
    if view.state == LifecycleState::Terminal || view.start_nonce != context.start_nonce {
        context.terminal = true;
        client_session::save_context(&config, context)?;
        settle_terminal_reports(&config, context)?;
        return Ok(());
    }
    if context.registered && context.incarnation.as_deref() != Some(view.incarnation.as_str()) {
        return Err(invalid());
    }
    context.incarnation = Some(view.incarnation.clone());
    context.registered = true;
    context.qualification = Some(view.qualification);
    if reason == EndReason::ObservedExit && view.qualification != Qualification::Qualified {
        context.exit_ineligible = true;
        client_session::save_context(&config, context)?;
        return Ok(());
    }
    let request = terminal_request(context, &state, reason)?;
    context.incarnation = Some(request.incarnation.clone());
    context.registered = true;
    let record = operation(
        &config,
        Record {
            id: request.operation_id.clone(),
            kind: Kind::TerminalEnd,
            phase: Phase::Prepared,
            payload: payload(
                context,
                "/v1/hub/cooperation/end",
                serde_json::to_value(request)?,
            ),
        },
    )?;
    let result = report(connection, context, &record).await;
    client_session::save_context(&config, context)?;
    settle(&config, &record, result?)?;
    Ok(())
}

fn terminal_request(
    context: &SessionContext,
    state: &Value,
    reason: EndReason,
) -> Result<LifecycleEnd> {
    let view: LifecycleView = reply::decode(&state["lifecycle"])?;
    if view.session != context.session
        || view.start_nonce != context.start_nonce
        || view.state != LifecycleState::Live
        || (reason == EndReason::ObservedExit
            && view.qualification != agentscm::hub::session_lifecycle::Qualification::Qualified)
    {
        return Err(invalid());
    }
    if context.registered
        && context
            .incarnation
            .as_ref()
            .is_some_and(|id| id != &view.incarnation)
    {
        return Err(invalid());
    }
    let work: Vec<WorkFence> = reply::decode(&state["work"])?;
    Ok(LifecycleEnd {
        session: context.session.clone(),
        incarnation: view.incarnation.clone(),
        operation_id: format!("end-{}", view.incarnation),
        observed_at: context.exit_observed_at.ok_or_else(invalid)?,
        reason,
        work,
    })
}

pub(crate) async fn finish_hook(connection: &Connection, expected: &SessionContext) -> Result<()> {
    if let Some(mut context) = transport_context(connection, &expected.session).filter(|context| {
        context.explicit_end_requested
            && context.start_nonce == expected.start_nonce
            && context.incarnation == expected.incarnation
    }) {
        terminal(connection, &mut context, EndReason::ExplicitEnd).await?;
    }
    Ok(())
}

async fn refresh(connection: &Connection, context: &mut SessionContext) -> Result<()> {
    let mut after = Value::Null;
    for _ in 0..8 {
        let body = json!({"session":context.session,"incarnation":context.incarnation,"after":after,"limit":100});
        let (status, page) =
            request(connection, "/v1/hub/cooperation/refresh", Some(&body)).await?;
        response_state(context, status, &page, unix_now());
        if context.revoked {
            return Err(invalid());
        }
        if unsupported(status, &page) {
            return Ok(());
        }
        if status != 200 {
            return Err(invalid());
        }
        if page["complete"] == true {
            return Ok(());
        }
        after = page["next_after"].clone();
        if !after.is_string() {
            return Err(invalid());
        }
    }
    Err(Error::Invalid(
        "dependency refresh coverage is incomplete".into(),
    ))
}

async fn cycle(config: &Path) -> Result<()> {
    let connections = crate::client_config::load()?;
    let pruned = client_session::prune_contexts(config).is_ok();
    let scan = client_session::contexts(config)?;
    if !pruned || !scan.complete() {
        eprintln!(
            "AgentSCM: session coverage is incomplete ({} unreadable, {} omitted); valid pending reports remain eligible.",
            scan.stalled, scan.truncated
        );
    }
    let mut contexts = scan.contexts;
    let records = client_journal::scan(config)?.records;
    // Pending evidence receives an exact route read even when the live-context page is full.
    for record in &records {
        let (Some(session), Some(hub), Some(repo)) = (
            record.payload["session"].as_str(),
            record.payload["hub"].as_str(),
            record.payload["repo"].as_str(),
        ) else {
            continue;
        };
        if !contexts
            .iter()
            .any(|context| context.session == session && context.hub == hub && context.repo == repo)
            && let Ok(Some(context)) = client_session::context(config, session, hub, repo)
        {
            contexts.push(context);
        }
    }
    // Local observation precedes remote negotiation so an outage cannot move the exit time.
    for context in &mut contexts {
        if !context.terminal
            && !context.revoked
            && context
                .host
                .as_ref()
                .is_some_and(|host| client_session::observe_host(host) == HostObservation::Exited)
            && observe_exit(context, unix_now())
        {
            client_session::save_context(config, context)?;
        }
    }
    for record in records {
        if record.payload["disposition"].is_string() {
            continue;
        }
        let Some(context) = contexts.iter_mut().find(|context| {
            record.payload["hub"] == context.hub
                && record.payload["repo"] == context.repo
                && record.payload["root"] == json!(context.root)
                && record.payload["session"] == context.session
        }) else {
            continue;
        };
        if context.revoked {
            settle(config, &record, Report::Stalled("revoked"))?;
            continue;
        }
        let Some(connection) = connections
            .iter()
            .find(|connection| matches(connection, context))
        else {
            continue;
        };
        if end_intent(&record) {
            if record.payload["start_nonce"] != context.start_nonce
                || record.payload["incarnation"].as_str() != context.incarnation.as_deref()
            {
                settle(config, &record, Report::Stalled("superseded"))?;
            } else {
                let negotiation = negotiate(connection, context).await;
                client_session::save_context(config, context)?;
                if negotiation.is_ok() && context.protocol == 1 {
                    let _ = terminal(connection, context, EndReason::ExplicitEnd).await;
                }
            }
            continue;
        }
        if record.kind == Kind::Registration
            && !context
                .registration_host
                .as_ref()
                .is_some_and(|host| client_session::observe_host(host) == HostObservation::Same)
        {
            continue;
        }
        let negotiation = negotiate(connection, context).await;
        client_session::save_context(config, context)?;
        if context.revoked {
            settle(config, &record, Report::Stalled("revoked"))?;
            continue;
        }
        if negotiation.is_err() || context.protocol != 1 {
            continue;
        }
        let result = report(connection, context, &record).await;
        client_session::save_context(config, context)?;
        if let Ok(result) = result {
            settle(config, &record, result)?;
        }
    }
    for context in &mut contexts {
        if context.terminal
            || context.revoked
            || (context.exit_ineligible && !context.explicit_end_requested)
        {
            continue;
        }
        let Some(connection) = connections
            .iter()
            .find(|connection| matches(connection, context))
        else {
            continue;
        };
        let negotiation = negotiate(connection, context).await;
        client_session::save_context(config, context)?;
        if negotiation.is_err() || context.protocol != 1 {
            continue;
        }
        if context.exit_observed_at.is_some() {
            let reason = if context.explicit_end_requested {
                EndReason::ExplicitEnd
            } else {
                EndReason::ObservedExit
            };
            let _ = terminal(connection, context, reason).await;
        } else if context.registered && context.incarnation.is_some() {
            let _ = refresh(connection, context).await;
        }
        client_session::save_context(config, context)?;
    }
    Ok(())
}

pub(crate) async fn run() -> Result<()> {
    let Some(guard) = crate::client_service::begin_run()? else {
        return Ok(());
    };
    let config = crate::hub_cli::config_dir()?;
    while guard.is_current()? {
        if cycle(&config).await.is_err() {
            eprintln!("AgentSCM: local cooperation coverage is unknown; pending reports retained.");
        }
        tokio::select! {_=tokio::time::sleep(Duration::from_secs(30))=>{},_=tokio::signal::ctrl_c()=>break,}
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use agentscm::hub::session_lifecycle::Qualification;
    fn context() -> SessionContext {
        SessionContext {
            session: "native".into(),
            start_nonce: "nonce".into(),
            incarnation: Some("inc".into()),
            agent: "codex".into(),
            hub: "https://hub.example".into(),
            repo: "acme/widgets".into(),
            root: "/synthetic/repo".into(),
            protocol: 1,
            protocol_checked_at: 10,
            terminal: false,
            registered: true,
            qualification: None,
            exit_ineligible: false,
            revoked: false,
            exit_observed_at: None,
            explicit_end_requested: false,
            observed_at: 10,
            host: None,
            registration_host: None,
            credential_id: "token-id".into(),
            binding_id: "binding".into(),
        }
    }
    #[test]
    fn journal_retry_keeps_original_observation_and_exact_body() {
        let config = tempfile::tempdir().unwrap();
        let context = context();
        let first = Record {
            id: "operation".into(),
            kind: Kind::TerminalEnd,
            phase: Phase::Prepared,
            payload: payload(&context, "/end", json!({"observed_at":10,"work":[1]})),
        };
        operation(config.path(), first.clone()).unwrap();
        let mut retry = first.clone();
        retry.payload["body"]["observed_at"] = json!(20);
        retry.payload["body"]["work"] = json!([2]);
        assert_eq!(operation(config.path(), retry).unwrap(), first);
    }
    #[test]
    fn initial_exit_observation_survives_outage_retry() {
        let mut context = context();
        assert!(observe_exit(&mut context, 20));
        assert!(!observe_exit(&mut context, 100));
        assert_eq!(context.exit_observed_at, Some(20));
    }

    #[test]
    fn host_adoption_belongs_only_to_the_process_that_started_the_context() {
        let first = HostIdentity {
            pid: 1,
            started: "first".into(),
        };
        let second = HostIdentity {
            pid: 2,
            started: "second".into(),
        };
        let native = HostIdentity {
            pid: 3,
            started: "native-ancestor".into(),
        };
        let mut context = context();
        assert!(!adopt_host(&mut context, Some(first.clone()), Some(&first)));
        assert!(context.registration_host.is_none());
        context.registration_host = Some(second.clone());
        assert!(!adopt_host(&mut context, Some(first.clone()), Some(&first)));
        assert!(!adopt_host(&mut context, Some(first.clone()), None));
        assert!(!adopt_host(&mut context, None, Some(&second)));
        assert!(context.host.is_none());
        // The nearest registration ancestor can differ from the qualified native host.
        assert!(adopt_host(
            &mut context,
            Some(native.clone()),
            Some(&second)
        ));
        assert!(!adopt_host(&mut context, None, Some(&second)));
        assert!(!adopt_host(&mut context, Some(first.clone()), Some(&first)));
        assert_eq!(context.host, Some(native));
        assert_eq!(context.registration_host, Some(second));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn foreign_host_exit_cannot_end_the_live_successor() {
        crate::client_config::in_isolated_config_home(|config| {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            for args in [
                vec!["init", "--quiet"],
                vec![
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/acme/widgets.git",
                ],
            ] {
                assert!(
                    std::process::Command::new("git")
                        .current_dir(&root)
                        .args(args)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let mut context = context();
                    context.start_nonce = "successor".into();
                    context.hub = format!("http://{}", listener.local_addr().unwrap());
                    context.root = root;
                    context.protocol_checked_at = unix_now();
                    let second = client_session::registration_host().unwrap();
                    let mut first = second.clone();
                    first.pid = i32::MAX as u32;
                    context.registration_host = Some(second.clone());
                    assert!(!adopt_host(&mut context, Some(first.clone()), Some(&first)));
                    assert!(context.host.is_none());
                    assert_eq!(
                        client_session::observe_host(&first),
                        HostObservation::Exited
                    );
                    context.binding_id = onboarding_client(&context.root, "synthetic-token");
                    context.credential_id = context.binding_id.clone();
                    let token_file = config.join("synthetic-token");
                    crate::hub_cli::write_private(&token_file, "synthetic-token").unwrap();
                    let connection = Connection {
                        root: context.root.clone(),
                        origin_host: "github.com".into(),
                        hub: context.hub.clone(),
                        repo: context.repo.clone(),
                        token_file,
                        report_completion: false,
                        owned_token_id: None,
                        owned_token_expires_at: None,
                        agents: vec![],
                    };
                    client_session::save_context(config, &context).unwrap();
                    crate::hub_cli::write_private(
                        &config.join("agentscm/connections.json"),
                        &json!({"version":1,"connections":[connection]}).to_string(),
                    )
                    .unwrap();
                    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                    let seen = requests.clone();
                    let state = json!({"complete":true,"lifecycle": {
                    "session":context.session,"start_nonce":context.start_nonce,
                    "incarnation":"inc","state":"live","qualification":"qualified","observed_at":10
                },"work":[]});
                    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
                        seen.lock().unwrap().push(uri.path().to_owned());
                        std::future::ready(axum::Json(state.clone()))
                    });
                    let server = tokio::spawn(async move {
                        axum::serve(listener, app).await.unwrap();
                    });
                    cycle(config).await.unwrap();
                    cycle(config).await.unwrap();
                    server.abort();
                    let _ = server.await;
                    assert_eq!(
                        *requests.lock().unwrap(),
                        ["/v1/hub/cooperation/refresh"; 2]
                    );
                    let current = client_session::context(
                        config,
                        &context.session,
                        &context.hub,
                        &context.repo,
                    )
                    .unwrap()
                    .unwrap();
                    assert!(current.host.is_none());
                    assert_eq!(current.registration_host, Some(second));
                    assert!(current.exit_observed_at.is_none());
                    assert!(!current.terminal);
                    assert!(client_journal::inspect(config).unwrap().records.is_empty());
                });
        });
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn retained_host_still_observes_exit_after_a_stale_save() {
        crate::client_config::in_isolated_config_home(|config| {
            let pending = context();
            client_session::save_context(config, &pending).unwrap();
            let mut qualified = pending.clone();
            let mut host = client_session::registration_host().unwrap();
            host.pid = i32::MAX as u32;
            assert_eq!(client_session::observe_host(&host), HostObservation::Exited);
            qualified.host = Some(host);
            qualified.qualification = Some(Qualification::Qualified);
            client_session::save_context(config, &qualified).unwrap();
            client_session::save_context(config, &pending).unwrap();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(cycle(config))
                .unwrap();
            let current =
                client_session::context(config, &pending.session, &pending.hub, &pending.repo)
                    .unwrap()
                    .unwrap();
            assert_eq!(current.host, qualified.host);
            assert!(current.exit_observed_at.is_some());
            assert!(!current.terminal, "local observation is not a Hub end");
        });
    }

    #[test]
    fn ineligible_exited_sessions_stop_polling_without_ending_hub_work() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for qualification in [Qualification::Unknown, Qualification::Mixed] {
            crate::client_config::in_isolated_config_home(|config| {
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path().canonicalize().unwrap();
                for args in [
                    vec!["init", "--quiet"],
                    vec![
                        "remote",
                        "add",
                        "origin",
                        "https://github.com/acme/widgets.git",
                    ],
                ] {
                    assert!(
                        std::process::Command::new("git")
                            .current_dir(&root)
                            .args(args)
                            .status()
                            .unwrap()
                            .success()
                    );
                }
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let mut context = context();
                    context.hub = format!("http://{}", listener.local_addr().unwrap());
                    context.root = root.clone();
                    context.exit_observed_at = Some(20);
                    context.binding_id = onboarding_client(&root, "synthetic-token");
                    context.credential_id = context.binding_id.clone();
                    let token_file = config.join("synthetic-token");
                    crate::hub_cli::write_private(&token_file, "synthetic-token").unwrap();
                    let connection = Connection {
                        root, origin_host: "github.com".into(), hub: context.hub.clone(),
                        repo: context.repo.clone(), token_file, report_completion: false,
                        owned_token_id: None, owned_token_expires_at: None, agents: vec![],
                    };
                    client_session::save_context(config, &context).unwrap();
                    crate::hub_cli::write_private(&config.join("agentscm/connections.json"),
                        &json!({"version":1,"connections":[connection.clone()]}).to_string()).unwrap();
                    let state = json!({"lifecycle": {
                        "session":context.session,"start_nonce":context.start_nonce,
                        "incarnation":"inc","state":"live","qualification":qualification,"observed_at":10
                    },"work":[{"work_id":"wrk_synthetic","generation":3,"updated_seq":7}]}).to_string();
                    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                    let seen = requests.clone();
                    let server = tokio::spawn(async move {
                        loop {
                            let (mut stream, _) = listener.accept().await.unwrap();
                            let mut request = Vec::new();
                            loop {
                                let mut bytes = [0; 4096];
                                let count = stream.read(&mut bytes).await.unwrap();
                                assert!(count != 0 && request.len() < 16 * 1024);
                                request.extend_from_slice(&bytes[..count]);
                                if let Some(head) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                                    let text = String::from_utf8_lossy(&request[..head]);
                                    let length = text.lines().find_map(|line| {
                                        let (name, value) = line.split_once(':')?;
                                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                                    }).unwrap_or(0);
                                    if request.len() >= head + 4 + length { break; }
                                }
                            }
                            seen.lock().unwrap().push(String::from_utf8_lossy(&request).lines().next().unwrap().to_owned());
                            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", state.len(), state);
                            stream.write_all(response.as_bytes()).await.unwrap();
                        }
                    });
                    terminal(&connection, &mut context, EndReason::ObservedExit).await.unwrap();
                    cycle(config).await.unwrap();
                    cycle(config).await.unwrap();
                    server.abort();
                    let _ = server.await;
                    assert_eq!(*requests.lock().unwrap(), ["POST /v1/hub/cooperation/state HTTP/1.1"]);
                    let current = client_session::context(config, &context.session, &context.hub, &context.repo).unwrap().unwrap();
                    assert!(current.exit_ineligible);
                    assert!(!current.terminal);
                    assert_eq!(current.qualification, Some(qualification));
                    assert!(client_journal::inspect(config).unwrap().records.is_empty());
                });
            });
        }
    }

    #[test]
    fn confirmed_terminal_settles_original_end_intent_without_erasing_successor_evidence() {
        let config = tempfile::tempdir().unwrap();
        let context = context();
        client_session::save_context(config.path(), &context).unwrap();
        let fence = client_session::cleanup_fence(config.path(), &context.session).unwrap();
        remember_end(config.path(), "codex", &fence, 20).unwrap();
        let mut successor = client_journal::inspect(config.path()).unwrap().records[0].clone();
        successor.id = "end-successor".into();
        successor.payload["start_nonce"] = json!("successor");
        client_journal::put(config.path(), &successor).unwrap();
        settle_terminal_reports(config.path(), &context).unwrap();
        assert_eq!(
            client_journal::inspect(config.path()).unwrap().records,
            [successor]
        );
    }

    #[test]
    fn idle_delivery_uses_current_binding_and_attempt_identity_without_capability_age() {
        crate::client_config::in_isolated_config_home(|config| {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            for args in [
                vec!["init", "--quiet"],
                vec![
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/acme/widgets.git",
                ],
            ] {
                assert!(
                    std::process::Command::new("git")
                        .current_dir(&root)
                        .args(args)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            let token_file = config.join("synthetic-token");
            crate::hub_cli::write_private(&token_file, "synthetic-token").unwrap();
            let connection = Connection {
                root,
                origin_host: "github.com".into(),
                hub: "https://hub.example".into(),
                repo: "acme/widgets".into(),
                token_file,
                report_completion: false,
                owned_token_id: None,
                owned_token_expires_at: None,
                agents: vec![],
            };
            let mut context = context();
            context.root = connection.root.clone();
            context.binding_id = onboarding_client(&connection.root, "synthetic-token");
            context.credential_id = context.binding_id.clone();
            client_session::save_context(config, &context).unwrap();
            crate::hub_cli::write_private(
                &config.join("agentscm/connections.json"),
                &json!({"version":1,"connections":[connection.clone()]}).to_string(),
            )
            .unwrap();
            assert!(transport_context(&connection, "native").is_none());
            assert!(delivery_context(&connection, "native", "inc").is_some());
            assert!(delivery_context(&connection, "native", "successor").is_none());
            context.terminal = true;
            client_session::save_context(config, &context).unwrap();
            assert!(
                delivery_context(&connection, "native", "inc")
                    .unwrap()
                    .terminal
            );
            context.revoked = true;
            client_session::save_context(config, &context).unwrap();
            assert!(delivery_context(&connection, "native", "inc").is_none());
        });
    }

    #[test]
    fn newer_hub_fields_are_lenient_but_unknown_lifecycle_values_are_not_authority() {
        let mut context = context();
        let mut lifecycle = json!({
            "session": "native", "incarnation": "inc", "start_nonce": "nonce",
            "state": "live", "qualification": "qualified", "observed_at": 10,
            "newer_hub_field": {"detail": true}
        });
        assert_eq!(
            apply_reply(&mut context, Kind::Registration, 200, lifecycle.clone(), 20).unwrap(),
            Report::Applied
        );
        context.exit_observed_at = Some(20);
        let state = json!({
            "lifecycle": lifecycle,
            "work": [{"work_id": "wrk_synthetic", "generation": 3, "updated_seq": 7,
                "newer_hub_field": {"detail": true}}]
        });
        let request = terminal_request(&context, &state, EndReason::ExplicitEnd).unwrap();
        assert_eq!(request.work[0].updated_seq, 7);
        lifecycle["state"] = json!("newer_lifecycle_state");
        assert!(apply_reply(&mut context, Kind::Registration, 200, lifecycle.clone(), 20).is_err());
        let mut unknown = state;
        unknown["lifecycle"] = lifecycle;
        assert!(terminal_request(&context, &unknown, EndReason::ExplicitEnd).is_err());
    }

    fn host() -> client_session::Process {
        client_session::Process {
            pid: 123,
            started: "synthetic-start".into(),
            name: "/synthetic/codex".into(),
        }
    }

    fn start_successor(config: &Path, previous: &SessionContext) -> SessionContext {
        let mut successor = previous.clone();
        successor.start_nonce = "nonce-b".into();
        successor.incarnation = Some("inc-b".into());
        successor.terminal = false;
        successor.explicit_end_requested = false;
        successor.exit_observed_at = None;
        client_session::begin_context(config, Some(&previous.start_nonce), &successor).unwrap();
        client_session::record_under(&client_session::directory(config), &host(), "native")
            .unwrap();
        successor
    }

    #[test]
    fn successful_old_end_response_then_fresh_start_keeps_successor_association() {
        let config = tempfile::tempdir().unwrap();
        let mut old = context();
        client_session::save_context(config.path(), &old).unwrap();
        client_session::record_under(&client_session::directory(config.path()), &host(), "native")
            .unwrap();
        let fence = client_session::cleanup_fence(config.path(), "native").unwrap();
        remember_end(config.path(), "codex", &fence, 20).unwrap();
        assert_eq!(
            apply_reply(
                &mut old,
                Kind::TerminalEnd,
                200,
                json!({"lifecycle":{
                    "session":"native","incarnation":"inc","start_nonce":"nonce",
                    "state":"terminal","qualification":"qualified","observed_at":20
                }}),
                21
            )
            .unwrap(),
            Report::Applied
        );
        // The old request succeeded. Its caller has not yet run its trailing local cleanup.
        let successor = start_successor(config.path(), &old);
        client_session::forget(config.path(), &fence).unwrap();
        assert_eq!(
            client_session::context(config.path(), "native", &old.hub, &old.repo).unwrap(),
            Some(successor)
        );
        assert_eq!(
            client_session::recorded(config.path(), &[host()]),
            client_session::Recorded::One("native".into())
        );
    }

    #[test]
    fn start_during_early_await_cannot_be_ended_or_forgotten_by_old_event() {
        let config = tempfile::tempdir().unwrap();
        let old = context();
        client_session::save_context(config.path(), &old).unwrap();
        client_session::record_under(&client_session::directory(config.path()), &host(), "native")
            .unwrap();
        let fence = client_session::cleanup_fence(config.path(), "native").unwrap();
        // Hook-agent identification awaits after capturing A and before remembering its end.
        let successor = start_successor(config.path(), &old);
        remember_end(config.path(), "codex", &fence, 20).unwrap();
        client_session::forget(config.path(), &fence).unwrap();
        assert_eq!(
            client_session::context(config.path(), "native", &old.hub, &old.repo).unwrap(),
            Some(successor)
        );
        assert_eq!(
            client_session::recorded(config.path(), &[host()]),
            client_session::Recorded::One("native".into())
        );
        let journal = client_journal::inspect(config.path()).unwrap();
        assert_eq!(journal.records.len(), 1);
        let evidence = &journal.records[0];
        assert_eq!(evidence.phase, Phase::Outcome);
        assert_eq!(evidence.payload["disposition"], "superseded");
        assert_eq!(evidence.payload["start_nonce"], "nonce");
        assert_eq!(evidence.payload["incarnation"], "inc");
        assert_eq!(evidence.payload["observed_at"], 20);
    }

    #[test]
    fn end_metadata_uses_entry_identity_and_requires_its_original_binding() {
        use std::io::Write;
        let mut token = tempfile::NamedTempFile::new().unwrap();
        token.write_all(b"synthetic-token").unwrap();
        let mut original = context();
        original.binding_id = onboarding_client(&original.root, "synthetic-token");
        let connection = Connection {
            root: original.root.clone(),
            origin_host: "example.test".into(),
            hub: original.hub.clone(),
            repo: original.repo.clone(),
            token_file: token.path().into(),
            report_completion: false,
            owned_token_id: Some(original.credential_id.clone()),
            owned_token_expires_at: None,
            agents: Vec::new(),
        };
        let config = tempfile::tempdir().unwrap();
        client_session::save_context(config.path(), &original).unwrap();
        let fence = client_session::cleanup_fence(config.path(), "native").unwrap();
        let successor = start_successor(config.path(), &original);
        assert_ne!(successor.incarnation, original.incarnation);
        assert_eq!(
            end_transport_context(&connection, fence.end_contexts(), true).unwrap(),
            Some(original.clone())
        );
        assert!(end_transport_context(&connection, &[], false).is_err());
        assert_eq!(end_transport_context(&connection, &[], true).unwrap(), None);
        let mut unknown = original.clone();
        unknown.registered = false;
        assert!(end_transport_context(&connection, &[unknown.clone()], true).is_err());
        unknown.protocol = 0;
        assert_eq!(
            end_transport_context(&connection, &[unknown], true).unwrap(),
            None
        );
        std::fs::write(token.path(), "replacement-synthetic-token").unwrap();
        assert_eq!(
            end_transport_context(&connection, &[original], true).unwrap(),
            None
        );
    }

    #[test]
    fn republished_association_fences_end_before_successor_nonce_is_saved() {
        let config = tempfile::tempdir().unwrap();
        let old = context();
        client_session::save_context(config.path(), &old).unwrap();
        client_session::record_under(&client_session::directory(config.path()), &host(), "native")
            .unwrap();
        let fence = client_session::cleanup_fence(config.path(), "native").unwrap();
        // The start hook's host recording can publish before cooperation saves its fresh nonce.
        client_session::record_under(&client_session::directory(config.path()), &host(), "native")
            .unwrap();
        remember_end(config.path(), "codex", &fence, 20).unwrap();
        client_session::forget(config.path(), &fence).unwrap();
        assert_eq!(
            client_session::context(config.path(), "native", &old.hub, &old.repo).unwrap(),
            Some(old)
        );
        assert_eq!(
            client_session::recorded(config.path(), &[host()]),
            client_session::Recorded::One("native".into())
        );
    }

    #[test]
    fn end_retains_same_start_registration_completed_during_identification() {
        let config = tempfile::tempdir().unwrap();
        let mut pending = context();
        pending.protocol = 0;
        pending.registered = false;
        pending.incarnation = None;
        client_session::save_context(config.path(), &pending).unwrap();
        let fence = client_session::cleanup_fence(config.path(), "native").unwrap();
        let registered = context();
        client_session::save_context(config.path(), &registered).unwrap();
        remember_end(config.path(), "codex", &fence, 20).unwrap();
        let saved =
            client_session::context(config.path(), "native", &registered.hub, &registered.repo)
                .unwrap()
                .unwrap();
        assert!(saved.registered && saved.explicit_end_requested);
        assert_eq!(saved.protocol, 1);
        assert_eq!(saved.incarnation, registered.incarnation);
        assert_eq!(saved.exit_observed_at, Some(20));
        let journal = client_journal::inspect(config.path()).unwrap();
        assert_eq!(journal.records.len(), 1);
        assert_eq!(journal.records[0].id, "end-inc");
        assert_eq!(journal.records[0].phase, Phase::Prepared);
    }

    #[test]
    fn end_deadline_retains_intent_then_admits_one_fenced_resident_replay() {
        let config = tempfile::tempdir().unwrap();
        let context = context();
        client_session::save_context(config.path(), &context).unwrap();
        let fence = client_session::cleanup_fence(config.path(), "native").unwrap();
        remember_end(config.path(), "codex", &fence, 20).unwrap();
        client_session::forget(config.path(), &fence).unwrap();
        remember_end(config.path(), "codex", &fence, 100).unwrap();
        let mut retained =
            client_session::context(config.path(), "native", &context.hub, &context.repo)
                .unwrap()
                .unwrap();
        assert_eq!(retained.exit_observed_at, Some(20));
        assert!(retained.explicit_end_requested);
        let state = json!({"lifecycle":LifecycleView {session:"native".into(),incarnation:"inc".into(),start_nonce:"nonce".into(),state:LifecycleState::Live,qualification:Qualification::Qualified,observed_at:10},"work":[{"work_id":"wrk_synthetic","generation":3,"updated_seq":7}]});
        let request = terminal_request(&retained, &state, EndReason::ExplicitEnd).unwrap();
        let candidate = Record {
            id: request.operation_id.clone(),
            kind: Kind::TerminalEnd,
            phase: Phase::Prepared,
            payload: payload(
                &retained,
                "/v1/hub/cooperation/end",
                serde_json::to_value(request).unwrap(),
            ),
        };
        let admitted = operation(config.path(), candidate.clone()).unwrap();
        assert_eq!(admitted.phase, Phase::Admitted);
        assert_eq!(admitted.payload["body"]["observed_at"], 20);
        assert_eq!(admitted.payload["body"]["work"][0]["updated_seq"], 7);
        // Another deadline after admission must not replace the original Work fence on replay.
        let mut changed = candidate;
        changed.payload["body"]["work"][0]["updated_seq"] = json!(8);
        assert_eq!(operation(config.path(), changed).unwrap(), admitted);
        let mut successor = state;
        successor["lifecycle"]["incarnation"] = json!("new-inc");
        assert!(terminal_request(&retained, &successor, EndReason::ExplicitEnd).is_err());
        retained.terminal = true;
        client_session::save_context(config.path(), &retained).unwrap();
        settle(config.path(), &admitted, Report::Applied).unwrap();
        client_session::forget(config.path(), &fence).unwrap();
        let tombstone =
            client_session::context(config.path(), "native", &context.hub, &context.repo)
                .unwrap()
                .unwrap();
        assert!(tombstone.terminal);
        assert_eq!(tombstone.observed_at, 10);
        assert_eq!(tombstone.exit_observed_at, Some(20));
        assert!(
            client_journal::inspect(config.path())
                .unwrap()
                .records
                .is_empty()
        );
    }
    #[test]
    fn revoked_and_rollback_are_distinct_from_success() {
        let mut context = context();
        assert_eq!(
            apply_reply(&mut context, Kind::TerminalEnd, 401, json!({}), 20).unwrap(),
            Report::Stalled("revoked")
        );
        assert!(context.revoked);
        let mut context = super::tests::context();
        assert_eq!(
            apply_reply(&mut context, Kind::TerminalEnd, 404, json!({}), 20).unwrap(),
            Report::Pending
        );
        assert_eq!(context.protocol, 0);
        assert!(!context.terminal);
        assert!(!context.revoked);
    }
    #[test]
    fn registration_response_is_receipt_not_activity_or_terminal_permission() {
        let mut context = context();
        let at = context.observed_at;
        let view = LifecycleView {
            session: "native".into(),
            incarnation: "new".into(),
            start_nonce: "nonce".into(),
            state: LifecycleState::Live,
            qualification: Qualification::Unknown,
            observed_at: 10,
        };
        assert_eq!(
            apply_reply(
                &mut context,
                Kind::Registration,
                200,
                serde_json::to_value(view).unwrap(),
                20
            )
            .unwrap(),
            Report::Applied
        );
        assert_eq!(context.observed_at, at);
        assert!(context.host.is_none());
        assert_eq!(context.incarnation.as_deref(), Some("new"));
    }
    #[test]
    fn definitely_refused_end_can_readmit_fresh_fences_without_losing_observation() {
        let config = tempfile::tempdir().unwrap();
        let mut context = context();
        let record = operation(
            config.path(),
            Record {
                id: "operation".into(),
                kind: Kind::TerminalEnd,
                phase: Phase::Prepared,
                payload: payload(&context, "/end", json!({"observed_at":10})),
            },
        )
        .unwrap();
        let result = apply_reply(
            &mut context,
            Kind::TerminalEnd,
            400,
            json!({"code":"invalid_or_changed"}),
            20,
        )
        .unwrap();
        assert_eq!(result, Report::RefusedEnd);
        assert!(!context.terminal);
        settle(config.path(), &record, result).unwrap();
        let mut next = record.clone();
        next.payload["body"]["work"] = json!([{"updated_seq":2}]);
        assert_eq!(operation(config.path(), next.clone()).unwrap(), next);
        // A concurrent late refusal cannot erase the newly admitted request.
        settle(config.path(), &record, Report::RefusedEnd).unwrap();
        let saved = client_journal::inspect(config.path())
            .unwrap()
            .records
            .remove(0);
        assert_eq!(saved, next);
        assert_eq!(saved.payload["body"]["observed_at"], 10);
        assert_eq!(
            apply_reply(&mut context, Kind::TerminalEnd, 503, json!({}), 30).unwrap(),
            Report::Pending
        );
        assert!(!context.terminal);
    }
}
