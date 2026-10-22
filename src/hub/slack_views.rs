//! Bounded, repository-scoped Slack App Home views.

use serde_json::{Value, json};

use super::coordination::{CoordinationListQuery, CoordinationStatus};
use super::onboarding;
use super::skill_workflow::{Availability, ReadError};
use super::store::{ActivityEvent, Principal, SessionView};
use super::work::{
    Briefing, BriefingQuery, CheckpointKind, ClaimStatus, WorkItem, WorkOutcomeKind, WorkOverview,
    WorkOverviewQuery, WorkSource, WorkState,
};
use super::{HubError, HubState};

const MAX_BLOCKS: usize = 90;
const MAX_TEXT_CHARS: usize = 2_000;
const MAX_SECTION_CHARS: usize = 1_800;
const MAX_LIST_ITEMS: usize = 24;
const MAX_CURRENT_ITEMS: usize = 12;
const MAX_OVERVIEW_ITEMS: usize = 10;
const SLACK_SESSION: &str = "slack-human";
const SLACK_BRIEFING_SUBSCRIBER: &str = "slack-home";

/// Renders one private Slack App Home page without registering a session or heartbeat.
pub(super) fn render(
    state: &HubState,
    principal: &Principal,
    page: &str,
    now: i64,
) -> super::Result<Value> {
    let mut home = Home::new(page, principal, now);
    match page {
        "status" => render_status(&mut home, state, principal, now)?,
        "work" => render_work(&mut home, state, principal, now)?,
        "briefing" => render_briefing(&mut home, state, principal, now)?,
        "coordination" => render_coordination(&mut home, state, principal, now)?,
        "handoffs" => render_handoffs(&mut home, state, principal)?,
        "workflows" => render_workflows(&mut home, state, principal)?,
        value if value.starts_with("work:") => {
            render_work_detail(&mut home, state, principal, &value[5..], now)?;
        }
        value if value.starts_with("workflow:") => {
            render_workflow_detail(&mut home, state, principal, &value[9..])?;
        }
        _ => render_help(&mut home),
    }
    Ok(home.finish())
}

struct Home {
    blocks: Vec<Value>,
    refreshed_at: i64,
    display_partial: bool,
}

impl Home {
    fn new(page: &str, principal: &Principal, now: i64) -> Self {
        let mut home = Self {
            blocks: Vec::with_capacity(32),
            refreshed_at: now,
            display_partial: false,
        };
        home.heading("AgentSCM");
        home.actions(navigation(page, !principal.read_only));
        home
    }

    fn push(&mut self, block: Value) {
        // Keep one block for the timestamp and explicit display-completeness footer.
        if self.blocks.len() < MAX_BLOCKS - 1 {
            self.blocks.push(block);
        } else {
            self.display_partial = true;
        }
    }

    fn heading(&mut self, text: &str) {
        self.push(json!({"type": "header", "text": plain_text(text, 150)}));
    }

    fn section(&mut self, text: impl AsRef<str>) {
        self.push(json!({
            "type": "section",
            "text": plain_text(text.as_ref(), MAX_SECTION_CHARS)
        }));
    }

    fn section_with_actions(&mut self, text: impl AsRef<str>, elements: Vec<Value>) {
        self.section(text);
        if !elements.is_empty() {
            self.actions(elements);
        }
    }

    fn actions(&mut self, elements: Vec<Value>) {
        self.push(json!({"type": "actions", "elements": elements}));
    }

    fn divider(&mut self) {
        self.push(json!({"type": "divider"}));
    }

    fn mark_partial(&mut self) {
        self.display_partial = true;
    }

    fn finish(mut self) -> Value {
        self.blocks.push(json!({
            "type": "context",
            "elements": [plain_text(
                &format!(
                    "Refreshed at Unix {}. Slack display partial: {}.",
                    self.refreshed_at,
                    yes_no(self.display_partial)
                ),
                200
            )]
        }));
        json!({"type": "home", "blocks": self.blocks})
    }
}

