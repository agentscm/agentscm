//! Incarnation guards run inside the domain mutation's transaction.
use super::*;
use crate::hub::session_lifecycle::{
    self as contract, EndReason, LifecycleEnd, LifecycleEndResult, LifecycleRegister,
    LifecycleState, LifecycleView, Qualification,
};

fn writer(tx: &Transaction<'_>, token: &str, now: i64) -> Result<WorkWho> {
    let who = work_who(tx, token, now)?;
    if who.read_only {
        return Err(HubError::Unauthorized);
    }
    Ok(who)
}

fn loaded(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, String, String, i64)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn decode(session: &str, row: (String, String, String, String, i64)) -> Result<LifecycleView> {
    let (incarnation, start_nonce, state, qualification, observed_at) = row;
    Ok(LifecycleView {
        session: session.into(),
        incarnation,
        start_nonce,
        observed_at,
        state: match state.as_str() {
            "live" => LifecycleState::Live,
            "terminal" => LifecycleState::Terminal,
            _ => return Err(contract::changed()),
        },
        qualification: match qualification.as_str() {
            "unknown" => Qualification::Unknown,
            "qualified" => Qualification::Qualified,
            "mixed" => Qualification::Mixed,
            _ => return Err(contract::changed()),
        },
    })
}

fn current(tx: &Transaction<'_>, token: &str, session: &str) -> Result<Option<LifecycleView>> {
    tx.query_row("SELECT incarnation,start_nonce,state,qualification,observed_at FROM hub_session_incarnations WHERE token=?1 AND session=?2 ORDER BY ordinal DESC LIMIT 1",params![token,session],loaded)
        .optional()?.map(|row|decode(session,row)).transpose()
}

