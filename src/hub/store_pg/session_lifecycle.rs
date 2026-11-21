//! Incarnation guards run inside the domain mutation's transaction.
use super::*;
use crate::hub::session_lifecycle::{
    self as contract, EndReason, LifecycleEnd, LifecycleEndResult, LifecycleRegister,
    LifecycleState, LifecycleView, Qualification,
};

fn writer(tx: &mut postgres::Transaction<'_>, token: &str, now: i64) -> Result<WorkWho> {
    let who = work_who(tx, token, now)?;
    if who.read_only {
        return Err(HubError::Unauthorized);
    }
    Ok(who)
}

fn lock(tx: &mut postgres::Transaction<'_>, token: &str, session: &str) -> Result<()> {
    let canonical = session_name(tx, token, Some(session), 0)?;
    let key = contract::encode(&(token, canonical))?;
    tx.query_one(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        &[&key],
    )?;
    Ok(())
}

fn decode(session: &str, row: postgres::Row) -> Result<LifecycleView> {
    let state: String = row.get(2);
    let qualification: String = row.get(3);
    Ok(LifecycleView {
        session: session.into(),
        incarnation: row.get(0),
        start_nonce: row.get(1),
        observed_at: row.get(4),
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

fn current(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
    locked: bool,
) -> Result<Option<LifecycleView>> {
    let query = if locked {
        "SELECT incarnation,start_nonce,state,qualification,observed_at FROM hub_session_incarnations WHERE token=$1 AND session=$2 ORDER BY ordinal DESC LIMIT 1 FOR UPDATE"
    } else {
        "SELECT incarnation,start_nonce,state,qualification,observed_at FROM hub_session_incarnations WHERE token=$1 AND session=$2 ORDER BY ordinal DESC LIMIT 1"
    };
    tx.query_opt(query, &[&token, &session])?
        .map(|row| decode(session, row))
        .transpose()
}

pub(super) fn current_for_domain(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
) -> Result<Option<LifecycleView>> {
    let canonical = session_name(tx, token, Some(session), 0)?;
    let rows=tx.query("SELECT incarnation,start_nonce,state,qualification,observed_at,session FROM hub_session_incarnations i WHERE token=$1 AND (session=$2 OR session=$3 OR EXISTS(SELECT 1 FROM hub_session_aliases a WHERE a.token=i.token AND a.observed_session=i.session AND a.canonical_session=$3)) AND ordinal=(SELECT MAX(j.ordinal) FROM hub_session_incarnations j WHERE j.token=i.token AND j.session=i.session) ORDER BY session LIMIT 2",&[&token,&session,&canonical])?;
    if rows.len() > 1 {
        return Err(contract::changed());
    }
    rows.into_iter()
        .next()
        .map(|row| {
            let session: String = row.get(5);
            decode(&session, row)
        })
        .transpose()
}

fn current_for_transport(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
    incarnation: &str,
) -> Result<LifecycleView> {
    let names=tx.query("SELECT session FROM hub_session_incarnations WHERE token=$1 AND incarnation=$2 LIMIT 2",&[&token,&incarnation])?.into_iter().map(|row|row.get::<_,String>(0)).collect::<Vec<_>>();
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
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
    now: i64,
) -> Result<Option<LifecycleView>> {
    active_token(tx, token, now)?;
    check_name(session)?;
    current(tx, token, session, false)
}

fn domain_get(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
    now: i64,
) -> Result<Option<LifecycleView>> {
    active_token(tx, token, now)?;
    check_name(session)?;
    current_for_domain(tx, token, session)
}

pub(super) fn register(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    request: &LifecycleRegister,
    now: i64,
) -> Result<LifecycleView> {
    request.validate(now)?;
    writer(tx, token, now)?;
    lock(tx, token, &request.session)?;
    let current = current(tx, token, &request.session, true)?;
    if let Some(view) = current
        .as_ref()
        .filter(|view| view.start_nonce == request.start_nonce)
    {
        let original=tx.query_one("SELECT register_operation,register_request FROM hub_session_incarnations WHERE token=$1 AND session=$2 AND incarnation=$3",&[&token,&request.session,&view.incarnation])?;
        let operation: String = original.get(0);
        if operation == request.operation_id {
            return if view.state == LifecycleState::Live
                && original.get::<_, String>(1) == contract::encode(request)?
            {
                Ok(view.clone())
            } else {
                Err(contract::changed())
            };
        }
        if view.state != LifecycleState::Live
            || request
                .expected_incarnation
                .as_deref()
                .is_some_and(|expected| expected != view.incarnation)
        {
            return Err(contract::changed());
        }
        if request.qualified && view.qualification == Qualification::Unknown {
            tx.execute("UPDATE hub_session_incarnations SET qualification='qualified' WHERE token=$1 AND session=$2 AND incarnation=$3",&[&token,&request.session,&view.incarnation])?;
        }
        return current_view(tx, token, &request.session);
    }
    if current.as_ref().map(|view| view.incarnation.as_str())
        != request.expected_incarnation.as_deref()
    {
        return Err(contract::changed());
    }
    let reused:bool=tx.query_one("SELECT EXISTS(SELECT 1 FROM hub_session_incarnations WHERE token=$1 AND session=$2 AND (start_nonce=$3 OR register_operation=$4))",&[&token,&request.session,&request.start_nonce,&request.operation_id])?.get(0);
    if reused {
        return Err(contract::changed());
    }
    let ordinal:i64=tx.query_one("SELECT COALESCE(MAX(ordinal),0)+1 FROM hub_session_incarnations WHERE token=$1 AND session=$2",&[&token,&request.session])?.get(0);
    let incarnation = new_id("inc")?;
    let qualification = if request.qualified {
        "qualified"
    } else {
        "unknown"
    };
    tx.execute("INSERT INTO hub_session_incarnations(token,session,incarnation,start_nonce,ordinal,qualification,state,observed_at,register_operation,register_request) VALUES ($1,$2,$3,$4,$5,$6,'live',$7,$8,$9)",&[&token,&request.session,&incarnation,&request.start_nonce,&ordinal,&qualification,&request.observed_at,&request.operation_id,&contract::encode(request)?])?;
    current_view(tx, token, &request.session)
}

fn current_view(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
) -> Result<LifecycleView> {
    current(tx, token, session, true)?.ok_or_else(contract::changed)
}

pub(super) fn check(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
    incarnation: Option<&str>,
    now: i64,
) -> Result<Option<LifecycleView>> {
    active_token(tx, token, now)?;
    check_name(session)?;
    lock(tx, token, session)?;
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
    let Some(found) = found else {
        return if incarnation.is_none() {
            Ok(None)
        } else {
            Err(contract::changed())
        };
    };
    let mut view = current_view(tx, token, &found.session)?;
    writer(tx, token, now)?;
    if view.state != LifecycleState::Live || incarnation.is_some_and(|id| id != view.incarnation) {
        return Err(contract::changed());
    }
    if incarnation.is_none() && view.qualification != Qualification::Mixed {
        tx.execute("UPDATE hub_session_incarnations SET qualification='mixed' WHERE token=$1 AND session=$2 AND incarnation=$3",&[&token,&view.session,&view.incarnation])?;
        view.qualification = Qualification::Mixed;
    }
    Ok(Some(view))
}

/// Observation-only eligibility: no qualification downgrade, activity or lease renewal.
pub(super) fn registered_incarnation(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
    incarnation: &str,
) -> Result<()> {
    lock(tx, token, session)?;
    let view = current_for_transport(tx, token, session, incarnation)?;
    if view.state != LifecycleState::Live || view.incarnation != incarnation {
        return Err(contract::changed());
    }
    Ok(())
}

pub(super) fn end(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    request: &LifecycleEnd,
    now: i64,
) -> Result<LifecycleEndResult> {
    request.validate(now)?;
    let who = writer(tx, token, now)?;
    lock(tx, token, &request.session)?;
    let encoded = contract::encode(request)?;
    if let Some(row)=tx.query_opt("SELECT end_operation,end_request,end_result FROM hub_session_incarnations WHERE token=$1 AND session=$2 AND incarnation=$3 FOR UPDATE",&[&token,&request.session,&request.incarnation])? {
        let values:(Option<String>,Option<String>,Option<String>)=(row.get(0),row.get(1),row.get(2));
        if let (Some(operation),Some(original),Some(result))=values {
            if operation == request.operation_id && original == encoded {return json_value(&result);}
            return Err(contract::changed());
        }
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
    lock_work_capacity(tx, &who)?;
    let domain_session = session_name(tx, token, Some(&request.session), now)?;
    let rows=tx.query("SELECT snapshot FROM hub_work_items WHERE lead_token=$1 AND lead_session=$2 AND state<>'finished' ORDER BY id COLLATE \"C\" LIMIT 101 FOR UPDATE",&[&token,&domain_session])?;
    let items = rows
        .iter()
        .map(|row| json_value::<WorkItem>(&row.get::<_, String>(0)))
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
        if let Some(row)=tx.query_opt("SELECT reservation_renewed_at,reservation_expires_at,reservation_grace_at FROM hub_work_items WHERE id=$1 AND reservation_basis='retained'",&[&item.id])? {retained.push((item.id.clone(),row.get::<_,Option<i64>>(0),row.get::<_,Option<i64>>(1),row.get::<_,Option<i64>>(2)));}
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
        tx.execute("UPDATE hub_work_items SET reservation_expires_at=LEAST(reservation_expires_at,$3),reservation_grace_at=NULL WHERE id=$1 AND generation=$2 AND reservation_basis IS NOT NULL AND reservation_basis<>'retained'",&[&item.id,&item.generation,&request.observed_at])?;
    }
    for (id, renewed, expires, grace) in retained {
        tx.execute("UPDATE hub_work_items SET reservation_renewed_at=$2,reservation_expires_at=$3,reservation_grace_at=$4 WHERE id=$1 AND reservation_basis='retained'",&[&id,&renewed,&expires,&grace])?;
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
    tx.execute("UPDATE hub_session_incarnations SET state='terminal',end_operation=$4,end_request=$5,end_result=$6,observed_at=$7 WHERE token=$1 AND session=$2 AND incarnation=$3",&[&token,&request.session,&request.incarnation,&request.operation_id,&encoded,&contract::encode(&result)?,&request.observed_at])?;
    notices::withdraw_for_terminal(
        tx,
        token,
        &domain_session,
        &request.incarnation,
        request.observed_at,
    )?;
    Ok(result)
}

impl PgStore {
    pub(super) fn lifecycle_fences_impl(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Vec<contract::WorkFence>> {
        self.run(true,|client|{
            let mut tx=client.transaction()?;
            active_token(&mut tx,token,now)?;
            check_name(session)?;
            let domain_session=session_name(&mut tx,token,Some(session),now)?;
            let fences=tx.query("SELECT id,generation,updated_seq FROM hub_work_items WHERE lead_token=$1 AND lead_session=$2 AND state<>'finished' ORDER BY id COLLATE \"C\" LIMIT 101",&[&token,&domain_session])?.iter().map(|row|contract::WorkFence {work_id:row.get(0),generation:row.get(1),updated_seq:row.get(2)}).collect::<Vec<_>>();
            if fences.len()>contract::MAX_LIFECYCLE_WORK {return Err(HubError::Unavailable("session lifecycle Work coverage is truncated".into()));}
            tx.commit()?;
            Ok(fences)
        })
    }

    pub(super) fn lifecycle_register_impl(
        &self,
        token: &str,
        request: &LifecycleRegister,
        now: i64,
    ) -> Result<LifecycleView> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let result = register(&mut tx, token, request, now)?;
            tx.commit()?;
            Ok(result)
        })
    }
    pub(super) fn lifecycle_get_impl(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Option<LifecycleView>> {
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let result = get(&mut tx, token, session, now)?;
            tx.commit()?;
            Ok(result)
        })
    }
    pub(super) fn lifecycle_domain_get_impl(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Option<LifecycleView>> {
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let result = domain_get(&mut tx, token, session, now)?;
            tx.commit()?;
            Ok(result)
        })
    }

    pub(super) fn lifecycle_check_impl(
        &self,
        token: &str,
        session: &str,
        incarnation: Option<&str>,
        now: i64,
    ) -> Result<Option<LifecycleView>> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let result = check(&mut tx, token, session, incarnation, now)?;
            tx.commit()?;
            Ok(result)
        })
    }
    pub(super) fn lifecycle_end_impl(
        &self,
        token: &str,
        request: &LifecycleEnd,
        now: i64,
    ) -> Result<LifecycleEndResult> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let result = end(&mut tx, token, request, now)?;
            tx.commit()?;
            Ok(result)
        })
    }

    pub(super) fn clear_native_wait_with_transport_impl(
        &self,
        token: &str,
        session: &str,
        incarnation: Option<&str>,
        setting_key: &str,
        now: i64,
    ) -> Result<()> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            writer(&mut tx, token, now)?;
            check(&mut tx, token, session, incarnation, now)?;
            tx.execute(store_sql::SET_SETTING, &[&setting_key, &"1", &now, &"hub"])?;
            tx.commit()?;
            Ok(())
        })
    }
}
