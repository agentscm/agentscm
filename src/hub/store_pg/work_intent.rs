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