fn navigation(current: &str, allow_write_controls: bool) -> Vec<Value> {
    let mut elements = [
        ("Status", "status"),
        ("Work", "work"),
        ("Briefing", "briefing"),
        ("Coordination", "coordination"),
        ("Handoffs", "handoffs"),
        ("Workflows", "workflows"),
        ("Help", "help"),
    ]
    .into_iter()
    .map(|(label, page)| page_button(label, page))
    .collect::<Vec<_>>();
    elements.push(json!({
        "type": "button",
        "text": plain_text("Refresh", 75),
        "action_id": "agentscm_page:refresh",
        "value": bounded_value(current)
    }));
    if allow_write_controls {
        elements.push(json!({
            "type": "button",
            "text": plain_text("Start work", 75),
            "action_id": "agentscm_start",
            "value": "start",
            "style": "primary"
        }));
    }
    elements
}

fn page_button(label: &str, page: &str) -> Value {
    json!({
        "type": "button",
        "text": plain_text(label, 75),
        "action_id": format!("agentscm_page:{page}"),
        "value": bounded_value(page)
    })
}

fn checkpoint_button(work_id: &str) -> Value {
    json!({
        "type": "button",
        "text": plain_text("Checkpoint", 75),
        "action_id": "agentscm_checkpoint",
        "value": bounded_value(work_id)
    })
}

fn render_status(
    home: &mut Home,
    state: &HubState,
    principal: &Principal,
    now: i64,
) -> super::Result<()> {
    let sessions = state
        .store
        .live_sessions(principal, now, MAX_LIST_ITEMS + 1)?;
    let truncated = sessions.len() > MAX_LIST_ITEMS;
    home.heading("Status");
    home.section(format!(
        "Repository: {}\n{} live session report(s). Live means self-reported AgentSCM activity within the session window; it does not show or prove remote execution.",
        principal.repository,
        sessions.len().min(MAX_LIST_ITEMS)
    ));
    if sessions.is_empty() {
        home.section("No recently self-reported sessions are visible in this repository scope.");
    } else {
        for session in sessions.iter().take(MAX_LIST_ITEMS) {
            home.section(session_summary(session, now));
        }
        if truncated {
            home.mark_partial();
            home.section("Additional self-reported sessions are omitted from this bounded view.");
        }
    }

    let readiness = onboarding::repository_status(state, principal, now)?;
    home.heading("Agent readiness");
    home.section(format!(
        "{} current readiness report(s). Readiness requires recent MCP and native edit-hook observations for the same binding and expires; it is coordination evidence, not remote execution or future availability.",
        readiness.len().min(MAX_LIST_ITEMS)
    ));
    if readiness.is_empty() {
        home.section("No current readiness observations are visible in this repository scope.");
    }
    for status in readiness.iter().take(MAX_LIST_ITEMS) {
        let blocker = status
            .blocker
            .map_or_else(|| "none reported".into(), debug_name);
        home.section(format!(
            "{} ({}) by {}\nState: {}; blocker: {}; MCP verified: {}; native edit-hook verified: {}\nExpires: Unix {} ({})\nNext action: {}",
            status.name,
            debug_name(status.agent),
            status.owner,
            status.state,
            blocker,
            yes_no(status.mcp_verified),
            yes_no(status.native_verified),
            status.expires_at,
            expiry(status.expires_at, now),
            status.next_action
        ));
    }
    if readiness.len() > MAX_LIST_ITEMS {
        home.mark_partial();
        home.section("Additional readiness observations are omitted from this bounded view.");
    }
    Ok(())
}

fn session_summary(session: &SessionView, now: i64) -> String {
    let task = session.task.as_deref().unwrap_or("task not reported");
    format!(
        "{} by {}\n{}\nLast self-report: {}",
        session.agent,
        session.owner,
        task,
        relative_time(session.updated_at, now)
    )
}

