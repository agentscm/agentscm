//! Same read-only intent facts and snapshot isolation as SQLite.
use super::*;
use crate::hub::work_intent::{self as contract, IntentFacts, WorkIntentV1};

pub(super) fn read(
    store: &PgStore,
    token: &str,
    id: &str,
    now: i64,
) -> Result<Option<WorkIntentV1>> {
    crate::hub::work::validate_id("work", id, "wrk")?;
    store.run(true, |client| {
        let mut tx = client.build_transaction().isolation_level(IsolationLevel::RepeatableRead).start()?;
        let who = work_who(&mut tx, token, now)?;
        let Some(item) = scoped_work(&mut tx, &who, id, now, false)? else { return Ok(None); };
        let decisions = tx.query("SELECT event_seq,checkpoint FROM hub_work_decisions WHERE work=$1 ORDER BY event_seq DESC LIMIT $2", &[&id,&((contract::MAX_DECISIONS+1) as i64)])?
            .into_iter().map(|row| Ok((row.get(0),json_value(row.get::<_,&str>(1))?))).collect::<Result<Vec<_>>>()?;
        let declared = declaration_view(&mut tx, &item)?;
        let dependencies = work_dependencies::projection(&mut tx, &who, &declared, true, now)?;
        let issue_snapshot: Option<String> = tx.query_opt(
            "SELECT o.data FROM hub_repositories r JOIN hub_connections c ON c.id=r.connection
             JOIN hub_observations o ON o.repository=r.id AND o.kind='planned_issues' AND o.key='_snapshot' AND o.deleted=FALSE
             WHERE c.provider=$1 AND r.host=$2 AND r.repository_id IS NOT DISTINCT FROM $3 ORDER BY r.id LIMIT 1",
            &[&who.owner.identity.provider,&who.owner.identity.host,&who.repository_id])?.map(|row| row.get(0));
        let result = contract::project(IntentFacts { item: &item, who: &who, decisions, declared,
            dependencies, issue_snapshot: issue_snapshot.as_deref(), now })?;
        tx.commit()?;
        Ok(Some(result))
    })
}

pub(super) fn holder(
    store: &PgStore,
    token: &str,
    holder_token: &str,
    holder_session: &str,
    now: i64,
) -> Result<Option<String>> {
    crate::hub::intent::text("holder token", holder_token, 255)?;
    crate::hub::intent::text("holder session", holder_session, 255)?;

    store.run(true, |client| {
        let mut tx = client.transaction()?;
        let who = work_who(&mut tx, token, now)?;
        let rows = tx.query("SELECT id FROM hub_work_items WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_id IS NOT DISTINCT FROM $4 AND lead_token=$5 AND lead_session=$6 AND state IN ('active','blocked') ORDER BY id LIMIT 2", &[&who.owner.identity.provider,&who.owner.identity.host,&who.repository,&who.repository_id,&holder_token,&holder_session])?;
        let id = (rows.len() == 1).then(|| rows[0].get(0));
        tx.commit()?;
        Ok(id)
    })
}
