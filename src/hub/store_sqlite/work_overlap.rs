//! Transactional named-path consent storage. Admission binds requester acceptance separately.
use super::*;
use crate::hub::intent::text;
use crate::hub::work::validate_id;
use crate::hub::work_overlap::{
    self as contract, WorkOverlapConsent, WorkOverlapConsentRequest, WorkOverlapConsentState,
    WorkOverlapConsentView,
};

pub(super) fn reservation(tx: &Transaction<'_>, id: &str, now: i64) -> Result<(i64, bool)> {
    Ok(tx.query_row("SELECT overlap_epoch, reservation_basis IS NOT NULL AND COALESCE(reservation_expires_at > ?2, 0) FROM hub_work_items WHERE id=?1", params![id,now], |row| Ok((row.get(0)?,row.get(1)?)))?)
}

pub(super) fn view(
    tx: &Transaction<'_>,
    who: &WorkWho,
    consent: WorkOverlapConsent,
    now: i64,
) -> Result<WorkOverlapConsentView> {
    let target = scoped_work(tx, who, &consent.target.work_id, now)?;
    let valid_target = if let Some(item) = target {
        let (epoch, live) = reservation(tx, &item.id, now)?;
        contract::matches(&consent.target, &item, epoch, live)
    } else {
        false
    };
    let withdrawn: bool = tx.query_row(
        "SELECT status='withdrawn' OR (status='resolved' AND ?3) OR EXISTS(SELECT 1 FROM hub_coordination_responses WHERE request=?1 AND target_work=?2 AND kind='decline' AND event_seq>?4) FROM hub_coordination_requests WHERE id=?1",
        params![consent.request_id,consent.target.work_id,consent.accepted.is_none(),consent.event_seq], |row| row.get(0))?;
    let valid_target = valid_target && !withdrawn;
    // Once accepted, the resulting scope/epoch replaces the pre-expansion requester fence.
    let pending_valid = if consent.accepted.is_some() || consent.proposal.work_id.is_none() {
        true
    } else if let Some(pending) = &consent.pending_requester {
        if consent.proposal.work_id.as_deref() != Some(pending.work_id.as_str()) {
            false
        } else if let Some(item) = scoped_work(tx, who, &pending.work_id, now)? {
            let (epoch, live) = reservation(tx, &item.id, now)?;
            contract::matches(pending, &item, epoch, live)
                && pending.lead.token == consent.requester.token
                && pending.lead.session == consent.requester.session
        } else {
            false
        }
    } else {
        // An older pending grant without its requester fence cannot establish permission.
        false
    };
    let valid_target = valid_target && pending_valid;
    let mut state = if valid_target {
        WorkOverlapConsentState::AwaitingRequester
    } else {
        WorkOverlapConsentState::Invalidated
    };
    if valid_target && let Some(accepted) = &consent.accepted {
        state = if let Some(item) = scoped_work(tx, who, &accepted.work_id, now)? {
            let (epoch, live) = reservation(tx, &item.id, now)?;
            if contract::matches(accepted, &item, epoch, live) {
                WorkOverlapConsentState::Agreed
            } else {
                WorkOverlapConsentState::Invalidated
            }
        } else {
            WorkOverlapConsentState::Invalidated
        };
    }
    Ok(WorkOverlapConsentView { consent, state })
}

fn authorize_read(
    tx: &Transaction<'_>,
    who: &WorkWho,
    request: &CoordinationRequest,
    session: &str,
    now: i64,
) -> Result<()> {
    if coordination_is_requester(request, who, session) {
        return Ok(());
    }
    for target in &request.targets {
        if let Some(item) = scoped_work(tx, who, &target.work_id, now)?
            && item.lead_token == who.token
            && item.lead_session == session
        {
            return Ok(());
        }
    }
    Err(HubError::Unauthorized)
}