fn render_work(
    home: &mut Home,
    state: &HubState,
    principal: &Principal,
    now: i64,
) -> super::Result<()> {
    let overview =
        state
            .store
            .work_overview(&principal.token_id, &WorkOverviewQuery::default(), now)?;
    home.heading("Shared work");
    home.section(overview_coverage(&overview));

    home.heading("Current assignments");
    if overview.current.is_empty() {
        home.section("No unfinished assignments are visible in this bounded overview.");
    }
    for item in overview.current.iter().take(MAX_CURRENT_ITEMS) {
        render_work_item(home, item, principal, true);
    }
    if overview.current.len() > MAX_CURRENT_ITEMS {
        home.mark_partial();
        home.section("Additional current assignments are omitted from this bounded Slack view.");
    }

    home.heading("Blockers");
    let blockers = overview
        .current
        .iter()
        .filter(|item| item.state == WorkState::Blocked || item.blocker.is_some())
        .take(MAX_OVERVIEW_ITEMS)
        .collect::<Vec<_>>();
    if blockers.is_empty() {
        home.section("No reported blockers are visible in this bounded overview.");
    }
    for item in blockers {
        let summary = item
            .blocker
            .as_ref()
            .map_or("blocker summary unavailable", |checkpoint| {
                checkpoint.summary.as_str()
            });
        home.section(format!("{} — {}\n{}", item.id, item.title, summary));
    }

    home.heading("Recent reported outcomes");
    if overview.recent.is_empty() {
        home.section("No recent reported decisions or finishes are visible.");
    }
    for outcome in overview.recent.iter().take(MAX_OVERVIEW_ITEMS) {
        let kind = match outcome.kind {
            WorkOutcomeKind::Decision => "reported decision",
            WorkOutcomeKind::Completion => "reported finished",
        };
        let summary = outcome
            .item
            .current
            .as_ref()
            .map_or(outcome.item.title.as_str(), |checkpoint| {
                checkpoint.summary.as_str()
            });
        home.section(format!(
            "{}: {}\n{}\nReported state is not evidence of tests, review, publication, landing, or merge.",
            kind, outcome.item.title, summary
        ));
    }
    if overview.recent.len() > MAX_OVERVIEW_ITEMS {
        home.mark_partial();
        home.section("Additional recent outcomes are omitted from this bounded Slack view.");
    }

    home.heading("Declared-scope overlaps");
    if overview.overlaps.is_empty() {
        home.section("No declared-scope overlaps are visible in this bounded overview.");
    }
    for overlap in overview.overlaps.iter().take(MAX_OVERVIEW_ITEMS) {
        home.section(format!(
            "{} ↔ {}\n{} overlap reason(s); reasons complete: {}. Coordinate before changing the shared scope.",
            overlap.left.title,
            overlap.right.title,
            overlap.reasons.len(),
            yes_no(overlap.reasons_complete)
        ));
    }
    if overview.overlaps.len() > MAX_OVERVIEW_ITEMS {
        home.mark_partial();
        home.section(
            "Additional declared-scope overlaps are omitted from this bounded Slack view.",
        );
    }
    Ok(())
}

fn overview_coverage(overview: &WorkOverview) -> String {
    format!(
        "Source-page coverage — current: {}; recent: {}; overlaps: {}; retained history gap: {}. Slack display — current: {}; recent: {}; overlaps: {}. Recent window: {} to {}. Empty scope means unknown coverage.",
        complete_word(overview.current_complete),
        complete_word(overview.recent_complete),
        complete_word(overview.overlaps_complete),
        yes_no(overview.history_retention_gap),
        complete_word(overview.current.len() <= MAX_CURRENT_ITEMS),
        complete_word(overview.recent.len() <= MAX_OVERVIEW_ITEMS),
        complete_word(overview.overlaps.len() <= MAX_OVERVIEW_ITEMS),
        overview.recent_since,
        overview.until
    )
}

