//! Intent facts collected without mutating claims, notice versions or delivery cursors.
use super::*;
use crate::hub::work_intent::{self as contract, IntentFacts, WorkIntentV1};

pub(super) fn read(
    store: &SqliteStore,
    token: &str,
    id: &str,
    now: i64,
) -> Result<Option<WorkIntentV1>> {
    crate::hub::work::validate_id("work", id, "wrk")?;
    let mut conn = store.conn();
    let tx = conn.transaction()?;
    let who = work_who(&tx, token, now)?;
    let Some(item) = scoped_work(&tx, &who, id, now)? else {
        return Ok(None);
    };
    let decisions = tx.prepare("SELECT event_seq,checkpoint FROM hub_work_decisions WHERE work=?1 ORDER BY event_seq DESC LIMIT ?2")?
        .query_map(params![id,(contract::MAX_DECISIONS+1) as i64], |row| Ok((row.get::<_,i64>(0)?, row.get::<_,String>(1)?)))?
        .map(|row| { let (seq, checkpoint) = row?; Ok((seq,json_value(&checkpoint)?)) })
        .collect::<Result<Vec<_>>>()?;
    let declared = declaration_view(&tx, &item)?;
    let dependencies = work_dependencies::projection(&tx, &who, &declared, true, now)?;
    let issue_snapshot: Option<String> = tx.query_row(
        "SELECT o.data FROM hub_repositories r JOIN hub_connections c ON c.id=r.connection
         JOIN hub_observations o ON o.repository=r.id AND o.kind='planned_issues' AND o.key='_snapshot' AND o.deleted=0
         WHERE c.provider=?1 AND r.host=?2 AND r.repository_id IS ?3 ORDER BY r.id LIMIT 1",
        params![who.owner.identity.provider,who.owner.identity.host,who.repository_id], |row| row.get(0)).optional()?;
    let result = contract::project(IntentFacts {
        item: &item,
        who: &who,
        decisions,
        declared,
        dependencies,
        issue_snapshot: issue_snapshot.as_deref(),
        now,
    })?;
    tx.commit()?;
    Ok(Some(result))
}

/// The caller authorizes the repository; the holder's token is only an exact lookup key.
pub(super) fn holder(
    store: &SqliteStore,
    token: &str,
    holder_token: &str,
    holder_session: &str,
    now: i64,
) -> Result<Option<String>> {
    crate::hub::intent::text("holder token", holder_token, 255)?;
    crate::hub::intent::text("holder session", holder_session, 255)?;

    let mut conn = store.conn();
    let tx = conn.transaction()?;
    let who = work_who(&tx, token, now)?;
    let ids = tx.prepare("SELECT id FROM hub_work_items WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_id IS ?4 AND lead_token=?5 AND lead_session=?6 AND state IN ('active','blocked') ORDER BY id LIMIT 2")?
        .query_map(params![who.owner.identity.provider,who.owner.identity.host,who.repository,who.repository_id,holder_token,holder_session], |r| r.get::<_,String>(0))?
        .collect::<std::result::Result<Vec<_>,_>>()?;
    tx.commit()?;
    Ok((ids.len() == 1).then(|| ids[0].clone()))
}