impl SqliteStore {
    pub(super) fn overlap_consent(
        &self,
        token: &str,
        input: &WorkOverlapConsentRequest,
        now: i64,
    ) -> Result<WorkOverlapConsentView> {
        let paths = input.validate()?;
        let payload = json_string(input)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let request = coordination_request_at(&tx, &who, &input.request_id, i64::MAX, now)?
            .ok_or_else(|| contract::invalid("unknown coordination request"))?;
        let item =
            scoped_work(&tx, &who, &input.target_work_id, now)?.ok_or(HubError::Unauthorized)?;
        // Replay remains authorized by the current lead. Its recorded state is projected again;
        // neither an old receipt nor a changed request can reactivate an invalidated consent.
        if item.lead_token != who.token || item.lead_session != input.session {
            return Err(HubError::Unauthorized);
        }
        let replay:Option<(String,String)>=tx.query_row("SELECT payload,snapshot FROM hub_work_overlap_consents WHERE request=?1 AND target_work=?2 AND operation_id=?3",params![input.request_id,input.target_work_id,input.operation_id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((stored, snapshot)) = replay {
            if stored != payload {
                return Err(contract::invalid(
                    "overlap consent operation_id reused with changed payload",
                ));
            }
            let result = view(&tx, &who, json_value(&snapshot)?, now)?;
            tx.commit()?;
            return Ok(result);
        }
        let (epoch, live) = reservation(&tx, &item.id, now)?;
        contract::validate_target(&request, input, &item, &who.token, live, &paths)?;
        let pending_requester = if let Some(id) = &request.proposal.work_id {
            let requester = scoped_work(&tx, &who, id, now)?
                .ok_or_else(|| contract::invalid("requester Work is unavailable"))?;
            let (epoch, live) = reservation(&tx, id, now)?;
            let fence = contract::fence(&requester, epoch);
            if requester.lead_token != request.requester.token
                || requester.lead_session != request.requester.session
                || !contract::matches(&fence, &requester, epoch, live)
            {
                return Err(contract::invalid(
                    "requester lead changed or reservation lapsed",
                ));
            }
            Some(fence)
        } else {
            None
        };
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_work_overlap_consents WHERE request=?1",
            [&request.id],
            |row| row.get(0),
        )?;
        if count >= contract::MAX_OVERLAP_CONSENTS as i64 {
            return Err(who.capacity(CapacityLimit::CoordinationUpdates));
        }
        let id = new_id("woc")?;
        let event = coordination_event(
            "coordination_responded",
            &request.id,
            now,
            json!({"kind":"overlap_consent","consent_id":id,"target_work_id":item.id,"paths":paths,"token":who.token,"session":input.session}),
        )?;
        let seq = append(&tx, &event)?;
        let consent = WorkOverlapConsent {
            id,
            request_id: request.id.clone(),
            request_version: request.updated_seq,
            requester: request.requester,
            proposal: request.proposal,
            target: contract::fence(&item, epoch),
            pending_requester,
            paths,
            accepted: None,
            created_at: now,
            event_seq: seq,
        };
        let snapshot = json_string(&consent)?;
        tx.execute("INSERT INTO hub_work_overlap_consents(id,request,target_work,operation_id,payload,snapshot) VALUES (?1,?2,?3,?4,?5,?6)",params![consent.id,consent.request_id,item.id,input.operation_id,payload,snapshot])?;
        save_coordination_version(
            &tx,
            &consent.request_id,
            request.status,
            consent.proposal.work_id.as_deref(),
            now,
            seq,
        )?;
        adjust_work_payload_capacity(
            &tx,
            &who,
            coordination_payload_bytes(&[&payload, &snapshot, ""], &event)?,
            now,
        )?;
        let result = view(&tx, &who, consent, now)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn overlap_consents(
        &self,
        token: &str,
        request_id: &str,
        session: &str,
        now: i64,
    ) -> Result<Vec<WorkOverlapConsentView>> {
        validate_id("coordination request", request_id, "crq")?;
        text("session", session, 255)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let request = coordination_request_at(&tx, &who, request_id, i64::MAX, now)?
            .ok_or_else(|| contract::invalid("unknown coordination request"))?;
        authorize_read(&tx, &who, &request, session, now)?;
        let snapshots=tx.prepare("SELECT snapshot FROM hub_work_overlap_consents WHERE request=?1 ORDER BY id LIMIT 101")?.query_map([request_id],|row| row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
        if snapshots.len() > contract::MAX_OVERLAP_CONSENTS {
            return Err(HubError::WorkOverlapIncomplete);
        }
        let result = snapshots
            .into_iter()
            .map(|s| view(&tx, &who, json_value(&s)?, now))
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(result)
    }
}

pub(super) fn agreements(
    tx: &Transaction<'_>,
    who: &WorkWho,
    id: &str,
    now: i64,
) -> Result<Vec<WorkOverlapConsent>> {
    let snapshots = tx.prepare("SELECT snapshot FROM hub_work_overlap_consents WHERE target_work=?1 OR accepted_work=?1 ORDER BY id LIMIT 101")?.query_map([id],|row|row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
    if snapshots.len() > contract::MAX_OVERLAP_CONSENTS {
        return Err(HubError::WorkOverlapIncomplete);
    }
    let mut out = Vec::new();
    for snapshot in snapshots {
        let v = view(tx, who, json_value(&snapshot)?, now)?;
        if v.state == WorkOverlapConsentState::Agreed {
            out.push(v.consent);
        }
    }
    Ok(out)
}
pub(super) fn annotate_existing(
    tx: &Transaction<'_>,
    who: &WorkWho,
    id: &str,
    scope: &[String],
    overlaps: &mut [WorkScopeOverlap],
    now: i64,
) -> Result<()> {
    let Some(item) = scoped_work(tx, who, id, now)? else {
        return Ok(());
    };
    // A scope mutation ends all prior agreements, including unchanged intersections.
    if item.scope != scope {
        return Ok(());
    }
    for c in agreements(tx, who, id, now)? {
        let other = if c.target.work_id == id {
            c.accepted.as_ref().map(|f| f.work_id.as_str())
        } else {
            Some(c.target.work_id.as_str())
        };
        for overlap in overlaps.iter_mut() {
            if other == Some(overlap.item.id.as_str()) && overlap.their_scope == overlap.item.scope
            {
                overlap.agreed_paths.extend(c.paths.clone());
                overlap.agreed_paths.sort();
                overlap.agreed_paths.dedup();
            }
        }
    }
    Ok(())
}

pub(super) fn accepting(
    tx: &Transaction<'_>,
    who: &WorkWho,
    ids: &[String],
    session: &str,
    proposal: &CoordinationProposal,
    now: i64,
) -> Result<Vec<WorkOverlapConsent>> {
    contract::consent_ids(ids)?;
    let mut out = Vec::new();
    for id in ids {
        let snapshot: String = tx
            .query_row(
                "SELECT snapshot FROM hub_work_overlap_consents WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| contract::invalid("unknown overlap consent"))?;
        let v = view(tx, who, json_value(&snapshot)?, now)?;
        let c = v.consent;
        if c.requester.token != who.token
            || c.requester.session != session
            || c.proposal != *proposal
        {
            return Err(HubError::Unauthorized);
        }
        if v.state == WorkOverlapConsentState::Invalidated {
            return Err(contract::invalid(
                "overlap consent is invalidated; obtain new consent",
            ));
        }
        if let Some(f) = &c.accepted
            && (f.lead.token != who.token || f.lead.session != session || f.scope != proposal.scope)
        {
            return Err(HubError::Unauthorized);
        }
        out.push(c);
    }
    Ok(out)
}
pub(super) fn bind(
    tx: &Transaction<'_>,
    who: &WorkWho,
    consents: &[WorkOverlapConsent],
    item: &WorkItem,
    now: i64,
) -> Result<()> {
    let (epoch, live) = reservation(tx, &item.id, now)?;
    if !live {
        return Err(contract::invalid("requester reservation is not live"));
    }
    for old in consents {
        if let Some(f) = &old.accepted {
            if !contract::matches(f, item, epoch, live) {
                return Err(contract::invalid("accepted overlap cannot be rebound"));
            }
            continue;
        }
        let mut c = old.clone();
        c.accepted = Some(contract::fence(item, epoch));
        let snapshot = json_string(&c)?;
        tx.execute(
            "UPDATE hub_work_overlap_consents SET snapshot=?2,accepted_work=?3 WHERE id=?1",
            params![c.id, snapshot, item.id],
        )?;
        let request = coordination_request_at(tx, who, &c.request_id, i64::MAX, now)?
            .ok_or_else(|| contract::invalid("coordination request expired"))?;
        let event = coordination_event(
            "coordination_responded",
            &c.request_id,
            now,
            json!({"kind":"overlap_accepted","consent_id":c.id,"work_id":item.id}),
        )?;
        let seq = append(tx, &event)?;
        save_coordination_version(
            tx,
            &c.request_id,
            request.status,
            request.proposal.work_id.as_deref(),
            now,
            seq,
        )?;
        adjust_work_payload_capacity(tx, who, coordination_payload_bytes(&[""], &event)?, now)?;
        let delta = work_payload_row_bytes(snapshot.len())?
            - work_payload_row_bytes(json_string(old)?.len())?;
        adjust_work_payload_capacity(tx, who, delta, now)?;
    }
    Ok(())
}

fn mark_preflight(
    result: &mut crate::hub::work_preflight::WorkPreflightResult,
    consents: &[WorkOverlapConsent],
) {
    contract::annotate(&mut result.scope_overlaps, consents);
    if !result
        .scope_overlaps
        .iter()
        .any(WorkScopeOverlap::unresolved)
    {
        result
            .reasons
            .retain(|r| *r != crate::hub::work_preflight::WorkPreflightReason::ActiveScopeOverlap);
        result.status = crate::hub::work_preflight::status(&result.reasons);
    }
}

impl SqliteStore {
    pub(super) fn overlap_start(
        &self,
        token: &str,
        input: &crate::hub::work_overlap::WorkOverlapStart,
        preflight_only: bool,
        now: i64,
    ) -> Result<WorkStartResult> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let scope = input.start.validate(&who)?;
        if input.start.role != WorkRole::Lead || input.start.takeover_generation.is_some() {
            return Err(contract::invalid(
                "overlap acceptance requires the requesting lead without takeover",
            ));
        }
        let existing = resolve_work(&tx, &who, &input.start, now)?;
        let proposal = if let Some(item) = &existing {
            if item.lead_token != who.token || item.lead_session != input.start.session {
                return Err(HubError::Unauthorized);
            }
            CoordinationProposal {
                work_id: Some(item.id.clone()),
                source: None,
                request_key: None,
                title: input.start.title.clone(),
                scope: scope.clone(),
            }
        } else {
            CoordinationProposal {
                work_id: None,
                source: input.start.source.clone(),
                request_key: input.start.request_key.clone(),
                title: input.start.title.clone(),
                scope: scope.clone(),
            }
        };
        // A retry of first admission retains the initial proposal identity in its grants.
        let proposal = if let Some(item) = &existing {
            let mut original = proposal.clone();
            original.work_id = None;
            original.source = input.start.source.clone();
            original.request_key = input.start.request_key.clone();
            if input.start.work_id.is_none() {
                original
            } else {
                let _ = item;
                proposal
            }
        } else {
            proposal
        };
        let consents = accepting(
            &tx,
            &who,
            &input.consent_ids,
            &input.start.session,
            &proposal,
            now,
        )?;
        if existing.is_some() && consents.iter().any(|c| c.accepted.is_none()) {
            return Err(contract::invalid(
                "existing Work cannot bind a new initial-admission consent; use scope checkpoint acceptance",
            ));
        }
        let mut preflight = preflight::evaluate(&tx, &who, &input.start, &scope, now)?;
        mark_preflight(&mut preflight, &consents);
        require_clear_preflight(&who, &input.start, &preflight)?;
        if preflight
            .scope_overlaps
            .iter()
            .any(WorkScopeOverlap::unresolved)
        {
            return Err(HubError::WorkPreflight(Box::new(preflight)));
        }
        if preflight_only {
            // The dedicated preflight caller reads this projection; it creates no Work.
            return Err(HubError::WorkPreflight(Box::new(preflight)));
        }
        let mut result = start_overlap_in(&tx, &who, &input.start, true, &consents, now)?;
        if result.conflict.is_some() || result.coordination_request.is_some() {
            return Err(contract::invalid(
                "overlap admission did not acquire the requested Work",
            ));
        }
        bind(&tx, &who, &consents, &result.item, now)?;
        pickup_in(&tx, &who, &input.start, &result, now)?;
        result.preflight = Some(preflight);
        tx.commit()?;
        Ok(result)
    }
    pub(super) fn overlap_checkpoint(
        &self,
        token: &str,
        input: &crate::hub::work_overlap::WorkOverlapCheckpoint,
        now: i64,
    ) -> Result<WorkCheckpointResult> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let checkpoint = &input.checkpoint;
        let item =
            scoped_work(&tx, &who, &checkpoint.work_id, now)?.ok_or(HubError::Unauthorized)?;
        if item.lead_token != who.token
            || item.lead_session != checkpoint.session
            || item.generation != checkpoint.generation
        {
            return Err(HubError::Unauthorized);
        }
        let scope = checkpoint
            .normalized_scope()?
            .ok_or_else(|| contract::invalid("overlap acceptance requires explicit scope"))?;
        let proposal = CoordinationProposal {
            work_id: Some(item.id.clone()),
            source: None,
            request_key: None,
            title: item.title.clone(),
            scope,
        };
        let consents = accepting(
            &tx,
            &who,
            &input.consent_ids,
            &checkpoint.session,
            &proposal,
            now,
        )?;
        let result = checkpoint_overlap_in(&tx, token, checkpoint, &consents, now)?;
        bind(&tx, &who, &consents, &result.item, now)?;
        tx.commit()?;
        Ok(result)
    }
}

/// Only the two current lead sessions may use their accepted named-file agreement.
pub(super) fn agreed_intent_paths(
    tx: &Transaction<'_>,
    who: &WorkWho,
    session: &str,
    intent: &Intent,
    now: i64,
) -> Result<Vec<String>> {
    let Some(own) = unfinished_for_session(tx, who, session, now)? else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::new();
    for consent in agreements(tx, who, &own.id, now)? {
        let other = if consent.target.work_id == own.id {
            consent.accepted.as_ref()
        } else {
            Some(&consent.target)
        };
        if let Some(other) = other
            && other.lead.token == intent.token
            && intent.session.as_deref() == Some(other.lead.session.as_str())
        {
            paths.extend(consent.paths);
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}