fn render_work_item(home: &mut Home, item: &WorkItem, principal: &Principal, detail_button: bool) {
    let mut actions = Vec::new();
    if detail_button {
        actions.push(page_button("Details", &format!("work:{}", item.id)));
    }
    if !principal.read_only && owns_slack_work(item, principal) {
        actions.push(checkpoint_button(&item.id));
    }
    home.section_with_actions(work_summary(item), actions);
}

fn render_work_detail(
    home: &mut Home,
    state: &HubState,
    principal: &Principal,
    work_id: &str,
    now: i64,
) -> super::Result<()> {
    home.heading("Work detail");
    let Some(item) = state.store.work_get(&principal.token_id, work_id, now)? else {
        home.section("That work item is not available in this repository scope.");
        return Ok(());
    };
    render_work_item(home, &item, principal, false);
    home.divider();
    home.section(format!(
        "Lead: {} by {}\nParticipants: {}\nGeneration: {}\nLast check-in: {}\nSource: {}\nBranch: {}\nScope entries: {}{}",
        item.lead_agent,
        item.owner_login,
        item.participants.len(),
        item.generation,
        relative_time(item.last_checkin_at, now),
        work_source(&item.source),
        item.branch.as_deref().unwrap_or("not reported"),
        item.scope.len(),
        if item.scope.is_empty() {
            " (coverage unknown)"
        } else {
            ""
        }
    ));
    if !item.scope.is_empty() {
        let scope = item
            .scope
            .iter()
            .take(16)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        home.section(format!(
            "Declared repository scope: {}{}",
            scope,
            if item.scope.len() > 16 {
                "; additional entries omitted"
            } else {
                ""
            }
        ));
    }
    if let Some(checkpoint) = &item.current {
        home.section(format!(
            "Current checkpoint: {}\n{}\nNext action: {}",
            checkpoint_kind(checkpoint.kind),
            checkpoint.summary,
            checkpoint.next_action.as_deref().unwrap_or("not reported")
        ));
    }
    if let Some(blocker) = &item.blocker {
        home.section(format!("Reported blocker\n{}", blocker.summary));
    }
    home.section(format!(
        "Dependencies: {}\nRelated work: {}\nA reported finish is not evidence of tests, review, publication, landing, or merge.",
        display_ids(&item.depends_on),
        display_ids(&item.related_work)
    ));
    Ok(())
}

fn render_briefing(
    home: &mut Home,
    state: &HubState,
    principal: &Principal,
    now: i64,
) -> super::Result<()> {
    let query = BriefingQuery {
        subscriber: Some(SLACK_BRIEFING_SUBSCRIBER.into()),
        limit: MAX_LIST_ITEMS,
        ..BriefingQuery::default()
    };
    let briefing = state
        .store
        .work_briefing(&principal.token_id, &query, now)?;
    if !briefing.complete {
        home.mark_partial();
    }
    home.heading("Briefing");
    home.section(briefing_coverage(&briefing));
    home.section("Viewing this page does not acknowledge the delivery. Continue every page before explicitly acknowledging it through a supported AgentSCM client.");
    home.heading("Unfinished work snapshot");
    if briefing.items.is_empty() {
        home.section("No unfinished items appear on this page.");
    }
    for item in briefing.items.iter().take(MAX_LIST_ITEMS) {
        render_work_item(home, item, principal, true);
    }
    home.heading("Changes since catch-up cursor");
    if briefing.events.is_empty() {
        home.section("No work changes appear on this page.");
    }
    for event in briefing.events.iter().take(MAX_LIST_ITEMS) {
        let summary = event
            .item
            .current
            .as_ref()
            .map_or(event.item.title.as_str(), |checkpoint| {
                checkpoint.summary.as_str()
            });
        home.section(format!(
            "{} — {}\n{}\nReported at {}. Reported state is not proof of execution or merge.",
            event.work_id, event.kind, summary, event.at
        ));
    }
    Ok(())
}

