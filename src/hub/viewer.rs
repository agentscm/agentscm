//! What a caller sees of agents another owner runs. An agent session is shown by its
//! `session_key` digest, never by its native id, and a stored token label loses the host name
//! that older clients put in it. Each owner's own agents stay exactly as stored, because their
//! clients compare and send back their own session ids. The digest is a pure function of the
//! token and session, so two fields naming one session still compare equal after redaction.
//!
//! Hub logic reads raw records; redaction happens only where a reply leaves the Hub.

use super::HubError;
use super::catchup::{CatchupPage, CatchupPreview};
use super::coordination::{CoordinationAgent, CoordinationInbox, CoordinationRequest};
use super::store::session_key;
use super::work::{
    Briefing, Checkpoint, ReclaimedHold, SourceRef, WorkCheckpointResult, WorkItem, WorkOverview,
    WorkParticipant, WorkScopeOverlap, WorkStartConflict, WorkStartResult,
};
use super::work_http::actionable_status::{ActionableStatus, StatusRecipient};
use super::work_links::{WorkPrLinkResult, WorkPrLinkView};
use super::work_preflight::WorkPreflightResult;
use super::work_transition::WorkTransitionResult;

/// The owner id (`usr_…`) of the person or token a reply is for.
#[derive(Clone, Copy, Debug)]
pub struct Viewer<'a>(pub &'a str);

impl Viewer<'_> {
    /// Replaces `session` with its digest unless this viewer owns it.
    pub(super) fn session(self, owner: Option<&str>, token: &str, session: &mut String) {
        if owner != Some(self.0) {
            *session = session_key(token, session);
        }
    }

    /// Masks a host-named `label` unless this viewer owns it.
    pub(super) fn label(self, owner: Option<&str>, label: &mut String) {
        if owner != Some(self.0) {
            *label = masked_label(label);
        }
    }

    /// An agent session `owner` runs, `None` when the record does not say whose it is.
    pub(super) fn agent(
        self,
        owner: Option<&str>,
        token: &str,
        agent: &mut String,
        session: &mut String,
    ) {
        self.label(owner, agent);
        self.session(owner, token, session);
    }

    /// The owner and token of a native session captured for `participants`' Work: the
    /// participant whose session it is, else `fallback`, the agent that reported it.
    pub(super) fn capturer<'a>(
        participants: &'a [WorkParticipant],
        session: &str,
        fallback: (Option<&'a str>, &'a str),
    ) -> (Option<&'a str>, &'a str) {
        participants
            .iter()
            .find(|p| p.session == session)
            .map_or(fallback, |p| (Some(p.owner.as_str()), p.token.as_str()))
    }

    /// A checkpoint of a Work with these `participants`; an author not among them is shown as
    /// another owner's.
    pub(super) fn checkpoint(self, participants: &[WorkParticipant], checkpoint: &mut Checkpoint) {
        let author = participants
            .iter()
            .find(|p| p.token == checkpoint.token && p.session == checkpoint.session)
            .map(|p| p.owner.as_str());
        let Checkpoint {
            token,
            agent,
            session,
            source_refs,
            ..
        } = checkpoint;
        for reference in source_refs {
            if let SourceRef::CapturedEvent { session_id, .. } = reference {
                let (owner, by) =
                    Self::capturer(participants, session_id, (author, token.as_str()));
                self.session(owner, by, session_id);
            }
        }
        self.agent(author, token, agent, session);
    }

    pub(super) fn work(self, item: &mut WorkItem) {
        // Owners come from the stored participants; a lead not among them is shown as another
        // owner's.
        let participants = item.participants.clone();
        let lead = participants
            .iter()
            .find(|p| p.token == item.lead_token && p.session == item.lead_session)
            .map(|p| p.owner.as_str());
        self.agent(
            lead,
            &item.lead_token,
            &mut item.lead_agent,
            &mut item.lead_session,
        );
        for checkpoint in [&mut item.current, &mut item.blocker].into_iter().flatten() {
            self.checkpoint(&participants, checkpoint);
        }
        for p in &mut item.participants {
            self.agent(Some(&p.owner), &p.token, &mut p.agent, &mut p.session);
        }
    }

    fn coordination_agent(self, agent: &mut CoordinationAgent) {
        self.agent(
            Some(&agent.owner),
            &agent.token,
            &mut agent.agent,
            &mut agent.session,
        );
    }

    fn coordination(self, request: &mut CoordinationRequest) {
        self.coordination_agent(&mut request.requester);
        for target in &mut request.targets {
            self.coordination_agent(&mut target.lead);
        }
        for ack in &mut request.acknowledgements {
            self.coordination_agent(&mut ack.agent);
        }
        for response in &mut request.responses {
            self.coordination_agent(&mut response.responder);
        }
    }
}

