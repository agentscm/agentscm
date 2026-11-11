//! Transactional named-path consent storage. Admission binds requester acceptance separately.
use super::*;
use crate::hub::intent::text;
use crate::hub::work::validate_id;
use crate::hub::work_overlap::{
    self as contract, WorkOverlapConsent, WorkOverlapConsentRequest, WorkOverlapConsentState,
    WorkOverlapConsentView,
};

pub(super) fn reservation(
    tx: &mut postgres::Transaction<'_>,
    id: &str,
    now: i64,
) -> Result<(i64, bool)> {
    let row=tx.query_one("SELECT overlap_epoch, reservation_basis IS NOT NULL AND COALESCE(reservation_expires_at > $2, false) FROM hub_work_items WHERE id=$1", &[&id,&now])?;
    Ok((row.get(0), row.get(1)))
}

pub(super) fn view(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    consent: WorkOverlapConsent,
    now: i64,
) -> Result<WorkOverlapConsentView> {
    let target = scoped_work(tx, who, &consent.target.work_id, now, false)?;
    let valid_target = if let Some(item) = target {
        let (epoch, live) = reservation(tx, &item.id, now)?;
        contract::matches(&consent.target, &item, epoch, live)
    } else {
        false
    };
    let withdrawn: bool = tx.query_one(
        "SELECT status='withdrawn' OR (status='resolved' AND $3) OR EXISTS(SELECT 1 FROM hub_coordination_responses WHERE request=$1 AND target_work=$2 AND kind='decline' AND event_seq>$4) FROM hub_coordination_requests WHERE id=$1",
        &[&consent.request_id,&consent.target.work_id,&consent.accepted.is_none(),&consent.event_seq])?.get(0);
    let valid_target = valid_target && !withdrawn;
    let mut state = if valid_target {
        WorkOverlapConsentState::AwaitingRequester
    } else {
        WorkOverlapConsentState::Invalidated
    };
    if valid_target && let Some(accepted) = &consent.accepted {
        state = if let Some(item) = scoped_work(tx, who, &accepted.work_id, now, false)? {
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
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    request: &CoordinationRequest,
    session: &str,
    now: i64,
) -> Result<()> {
    if coordination_is_requester(request, who, session) {
        return Ok(());
    }
    for target in &request.targets {
        if let Some(item) = scoped_work(tx, who, &target.work_id, now, false)?
            && item.lead_token == who.token
            && item.lead_session == session
        {
            return Ok(());
        }
    }
    Err(HubError::Unauthorized)
}

impl PgStore {
    pub(super) fn overlap_consent(
        &self,
        token: &str,
        input: &WorkOverlapConsentRequest,
        now: i64,
    ) -> Result<WorkOverlapConsentView> {
        let paths = input.validate()?;
        let payload = json_string(input)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            lock_work_capacity(&mut tx, &who)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            let request = coordination_request_at(&mut tx, &who, &input.request_id, i64::MAX, now)?
                .ok_or_else(|| contract::invalid("unknown coordination request"))?;
            let item = scoped_work(&mut tx, &who, &input.target_work_id, now, true)?
                .ok_or(HubError::Unauthorized)?;
            // Replay checks the current lead and reprojects validity. It never reactivates consent.
            if item.lead_token != who.token || item.lead_session != input.session {
                return Err(HubError::Unauthorized);
            }
            let replay = tx.query_opt(
                "SELECT payload,snapshot FROM hub_work_overlap_consents
                 WHERE request=$1 AND target_work=$2 AND operation_id=$3",
                &[&input.request_id, &input.target_work_id, &input.operation_id],
            )?;
            if let Some(row) = replay {
                let stored: String = row.get(0);
                let snapshot: String = row.get(1);
                if stored != payload {
                    return Err(contract::invalid("overlap consent operation_id reused with changed payload"));
                }
                let result = view(&mut tx, &who, json_value(&snapshot)?, now)?;
                tx.commit()?;
                return Ok(result);
            }
            let (epoch, live) = reservation(&mut tx, &item.id, now)?;
            contract::validate_target(&request, input, &item, &who.token, live, &paths)?;
            let count: i64 = tx.query_one(
                "SELECT COUNT(*) FROM hub_work_overlap_consents WHERE request=$1",
                &[&request.id],
            )?.get(0);
            if count >= contract::MAX_OVERLAP_CONSENTS as i64 {
                return Err(who.capacity(CapacityLimit::CoordinationUpdates));
            }
            let id = new_id("woc")?;
            let event = coordination_event(
                "coordination_responded", &request.id, now,
                json!({"kind":"overlap_consent", "consent_id": id, "target_work_id": item.id,
                    "paths": paths, "token": who.token, "session": input.session}),
            )?;
            let seq = append(&mut tx, &event)?;
            let consent = WorkOverlapConsent {
                id, request_id: request.id.clone(), request_version: request.updated_seq,
                requester: request.requester, proposal: request.proposal,
                target: contract::fence(&item, epoch), paths: paths.clone(), accepted: None,
                created_at: now, event_seq: seq,
            };
            let snapshot = json_string(&consent)?;
            tx.execute(
                "INSERT INTO hub_work_overlap_consents(id,request,target_work,operation_id,payload,snapshot)
                 VALUES ($1,$2,$3,$4,$5,$6)",
                &[&consent.id, &consent.request_id, &item.id, &input.operation_id, &payload, &snapshot],
            )?;
            save_coordination_version(&mut tx, &consent.request_id, request.status,
                consent.proposal.work_id.as_deref(), now, seq)?;
            adjust_work_payload_capacity(&mut tx, &who,
                coordination_payload_bytes(&[&payload, &snapshot, ""], &event)?, now)?;
            let result = view(&mut tx, &who, consent, now)?;
            tx.commit()?;
            Ok(result)
        })
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
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            lock_work_capacity(&mut tx, &who)?;
            let request = coordination_request_at(&mut tx, &who, request_id, i64::MAX, now)?
                .ok_or_else(|| contract::invalid("unknown coordination request"))?;
            authorize_read(&mut tx, &who, &request, session, now)?;
            let snapshots = tx.query(
                "SELECT snapshot FROM hub_work_overlap_consents WHERE request=$1 ORDER BY id LIMIT 101",
                &[&request_id],
            )?.iter().map(|row| row.get::<_, String>(0)).collect::<Vec<_>>();
            if snapshots.len() > contract::MAX_OVERLAP_CONSENTS {
                return Err(HubError::WorkOverlapIncomplete);
            }
            let result = snapshots.into_iter()
                .map(|s| view(&mut tx, &who, json_value(&s)?, now)).collect::<Result<Vec<_>>>()?;
            tx.commit()?;
            Ok(result)
        })
    }
}