fn briefing_coverage(briefing: &Briefing) -> String {
    format!(
        "Bounded page — unfinished items: {}; change events: {}; complete: {}; retention gap: {}; event cursor {} to {}.",
        briefing.items.len(),
        briefing.events.len(),
        yes_no(briefing.complete),
        yes_no(briefing.retention_gap),
        briefing.from,
        briefing.through
    )
}

fn render_coordination(
    home: &mut Home,
    state: &HubState,
    principal: &Principal,
    now: i64,
) -> super::Result<()> {
    let query = CoordinationListQuery {
        active_only: true,
        limit: MAX_LIST_ITEMS,
        ..CoordinationListQuery::default()
    };
    let inbox = state
        .store
        .coordination_list(&principal.token_id, &query, now)?;
    if !inbox.complete {
        home.mark_partial();
    }
    home.heading("Active coordination");
    home.section(format!(
        "{} active request(s) on this bounded page; complete: {}; retention gap: {}. A coordination request records a proposed response and is not observed execution.",
        inbox.requests.len(),
        yes_no(inbox.complete),
        yes_no(inbox.retention_gap)
    ));
    if inbox.requests.is_empty() {
        home.section("No active coordination requests are visible.");
    }
    for request in inbox.requests.iter().take(MAX_LIST_ITEMS) {
        let status = match request.status {
            CoordinationStatus::Pending => "pending",
            CoordinationStatus::Responded => "responded",
            CoordinationStatus::Resolved => "resolved",
            CoordinationStatus::Withdrawn => "withdrawn",
        };
        home.section(format!(
            "{} — {}\nRequested by {} ({})\nStatus: {}; affected work: {}; responses: {} (complete: {}); acknowledgements complete: {}. Request not observed execution.",
            request.id,
            request.proposal.title,
            request.requester.agent,
            request.requester.login,
            status,
            request.targets.len(),
            request.responses.len(),
            yes_no(request.responses_complete),
            yes_no(request.acknowledgements_complete)
        ));
    }
    Ok(())
}

fn render_handoffs(home: &mut Home, state: &HubState, principal: &Principal) -> super::Result<()> {
    let events = state.store.repository_events(principal, 100)?;
    let handoffs = events
        .iter()
        .filter(|event| {
            matches!(
                event.event.kind.as_str(),
                "handoff_request" | "handoff_grant" | "handoff_decline"
            )
        })
        .collect::<Vec<_>>();
    home.heading("Handoffs");
    home.section("These are reported AgentSCM request and answer events. They do not prove that work was executed, transferred, tested, landed, or merged.");
    if handoffs.is_empty() {
        home.section("No recent handoff events are visible in this bounded activity window.");
    }
    for event in handoffs.iter().take(MAX_LIST_ITEMS) {
        home.section(handoff_summary(event));
    }
    if handoffs.len() > MAX_LIST_ITEMS {
        home.mark_partial();
        home.section("Additional handoff events are omitted from this bounded activity window.");
    }
    Ok(())
}

fn handoff_summary(event: &ActivityEvent) -> String {
    let data = serde_json::from_str::<Value>(&event.event.data).unwrap_or(Value::Null);
    let path = data
        .get("paths")
        .and_then(Value::as_array)
        .and_then(|paths| paths.first())
        .and_then(Value::as_str)
        .unwrap_or("path unavailable");
    let note = data
        .get("reason")
        .or_else(|| data.get("note"))
        .and_then(Value::as_str)
        .unwrap_or("no note");
    let kind = match event.event.kind.as_str() {
        "handoff_request" => "requested",
        "handoff_grant" => "granted",
        "handoff_decline" => "declined",
        _ => "reported",
    };
    format!(
        "Handoff {} by {} ({})\n{}\n{}\nReported at {}",
        kind, event.agent, event.owner, path, note, event.event.at
    )
}