pub(super) fn current_for_domain(
    tx: &Transaction<'_>,
    token: &str,
    session: &str,
) -> Result<Option<LifecycleView>> {
    let canonical = session_name(tx, token, Some(session), 0)?;
    let mut statement=tx.prepare("SELECT incarnation,start_nonce,state,qualification,observed_at,session FROM hub_session_incarnations i WHERE token=?1 AND (session=?2 OR session=?3 OR EXISTS(SELECT 1 FROM hub_session_aliases a WHERE a.token=i.token AND a.observed_session=i.session AND a.canonical_session=?3)) AND ordinal=(SELECT MAX(j.ordinal) FROM hub_session_incarnations j WHERE j.token=i.token AND j.session=i.session) ORDER BY session LIMIT 2")?;
    let rows = statement
        .query_map(params![token, session, canonical], |row| {
            Ok((row.get::<_, String>(5)?, loaded(row)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if rows.len() > 1 {
        return Err(contract::changed());
    }
    rows.into_iter()
        .next()
        .map(|(session, row)| decode(&session, row))
        .transpose()
}

fn current_for_transport(
    tx: &Transaction<'_>,
    token: &str,
    session: &str,
    incarnation: &str,
) -> Result<LifecycleView> {
    let mut statement = tx.prepare(
        "SELECT session FROM hub_session_incarnations WHERE token=?1 AND incarnation=?2 LIMIT 2",
    )?;
    let names = statement
        .query_map(params![token, incarnation], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let [native] = names.as_slice() else {
        return Err(contract::changed());
    };
    if session_name(tx, token, Some(native), 0)? != session_name(tx, token, Some(session), 0)? {
        return Err(contract::changed());
    }
    let view = current_view(tx, token, native)?;
    if view.incarnation != incarnation {
        return Err(contract::changed());
    }
    Ok(view)
}

pub(super) fn get(
    tx: &Transaction<'_>,
    token: &str,
    session: &str,
    now: i64,
) -> Result<Option<LifecycleView>> {
    active_token(tx, token, now)?;
    check_name(session)?;
    current(tx, token, session)
}

fn domain_get(
    tx: &Transaction<'_>,
    token: &str,
    session: &str,
    now: i64,
) -> Result<Option<LifecycleView>> {
    active_token(tx, token, now)?;
    check_name(session)?;
    current_for_domain(tx, token, session)
}

pub(super) fn register(
    tx: &Transaction<'_>,
    token: &str,
    request: &LifecycleRegister,
    now: i64,
) -> Result<LifecycleView> {
    request.validate(now)?;
    writer(tx, token, now)?;
    let current = current(tx, token, &request.session)?;
    if let Some(view) = current
        .as_ref()
        .filter(|view| view.start_nonce == request.start_nonce)
    {
        if view.state != LifecycleState::Live {
            return Err(contract::changed());
        }
        let (operation,original):(String,String)=tx.query_row("SELECT register_operation,register_request FROM hub_session_incarnations WHERE token=?1 AND session=?2 AND incarnation=?3",params![token,request.session,view.incarnation],|row|Ok((row.get(0)?,row.get(1)?)))?;
        if operation == request.operation_id {
            return if original == contract::encode(request)? {
                Ok(view.clone())
            } else {
                Err(contract::changed())
            };
        }
        if request
            .expected_incarnation
            .as_deref()
            .is_some_and(|expected| expected != view.incarnation)
        {
            return Err(contract::changed());
        }
        if request.qualified && view.qualification == Qualification::Unknown {
            tx.execute("UPDATE hub_session_incarnations SET qualification='qualified' WHERE token=?1 AND session=?2 AND incarnation=?3",params![token,request.session,view.incarnation])?;
        }
        return current_view(tx, token, &request.session);
    }
    if current.as_ref().map(|view| view.incarnation.as_str())
        != request.expected_incarnation.as_deref()
    {
        return Err(contract::changed());
    }
    let reused:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM hub_session_incarnations WHERE token=?1 AND session=?2 AND (start_nonce=?3 OR register_operation=?4))",params![token,request.session,request.start_nonce,request.operation_id],|row|row.get(0))?;
    if reused {
        return Err(contract::changed());
    }
    let ordinal:i64=tx.query_row("SELECT COALESCE(MAX(ordinal),0)+1 FROM hub_session_incarnations WHERE token=?1 AND session=?2",params![token,request.session],|row|row.get(0))?;
    let incarnation = new_id("inc")?;
    let qualification = if request.qualified {
        "qualified"
    } else {
        "unknown"
    };
    tx.execute("INSERT INTO hub_session_incarnations(token,session,incarnation,start_nonce,ordinal,qualification,state,observed_at,register_operation,register_request) VALUES (?1,?2,?3,?4,?5,?6,'live',?7,?8,?9)",params![token,request.session,incarnation,request.start_nonce,ordinal,qualification,request.observed_at,request.operation_id,contract::encode(request)?])?;
    current_view(tx, token, &request.session)
}

fn current_view(tx: &Transaction<'_>, token: &str, session: &str) -> Result<LifecycleView> {
    current(tx, token, session)?.ok_or_else(contract::changed)
}

pub(super) fn check(
    tx: &Transaction<'_>,
    token: &str,
    session: &str,
    incarnation: Option<&str>,
    now: i64,
) -> Result<Option<LifecycleView>> {
    active_token(tx, token, now)?;
    check_name(session)?;
    if incarnation == Some(contract::EXPECT_NO_LIFECYCLE) {
        return if current_for_domain(tx, token, session)?.is_none() {
            Ok(None)
        } else {
            Err(contract::changed())
        };
    }
    let found = match incarnation {
        Some(id) => Some(current_for_transport(tx, token, session, id)?),
        None => current_for_domain(tx, token, session)?,
    };
    let Some(mut view) = found else {
        return if incarnation.is_none() {
            Ok(None)
        } else {
            Err(contract::changed())
        };
    };
    writer(tx, token, now)?;
    if view.state != LifecycleState::Live || incarnation.is_some_and(|id| id != view.incarnation) {
        return Err(contract::changed());
    }
    if incarnation.is_none() && view.qualification != Qualification::Mixed {
        tx.execute("UPDATE hub_session_incarnations SET qualification='mixed' WHERE token=?1 AND session=?2 AND incarnation=?3",params![token,view.session,view.incarnation])?;
        view.qualification = Qualification::Mixed;
    }
    Ok(Some(view))
}

/// Observation-only eligibility: no qualification downgrade, activity or lease renewal.
pub(super) fn registered_incarnation(
    tx: &Transaction<'_>,
    token: &str,
    session: &str,
    incarnation: &str,
) -> Result<()> {
    let view = current_for_transport(tx, token, session, incarnation)?;
    if view.state != LifecycleState::Live || view.incarnation != incarnation {
        return Err(contract::changed());
    }
    Ok(())
}

pub(super) fn end(
    tx: &Transaction<'_>,
    token: &str,
    request: &LifecycleEnd,
    now: i64,
) -> Result<LifecycleEndResult> {
    request.validate(now)?;
    let who = writer(tx, token, now)?;
    let encoded = contract::encode(request)?;
    let prior:Option<(Option<String>,Option<String>,Option<String>)>=tx.query_row("SELECT end_operation,end_request,end_result FROM hub_session_incarnations WHERE token=?1 AND session=?2 AND incarnation=?3",params![token,request.session,request.incarnation],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
    if let Some((Some(operation), Some(original), Some(result))) = prior {
        if operation == request.operation_id && original == encoded {
            return json_value(&result);
        }
        return Err(contract::changed());
    }
    let view = current_view(tx, token, &request.session)?;
    if view.incarnation != request.incarnation
        || view.state != LifecycleState::Live
        || request.observed_at < view.observed_at
        || (request.reason == EndReason::ObservedExit
            && view.qualification != Qualification::Qualified)
    {
        return Err(contract::changed());
    }
    let domain_session = session_name(tx, token, Some(&request.session), now)?;
    let mut statement=tx.prepare("SELECT snapshot FROM hub_work_items WHERE lead_token=?1 AND lead_session=?2 AND state<>'finished' ORDER BY id LIMIT 101")?;
    let items = statement
        .query_map(params![token, domain_session], |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| json_value::<WorkItem>(&row?))
        .collect::<Result<Vec<_>>>()?;
    if items.len() > contract::MAX_LIFECYCLE_WORK {
        return Err(HubError::Unavailable(
            "session lifecycle Work coverage is truncated".into(),
        ));
    }
    if items.len() != request.work.len()
        || items.iter().any(|item| {
            !request.work.iter().any(|fence| {
                fence.work_id == item.id
                    && fence.generation == item.generation
                    && fence.updated_seq == item.updated_seq
            })
        })
    {
        return Err(contract::changed());
    }
    let mut paused_work = Vec::new();
    let mut retained = Vec::new();
    for item in &items {
        if let Some(lease)=tx.query_row("SELECT reservation_renewed_at,reservation_expires_at,reservation_grace_at FROM hub_work_items WHERE id=?1 AND reservation_basis='retained'",[&item.id],|row|Ok((row.get::<_,Option<i64>>(0)?,row.get::<_,Option<i64>>(1)?,row.get::<_,Option<i64>>(2)?))).optional()? {retained.push((item.id.clone(),lease));}
    }
    for item in items {
        if item.state == WorkState::Active {
            // Preserve later recorded activity without promoting replay time to liveness.
            work_checkpoint_in(
                tx,
                token,
                &WorkCheckpoint::auto_pause(&item, "observed-exit"),
                request
                    .observed_at
                    .max(item.updated_at)
                    .max(item.last_checkin_at),
            )?;
            paused_work.push(item.id.clone());
        }
        tx.execute("UPDATE hub_work_items SET reservation_expires_at=MIN(reservation_expires_at,?3),reservation_grace_at=NULL WHERE id=?1 AND generation=?2 AND reservation_basis IS NOT NULL AND reservation_basis<>'retained'",params![item.id,item.generation,request.observed_at])?;
    }
    for (id, (renewed, expires, grace)) in retained {
        tx.execute("UPDATE hub_work_items SET reservation_renewed_at=?2,reservation_expires_at=?3,reservation_grace_at=?4 WHERE id=?1 AND reservation_basis='retained'",params![id,renewed,expires,grace])?;
    }
    let ended = end_session_in(tx, token, &domain_session, request.observed_at)?;
    let result = LifecycleEndResult {
        lifecycle: LifecycleView {
            state: LifecycleState::Terminal,
            observed_at: request.observed_at,
            ..view
        },
        ended,
        paused_work,
    };
    tx.execute("UPDATE hub_session_incarnations SET state='terminal',end_operation=?4,end_request=?5,end_result=?6,observed_at=?7 WHERE token=?1 AND session=?2 AND incarnation=?3",params![token,request.session,request.incarnation,request.operation_id,encoded,contract::encode(&result)?,request.observed_at])?;
    notices::withdraw_for_terminal(
        tx,
        token,
        &domain_session,
        &request.incarnation,
        request.observed_at,
    )?;
    let _ = who;
    Ok(result)
}

impl SqliteStore {
    pub(super) fn lifecycle_fences_impl(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Vec<contract::WorkFence>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        active_token(&tx, token, now)?;
        check_name(session)?;
        let domain_session = session_name(&tx, token, Some(session), now)?;
        let fences = {
            let mut statement=tx.prepare("SELECT id,generation,updated_seq FROM hub_work_items WHERE lead_token=?1 AND lead_session=?2 AND state<>'finished' ORDER BY id LIMIT 101")?;
            statement
                .query_map(params![token, domain_session], |row| {
                    Ok(contract::WorkFence {
                        work_id: row.get(0)?,
                        generation: row.get(1)?,
                        updated_seq: row.get(2)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        if fences.len() > contract::MAX_LIFECYCLE_WORK {
            return Err(HubError::Unavailable(
                "session lifecycle Work coverage is truncated".into(),
            ));
        }
        tx.commit()?;
        Ok(fences)
    }

    pub(super) fn lifecycle_register_impl(
        &self,
        token: &str,
        request: &LifecycleRegister,
        now: i64,
    ) -> Result<LifecycleView> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = register(&tx, token, request, now)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn lifecycle_check_impl(
        &self,
        token: &str,
        session: &str,
        incarnation: Option<&str>,
        now: i64,
    ) -> Result<Option<LifecycleView>> {
        // An unregistered native session has no qualification to downgrade. Its
        // hook must still invalidate readiness while coordination writes are busy.
        if incarnation.is_none()
            && self
                .lifecycle_domain_get_impl(token, session, now)?
                .is_none()
        {
            return Ok(None);
        }
        // Recheck registered sessions under the writer lock before changing qualification.
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = check(&tx, token, session, incarnation, now)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn clear_native_wait_with_transport_impl(
        &self,
        token: &str,
        session: &str,
        incarnation: Option<&str>,
        setting_key: &str,
        now: i64,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        writer(&tx, token, now)?;
        check(&tx, token, session, incarnation, now)?;
        tx.execute(
            &store_sql::sqlite(store_sql::SET_SETTING),
            params![setting_key, "1", now, "hub"],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn lifecycle_get_impl(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Option<LifecycleView>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let result = get(&tx, token, session, now)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn lifecycle_domain_get_impl(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Option<LifecycleView>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let result = domain_get(&tx, token, session, now)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn lifecycle_end_impl(
        &self,
        token: &str,
        request: &LifecycleEnd,
        now: i64,
    ) -> Result<LifecycleEndResult> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = end(&tx, token, request, now)?;
        tx.commit()?;
        Ok(result)
    }
}