/// `value` as the owner `viewer` may see it.
pub fn shown<T: Shown>(mut value: T, viewer: &str) -> T {
    value.show(Viewer(viewer));
    value
}

/// A reply part that names agents, made into what `viewer` may see. Each reply is shown once,
/// where it leaves the Hub: a second pass would digest a digest. The operational projection is
/// not here because the stores already build it for its reader.
pub trait Shown {
    fn show(&mut self, viewer: Viewer<'_>);
}

impl<T: Shown> Shown for Vec<T> {
    fn show(&mut self, viewer: Viewer<'_>) {
        for item in self {
            item.show(viewer);
        }
    }
}

impl<T: Shown> Shown for Option<T> {
    fn show(&mut self, viewer: Viewer<'_>) {
        if let Some(item) = self {
            item.show(viewer);
        }
    }
}

impl<T: Shown> Shown for Box<T> {
    fn show(&mut self, viewer: Viewer<'_>) {
        (**self).show(viewer);
    }
}

impl Shown for WorkItem {
    fn show(&mut self, viewer: Viewer<'_>) {
        viewer.work(self);
    }
}

impl Shown for CoordinationRequest {
    fn show(&mut self, viewer: Viewer<'_>) {
        viewer.coordination(self);
    }
}

impl Shown for super::work_overlap::WorkOverlapConsentView {
    fn show(&mut self, viewer: Viewer<'_>) {
        viewer.coordination_agent(&mut self.consent.requester);
        viewer.coordination_agent(&mut self.consent.target.lead);
        if let Some(pending) = &mut self.consent.pending_requester {
            viewer.coordination_agent(&mut pending.lead);
        }
        if let Some(accepted) = &mut self.consent.accepted {
            viewer.coordination_agent(&mut accepted.lead);
        }
    }
}

impl Shown for CoordinationInbox {
    fn show(&mut self, viewer: Viewer<'_>) {
        self.requests.show(viewer);
    }
}

impl Shown for WorkOverview {
    fn show(&mut self, viewer: Viewer<'_>) {
        self.current.show(viewer);
        for outcome in &mut self.recent {
            outcome.item.show(viewer);
        }
        for pair in &mut self.overlaps {
            pair.left.show(viewer);
            pair.right.show(viewer);
        }
    }
}

impl Shown for WorkScopeOverlap {
    fn show(&mut self, viewer: Viewer<'_>) {
        self.item.show(viewer);
    }
}

impl Shown for WorkStartConflict {
    fn show(&mut self, viewer: Viewer<'_>) {
        self.overlaps.show(viewer);
    }
}

/// A lapsed hold names no owner, so its label is masked for everyone.
impl Shown for ReclaimedHold {
    fn show(&mut self, _: Viewer<'_>) {
        self.lead_agent = masked_label(&self.lead_agent);
    }
}

impl Shown for WorkPreflightResult {
    fn show(&mut self, viewer: Viewer<'_>) {
        self.subject_match.show(viewer);
        self.scope_overlaps.show(viewer);
    }
}

impl Shown for WorkStartResult {
    fn show(&mut self, viewer: Viewer<'_>) {
        self.item.show(viewer);
        self.conflict.show(viewer);
        self.context.show(viewer);
        self.coordination_request.show(viewer);
        self.preflight.show(viewer);
        self.reclaimed.show(viewer);
    }
}

impl Shown for WorkCheckpointResult {
    fn show(&mut self, viewer: Viewer<'_>) {
        self.item.show(viewer);
        self.reclaimed.show(viewer);
    }
}

impl Shown for WorkTransitionResult {
    fn show(&mut self, viewer: Viewer<'_>) {
        self.item.show(viewer);
        self.conflict.show(viewer);
        self.reclaimed.show(viewer);
    }
}

impl Shown for Briefing {
    fn show(&mut self, viewer: Viewer<'_>) {
        self.items.show(viewer);
        for event in &mut self.events {
            event.item.show(viewer);
        }
    }
}

impl Shown for CatchupPage {
    fn show(&mut self, viewer: Viewer<'_>) {
        for event in &mut self.work_events {
            event.item.show(viewer);
        }
    }
}

impl Shown for CatchupPreview {
    fn show(&mut self, viewer: Viewer<'_>) {
        for event in &mut self.work_events {
            event.item.show(viewer);
        }
    }
}

impl Shown for WorkPrLinkView {
    fn show(&mut self, viewer: Viewer<'_>) {
        if let Some(link) = &mut self.link {
            viewer.agent(
                Some(&link.author),
                &link.token,
                &mut link.agent,
                &mut link.session,
            );
        }
    }
}

impl Shown for WorkPrLinkResult {
    fn show(&mut self, viewer: Viewer<'_>) {
        self.view.show(viewer);
    }
}

impl Shown for ActionableStatus {
    fn show(&mut self, viewer: Viewer<'_>) {
        let mut recipient = |r: &mut StatusRecipient| {
            if let Some(agent) = &mut r.agent {
                viewer.label(r.owner.as_deref(), agent);
            }
        };
        for work in &mut self.work {
            recipient(&mut work.recipient);
        }
        for action in &mut self.actions {
            action.recipients.iter_mut().for_each(&mut recipient);
        }
    }
}

impl HubError {
    /// The parts of a work refusal that name agents, as `viewer` may see them.
    pub(super) fn show(&mut self, viewer: Viewer<'_>) {
        match self {
            Self::WorkOverlap(overlaps) => overlaps.show(viewer),
            Self::WorkCoordinationRequired { overlaps, request } => {
                overlaps.show(viewer);
                request.show(viewer);
            }
            Self::WorkPreflight(preflight) => preflight.show(viewer),
            _ => {}
        }
    }
}

/// A stored label as read by its owner (`own`) or by anyone else.
pub(super) fn shown_label(label: String, own: bool) -> String {
    if own { label } else { masked_label(&label) }
}

/// A stored token label without a host name: older clients named tokens
/// `client-<host name>-<hex>`, and this keeps `client-host-<hex>`, the name they used when the
/// host name was empty. Current `client-<16 hex machine id>-<hex>` labels, brands and names a
/// user chose stay as they are, as does anything else.
pub fn masked_label(label: &str) -> String {
    let Some((middle, id)) = label
        .strip_prefix("client-")
        .and_then(|rest| rest.rsplit_once('-'))
    else {
        return label.to_owned();
    };
    let machine_id = middle.len() == 16
        && middle
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if middle.is_empty() || machine_id || id.len() < 6 || !id.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return label.to_owned();
    }
    format!("client-host-{id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item() -> WorkItem {
        let agent = |owner: &str, token: &str, agent: &str, session: &str, role: &str| {
            json!({"owner": owner, "login": owner, "token": token, "agent": agent,
                "session": session, "role": role})
        };
        serde_json::from_value(json!({
            "id": "wrk_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "repository": "acme/widgets",
            "owner": "usr_alice", "owner_login": "alice",
            "source": {"kind": "native", "request_key": "synthetic"},
            "title": "Synthetic", "state": "active", "claim_status": "active",
            "last_checkin_at": 1, "generation": 1, "lead_token": "tok_alice",
            "lead_agent": "client-alices-laptop-1a2b3c4d", "lead_session": "alice-raw",
            "participants": [
                agent("usr_alice", "tok_alice", "client-alices-laptop-1a2b3c4d", "alice-raw", "lead"),
                agent("usr_bob", "tok_bob", "client-bobs-desktop-5e6f7a8b", "bob-raw", "helper"),
            ],
            "scope": [], "branch": null, "worktree": null, "next_action": null,
            "current": {"kind": "progress", "summary": "s", "next_action": null,
                "source_refs": [{"kind": "captured_event", "session_id": "alice-raw", "event_id": "e1"},
                    {"kind": "captured_event", "session_id": "bob-raw", "event_id": "e2"}],
                "at": 1, "token": "tok_bob", "agent": "client-bobs-desktop-5e6f7a8b",
                "session": "bob-raw", "generation": 1},
            "blocker": null, "depends_on": [], "related_work": [],
            "created_at": 1, "updated_at": 1, "created_seq": 1, "updated_seq": 1
        }))
        .unwrap()
    }