fn render_workflows(home: &mut Home, state: &HubState, principal: &Principal) -> super::Result<()> {
    let envelope = state
        .skill_workflows
        .list(&principal.repository)
        .map_err(workflow_error)?;
    home.heading("Native workflow evidence");
    home.section("Retained workflow status is historical evidence. Applicability and landing admission are separate, and this read-only view authorizes neither execution nor landing.");
    if envelope.availability == Availability::Unavailable {
        home.section(format!(
            "Workflow view unavailable: {}",
            envelope.reason.unwrap_or("reason unavailable")
        ));
        return Ok(());
    }
    let runs = envelope.runs.unwrap_or_default();
    if runs.is_empty() {
        home.section("No retained workflow runs are available for this repository binding.");
    }
    for run in runs.iter().take(MAX_LIST_ITEMS) {
        home.section_with_actions(
            format!(
                "{} — {}\nState: {}; sequence: {}; source revision: {}",
                run.run_id,
                run.name,
                debug_name(run.state),
                run.sequence,
                run.revision
            ),
            vec![page_button(
                "Workflow details",
                &format!("workflow:{}", run.run_id),
            )],
        );
    }
    if envelope.next_cursor.is_some() {
        home.mark_partial();
        home.section("More retained workflow runs exist beyond this bounded page.");
    }
    Ok(())
}

fn render_workflow_detail(
    home: &mut Home,
    state: &HubState,
    principal: &Principal,
    run_id: &str,
) -> super::Result<()> {
    home.heading("Workflow detail");
    let envelope = match state.skill_workflows.read(&principal.repository, run_id) {
        Ok(envelope) => envelope,
        Err(ReadError::NotFound | ReadError::InvalidRunId) => {
            home.section("That workflow run is not available in this repository binding.");
            return Ok(());
        }
        Err(error) => return Err(workflow_error(error)),
    };
    if envelope.availability == Availability::Unavailable {
        home.section(format!(
            "Workflow view unavailable: {}",
            envelope.reason.unwrap_or("reason unavailable")
        ));
        return Ok(());
    }
    let Some(projection) = envelope.projection else {
        home.section("Workflow projection unavailable.");
        return Ok(());
    };
    home.section(format!(
        "Run: {}\nState: {}\nSource repository binding: {}\nExact source revision: {}\nPolicy epoch: {}\nAdmission: {}",
        projection.run.run_id,
        debug_name(projection.run.state),
        projection.run.source.repository,
        projection.run.source.revision,
        projection.run.source.policy_epoch,
        projection.run.admission
    ));
    home.section(format!(
        "Present applicability\nState: {}\nBasis: {}\nReason: {}",
        debug_name(projection.present_applicability.state),
        projection.present_applicability.basis,
        projection
            .present_applicability
            .reason
            .as_deref()
            .unwrap_or("none reported")
    ));
    if let Some(binding) = &projection.present_landing_binding {
        home.section(format!(
            "Exact landing binding\nState: {}\nBasis: {}\nChange: {} at version {}\nContext digest: {}\nTarget: {}\nReason: {}",
            debug_name(binding.state),
            binding.basis,
            binding.change_id.as_deref().unwrap_or("unavailable"),
            binding
                .change_version
                .map_or_else(|| "unavailable".into(), |version| version.to_string()),
            binding.context_digest.as_deref().unwrap_or("unavailable"),
            binding.target.as_deref().unwrap_or("unavailable"),
            binding.reason.as_deref().unwrap_or("none reported")
        ));
    } else {
        home.section(
            "Exact landing binding: this retained workflow definition has no landing binding.",
        );
    }
    home.heading("Step evidence status");
    for step in projection.run.steps.iter().take(MAX_LIST_ITEMS) {
        let source = if let Some(execution) = &step.execution {
            format!(
                "retained command authority record; attempt {}; generation {}; state {}; outcome {}; record digest {}",
                execution.attempt_id,
                execution.generation,
                execution.state,
                execution.outcome.as_deref().unwrap_or("unavailable"),
                execution.record_digest.as_deref().unwrap_or("unavailable")
            )
        } else if let Some(review) = &step.review {
            format!(
                "retained review observation; requested model {}; observed model {}; status {}; findings {}; unresolved {}",
                review.model_requested,
                review.model_observed.as_deref().unwrap_or("unavailable"),
                review.status,
                review.findings.len(),
                review.unresolved.len()
            )
        } else if !step.observations.is_empty() {
            format!(
                "retained source observations; {} observation(s); paths omitted",
                step.observations.len()
            )
        } else {
            "no retained evidence source shown".into()
        };
        home.section(format!(
            "{}\nStatus: {}; required: {}; evidence source: {}",
            step.id,
            debug_name(step.state),
            yes_no(step.required),
            source
        ));
    }
    if projection.run.steps.len() > MAX_LIST_ITEMS {
        home.mark_partial();
        home.section("Additional step statuses are omitted from this bounded Slack view.");
    }
    home.section("Raw step output, model transcripts, local paths, and credentials are never included in this Slack view.");
    Ok(())
}

fn render_help(home: &mut Home) {
    home.heading("Help");
    home.section("AgentSCM Slack commands open private, repository-scoped views. Read views do not register native readiness or heartbeat activity.");
    for command in [
        "/agentscm status — self-reported live sessions",
        "/agentscm work — current work, blockers, outcomes, and overlaps",
        "/agentscm work ID — one work item",
        "/agentscm briefing — isolated, unacknowledged catch-up delivery",
        "/agentscm coordination — active coordination requests",
        "/agentscm handoffs — recent handoff events",
        "/agentscm workflows — retained native workflow evidence",
        "/agentscm start — open the structured start modal",
        "/agentscm checkpoint ID — checkpoint Slack-owned work",
    ] {
        home.section(command);
    }
}

fn work_summary(item: &WorkItem) -> String {
    let next = item.next_action.as_deref().unwrap_or("not reported");
    format!(
        "{} — {}\nState: {}; claim freshness: {}; lead: {} by {}\nNext action: {}",
        item.id,
        item.title,
        work_state(item.state),
        claim_status(item.claim_status),
        item.lead_agent,
        item.owner_login,
        next
    )
}

fn work_source(source: &WorkSource) -> String {
    match source {
        WorkSource::Native { .. } => "native request binding".into(),
        WorkSource::Forge {
            provider,
            host,
            repository,
            issue,
        } => format!("{provider} issue {issue} in {repository} on {host}"),
    }
}

fn owns_slack_work(item: &WorkItem, principal: &Principal) -> bool {
    item.lead_token == principal.token_id
        && item.lead_session == SLACK_SESSION
        && matches!(item.state, WorkState::Active | WorkState::Blocked)
}

fn work_state(state: WorkState) -> &'static str {
    match state {
        WorkState::Active => "active",
        WorkState::Blocked => "blocked",
        WorkState::Paused => "paused",
        WorkState::Finished => "reported finished",
        WorkState::Handover => "handover",
    }
}

fn claim_status(status: ClaimStatus) -> &'static str {
    match status {
        ClaimStatus::Active => "active",
        ClaimStatus::Stale => "stale",
        ClaimStatus::Unknown => "unknown",
    }
}

fn checkpoint_kind(kind: CheckpointKind) -> &'static str {
    match kind {
        CheckpointKind::Progress => "progress",
        CheckpointKind::Decision => "decision",
        CheckpointKind::Blocker => "blocker",
        CheckpointKind::Pause => "pause",
        CheckpointKind::Finish => "reported finish",
        CheckpointKind::Handover => "handover",
    }
}