    #[test]
    fn each_owner_keeps_its_own_agents_and_sees_the_other_by_digest() {
        let raw = item();
        let alice = shown(raw.clone(), "usr_alice");
        assert_eq!(
            (alice.lead_session.as_str(), alice.lead_agent.as_str()),
            ("alice-raw", "client-alices-laptop-1a2b3c4d")
        );
        let bob_key = session_key("tok_bob", "bob-raw");
        assert_eq!(alice.participants[1].session, bob_key);
        assert_eq!(alice.participants[1].agent, "client-host-5e6f7a8b");
        let current = alice.current.as_ref().unwrap();
        assert_eq!(
            (current.session.as_str(), current.agent.as_str()),
            (bob_key.as_str(), "client-host-5e6f7a8b")
        );
        let refs = serde_json::to_value(&current.source_refs).unwrap();
        assert_eq!(refs[0]["session_id"], "alice-raw");
        assert_eq!(refs[1]["session_id"], bob_key.as_str());

        let bob = shown(raw, "usr_bob");
        let alice_key = session_key("tok_alice", "alice-raw");
        assert_eq!(bob.lead_session, alice_key);
        assert_eq!(bob.participants[0].session, alice_key);
        assert_eq!(bob.lead_agent, "client-host-1a2b3c4d");
        assert_eq!(bob.participants[1].session, "bob-raw");
        assert_eq!(bob.current.as_ref().unwrap().session, "bob-raw");
        let text = serde_json::to_string(&bob).unwrap();
        assert!(
            !text.contains("alice-raw") && !text.contains("alices-laptop"),
            "{text}"
        );
    }