fn relative_time(at: i64, now: i64) -> String {
    let Some(age) = now.checked_sub(at) else {
        return "time unavailable".into();
    };
    if age < 0 {
        "clock difference detected".into()
    } else if age < 60 {
        format!("{age}s ago")
    } else if age < 3_600 {
        format!("{}m ago", age / 60)
    } else {
        format!("{}h ago", age / 3_600)
    }
}

fn expiry(at: i64, now: i64) -> String {
    match at.checked_sub(now) {
        Some(remaining) if remaining > 0 => format!("in {remaining}s"),
        Some(_) => "expired".into(),
        None => "time unavailable".into(),
    }
}

fn display_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        return "none reported".into();
    }
    ids.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn complete_word(value: bool) -> &'static str {
    if value { "complete" } else { "partial" }
}

fn debug_name(value: impl std::fmt::Debug) -> String {
    format!("{value:?}").to_ascii_lowercase()
}

fn workflow_error(error: ReadError) -> HubError {
    match error {
        ReadError::InvalidRunId => HubError::Invalid("invalid workflow run ID".into()),
        ReadError::NotFound => HubError::Invalid("workflow run not found".into()),
        ReadError::Internal(_) => HubError::Unavailable("workflow view unavailable".into()),
    }
}

fn bounded_value(value: &str) -> String {
    sanitize(value, MAX_TEXT_CHARS)
}

fn plain_text(value: &str, max_chars: usize) -> Value {
    json!({
        "type": "plain_text",
        "text": sanitize(value, max_chars.min(MAX_TEXT_CHARS)),
        "emoji": false
    })
}

fn sanitize(value: &str, max_chars: usize) -> String {
    let mut clean = String::with_capacity(value.len().min(max_chars));
    let mut previous_space = false;
    for character in value.chars().take(max_chars) {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if character == ' ' && previous_space {
            continue;
        }
        previous_space = character == ' ';
        clean.push(character);
    }
    let clean = clean.trim();
    if clean.is_empty() {
        "—".into()
    } else {
        clean.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_bounded_and_strips_controls_without_enabling_mrkdwn() {
        let input = format!("<@U123>\n{}\u{0007}", "x".repeat(MAX_TEXT_CHARS + 20));
        let value = plain_text(&input, MAX_TEXT_CHARS);
        assert_eq!(value["type"], "plain_text");
        let text = value["text"].as_str().unwrap();
        assert!(text.chars().count() <= MAX_TEXT_CHARS);
        assert!(!text.chars().any(char::is_control));
        assert!(text.starts_with("<@U123> "));
    }

    #[test]
    fn home_never_exceeds_slack_block_limit() {
        let mut home = Home {
            blocks: Vec::new(),
            refreshed_at: 42,
            display_partial: false,
        };
        for _ in 0..(MAX_BLOCKS * 2) {
            home.section("bounded");
        }
        let value = home.finish();
        assert_eq!(value["type"], "home");
        assert_eq!(value["blocks"].as_array().unwrap().len(), MAX_BLOCKS);
        assert!(
            value["blocks"][MAX_BLOCKS - 1]["elements"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Slack display partial: yes")
        );
    }

    #[test]
    fn navigation_uses_only_declared_action_contracts() {
        let buttons = navigation("work:wrk_0123456789abcdef0123456789abcdef", true);
        assert!(buttons.iter().any(|button| {
            button["action_id"] == "agentscm_page:work" && button["value"] == "work"
        }));
        assert!(buttons.iter().any(|button| {
            button["action_id"] == "agentscm_start" && button["value"] == "start"
        }));
        let action_ids = buttons
            .iter()
            .filter_map(|button| button["action_id"].as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(action_ids.len(), buttons.len());

        let read_only = navigation("status", false);
        assert!(
            read_only
                .iter()
                .all(|button| button["action_id"] != "agentscm_start")
        );
    }
}