    #[test]
    fn a_lead_missing_from_the_participants_counts_as_another_owners() {
        let mut item = item();
        item.participants.remove(0);
        let alice = shown(item, "usr_alice");
        assert_eq!(alice.lead_session, session_key("tok_alice", "alice-raw"));
    }

    #[test]
    fn coordination_and_pr_links_name_other_owners_by_digest() {
        let agent = |owner: &str, token: &str, session: &str| {
            json!({"token": token, "agent": format!("client-{owner}-host-0f0f0f"), "session": session,
                "owner": owner, "login": owner})
        };
        let mut request: CoordinationRequest = serde_json::from_value(json!({
            "id": "crq_synthetic", "requester": agent("usr_bob", "tok_bob", "bob-raw"),
            "proposal": {"work_id": null, "source": null, "request_key": "k", "title": "t", "scope": []},
            "targets": [{"work_id": "wrk_a", "lead": agent("usr_alice", "tok_alice", "alice-raw"),
                "generation": 1, "reasons": [], "reasons_complete": true}],
            "status": "pending", "acknowledgements": [{"target_work_id": null,
                "agent": agent("usr_bob", "tok_bob", "bob-raw"), "generation": 1, "through_seq": 1,
                "acknowledged_at": 1, "seq": 1}],
            "acknowledgements_complete": true, "responses": [], "responses_complete": true,
            "created_at": 1, "updated_at": 1, "created_seq": 1, "updated_seq": 1
        }))
        .unwrap();
        request.show(Viewer("usr_alice"));
        assert_eq!(request.targets[0].lead.session, "alice-raw");
        assert_eq!(request.requester.session, session_key("tok_bob", "bob-raw"));
        assert_eq!(request.requester.agent, "client-host-0f0f0f");
        assert_eq!(
            request.acknowledgements[0].agent.session,
            request.requester.session
        );

        let mut view: WorkPrLinkView = serde_json::from_value(json!({
            "work_id": "wrk_a", "repository": "acme/widgets", "repository_id": 1,
            "generation": 1, "updated_seq": 1,
            "link": {"target": {"repository_id": 1, "number": 7, "head_sha": "a".repeat(40)},
                "author": "usr_alice", "token": "tok_alice", "agent": "client-x-host-0f0f0f",
                "session": "alice-raw", "generation": 1, "linked_at": 1, "event_seq": 1},
            "snapshot_status": "not_compared", "head_status": "not_compared"
        }))
        .unwrap();
        view.show(Viewer("usr_bob"));
        let link = view.link.unwrap();
        assert_eq!(link.session, session_key("tok_alice", "alice-raw"));
        assert_eq!(link.agent, "client-host-0f0f0f");
    }

    #[test]
    fn only_host_named_client_labels_are_masked() {
        for (label, shown) in [
            ("client-Jonnys-MacBook-Pro-1a2b3c4d", "client-host-1a2b3c4d"),
            ("client-buildbox-0f0f0f", "client-host-0f0f0f"),
            (
                "client-0123456789abcdef-1a2b3c4d",
                "client-0123456789abcdef-1a2b3c4d",
            ),
            ("client-host-1a2b3c4d", "client-host-1a2b3c4d"),
            ("client-box-12", "client-box-12"),
            ("client--1a2b3c4d", "client--1a2b3c4d"),
            ("studio-1a2b3c4d", "studio-1a2b3c4d"),
            ("claude-code", "claude-code"),
            ("client-box-zzzzzz", "client-box-zzzzzz"),
        ] {
            assert_eq!(masked_label(label), shown, "{label}");
        }
    }
}
