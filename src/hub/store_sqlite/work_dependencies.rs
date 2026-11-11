//! Transactional projections of existing prerequisite declarations and a latest-version mailbox.
use super::*;
use crate::hub::work_dependencies::*;
use crate::hub::work_reconciliation::{PrerequisiteCondition, PrerequisiteRequirement};

pub(super) fn projection(
    tx: &Transaction<'_>,
    who: &WorkWho,
    declared: &WorkDeclarationsView,
    admitted: bool,
    now: i64,
) -> Result<WorkDependencyProjection> {
    let evidence = reconciliation_evidence(tx, who, declared, now)?;
    let dispositions = declarations::prerequisite_dispositions(declared, &evidence);
    let conditions = declared
        .declarations
        .prerequisites
        .iter()
        .zip(dispositions)
        .map(|(condition, disposition)| WorkDependency {
            condition: condition.clone(),
            prerequisite_updated_seq: evidence
                .prerequisites
                .iter()
                .find(|v| v.work_id == condition.requirement.work_id())
                .and_then(|v| v.updated_seq),
            disposition,
        })
        .collect();
    Ok(WorkDependencyProjection {
        declaration_seq: admitted.then_some(declared.declaration_seq),
        conditions,
        evidence_digest: evidence.digest(),
        observed_at: now,
    })
}

pub(super) fn for_start(
    tx: &Transaction<'_>,
    who: &WorkWho,
    start: &WorkStart,
    existing: Option<&WorkItem>,
    now: i64,
) -> Result<Option<WorkDependencyProjection>> {
    let declared = if let Some(item) = existing {
        declaration_view(tx, item)?
    } else {
        WorkDeclarationsView {
            schema_version: 2,
            work_id: String::new(),
            generation: 0,
            updated_seq: 0,
            declaration_seq: 0,
            compatibility_link: None,
            declarations: WorkDeclarations {
                links: vec![],
                prerequisites: start
                    .depends_on
                    .iter()
                    .map(|id| PrerequisiteCondition {
                        id: format!("legacy-{id}"),
                        requirement: PrerequisiteRequirement::LegacyWork {
                            work_id: id.clone(),
                        },
                    })
                    .collect(),
            },
        }
    };
    if declared.declarations.prerequisites.is_empty() {
        return Ok(None);
    }
    Ok(Some(projection(
        tx,
        who,
        &declared,
        existing.is_some(),
        now,
    )?))
}

/// Reverse edges use durable declarations; legacy hints are only a fallback for missing rows.
pub(super) fn dependents(tx: &Transaction<'_>, id: &str) -> Result<Vec<String>> {
    let mut statement=tx.prepare("SELECT DISTINCT w.id FROM hub_work_items w LEFT JOIN hub_work_declarations d ON d.work=w.id WHERE w.state<>'finished' AND ((d.work IS NULL AND EXISTS(SELECT 1 FROM hub_work_relations r WHERE r.work=w.id AND r.related=?1 AND r.kind='depends_on')) OR EXISTS(SELECT 1 FROM json_each(d.snapshot,'$.prerequisites') p WHERE json_extract(p.value,'$.requirement.work_id')=?1)) ORDER BY w.id LIMIT 101")?;
    let ids = statement
        .query_map([id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(ids)
}

fn refresh(
    tx: &Transaction<'_>,
    who: &WorkWho,
    id: &str,
    now: i64,
    enforce: bool,
    fanout_complete: Option<bool>,
) -> Result<()> {
    let Some(item) = scoped_work(tx, who, id, now)? else {
        return Ok(());
    };
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM hub_work_declarations WHERE work=?1)",
        [id],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let declared = declaration_view(tx, &item)?;
    let previous: Option<(String, i64)> = tx
        .query_row(
            "SELECT snapshot,event_seq FROM hub_work_dependency_notices WHERE work=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let previous_record = previous
        .as_ref()
        .map(|(snapshot, _)| json_value::<DependencyNoticeRecord>(snapshot))
        .transpose()?;
    let fanout_complete = fanout_complete.unwrap_or_else(|| {
        previous_record
            .as_ref()
            .is_none_or(|record| record.notification_fanout_complete)
    });
    if declared.declarations.prerequisites.is_empty() && previous.is_none() && fanout_complete {
        return Ok(());
    }
    let mut prerequisites = Vec::new();
    let mut recipients = vec![id.to_owned()];
    let mut targets = declared.declarations.targets();
    targets.sort();
    targets.dedup();
    for target in targets {
        let other = scoped_work(tx, who, &target, now)?;
        prerequisites.push(DependencyVersion {
            work_id: target.clone(),
            updated_seq: other.as_ref().map(|v| v.updated_seq),
            generation: other.as_ref().map(|v| v.generation),
        });
        recipients.push(target);
    }
    recipients.sort();
    recipients.dedup();
    let evidence = reconciliation_evidence(tx, who, &declared, now)?;
    let record = DependencyNoticeRecord {
        work_id: id.into(),
        generation: item.generation,
        declaration_seq: declared.declaration_seq,
        evidence_digest: evidence.digest(),
        conditions: declarations::prerequisite_dispositions(&declared, &evidence),
        notification_fanout_complete: fanout_complete,
        prerequisites,
        recipients,
    };
    let snapshot = json_string(&record)?;
    if snapshot.len() > MAX_DEPENDENCY_NOTICE_BYTES {
        return Err(HubError::Invalid(
            "dependency notice exceeds bounded capacity".into(),
        ));
    }
    if previous.as_ref().is_some_and(|(old, _)| old == &snapshot) {
        return Ok(());
    }
    let mut event = Event::new(
        "work_pr_link",
        id,
        json!({"operation":"dependency_notice","generation":item.generation,"owner":item.owner,"repository":item.repository,"declaration_seq":declared.declaration_seq,"prerequisite_versions":record.prerequisites}),
    )?;
    event.at = now;
    let seq = append(tx, &event)?;
    tx.execute("INSERT INTO hub_work_dependency_notices(work,event_seq,snapshot) VALUES (?1,?2,?3) ON CONFLICT(work) DO UPDATE SET event_seq=excluded.event_seq,snapshot=excluded.snapshot",params![id,seq,snapshot])?;
    let delta = work_payload_row_bytes(snapshot.len())?
        - previous
            .as_ref()
            .map(|(v, _)| work_payload_row_bytes(v.len()))
            .transpose()?
            .unwrap_or(0)
        + work_event_payload_bytes(&event)?;
    // These are automatic audit facts of an already-authorized transition, including release.
    charge_work_payload(tx, who, delta, now, enforce)?;
    Ok(())
}

/// A legacy checkpoint may add an unbound edge, but omission never erases durable conditions.
pub(super) fn add_legacy(
    tx: &Transaction<'_>,
    who: &WorkWho,
    item: &WorkItem,
    added: &[String],
    seq: i64,
    now: i64,
) -> Result<()> {
    if added.is_empty() {
        return Ok(());
    }
    let view = declaration_view(tx, item)?;
    let old = json_string(&view.declarations)?;
    let mut next = view.declarations;
    for id in added {
        if !next
            .prerequisites
            .iter()
            .any(|p| p.requirement.work_id() == id)
        {
            next.prerequisites.push(PrerequisiteCondition {
                id: format!("legacy-{id}"),
                requirement: PrerequisiteRequirement::LegacyWork {
                    work_id: id.clone(),
                },
            });
        }
    }
    if next.prerequisites.len() > 100 {
        return Err(HubError::Invalid(
            "too many durable prerequisite conditions".into(),
        ));
    }
    next.prerequisites.sort_by(|a, b| a.id.cmp(&b.id));
    let snapshot = json_string(&next)?;
    if snapshot != old {
        tx.execute(
            "UPDATE hub_work_declarations SET snapshot=?2,event_seq=?3 WHERE work=?1",
            params![item.id, snapshot, seq],
        )?;
        record_work_payload_delta(
            tx,
            who,
            work_payload_row_bytes(snapshot.len())? - work_payload_row_bytes(old.len())?,
            now,
        )?;
    }
    Ok(())
}

pub(super) fn changed(
    tx: &Transaction<'_>,
    who: &WorkWho,
    item: &WorkItem,
    now: i64,
) -> Result<()> {
    let enforce = matches!(item.state, WorkState::Active | WorkState::Blocked);
    let dependents = dependents(tx, &item.id)?;
    refresh(
        tx,
        who,
        &item.id,
        now,
        enforce,
        Some(dependents.len() <= 100),
    )?;
    for dependent in dependents.into_iter().take(100) {
        refresh(tx, who, &dependent, now, enforce, None)?;
    }
    Ok(())
}

impl SqliteStore {
    pub(super) fn dependency_notices(
        &self,
        token: &str,
        query: &WorkDependencyNoticeQuery,
        now: i64,
    ) -> Result<WorkDependencyNoticePage> {
        query.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let page = notice_page(&tx, &who, query, false, now)?;
        tx.commit()?;
        Ok(page)
    }
    pub(super) fn dependency_owner_notices(
        &self,
        reader: &WorkLifecycleReader<'_>,
        now: i64,
    ) -> Result<WorkDependencyNoticePage> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = match reader {
            WorkLifecycleReader::Token(token) => work_who(&tx, token, now)?,
            WorkLifecycleReader::Person {
                owner,
                repository,
                repository_id,
            } => {
                if owner.disabled {
                    return Err(HubError::Unauthorized);
                }
                work_transition::person_who(owner, repository, *repository_id)
            }
        };
        let query = WorkDependencyNoticeQuery {
            session: String::new(),
            after: 0,
            through: None,
            limit: 100,
        };
        let page = notice_page(&tx, &who, &query, true, now)?;
        tx.commit()?;
        Ok(page)
    }
    pub(super) fn dependency_notice_delivery(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Option<WorkDependencyNoticePage>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let after: Option<i64> = tx
            .query_row(
                "SELECT dependency_notice_cursor FROM hub_sessions WHERE token=?1 AND session=?2",
                params![token, session],
                |r| r.get(0),
            )
            .optional()?;
        let Some(after) = after else {
            return Ok(None);
        };
        let query = WorkDependencyNoticeQuery {
            session: session.into(),
            after,
            through: None,
            limit: 3,
        };
        let page = notice_page(&tx, &who, &query, false, now)?;
        if page.next_after <= after {
            return Ok(None);
        }
        tx.execute(
            "UPDATE hub_sessions SET dependency_notice_cursor=?3 WHERE token=?1 AND session=?2",
            params![token, session, page.next_after],
        )?;
        tx.commit()?;
        Ok((!page.notices.is_empty() || !page.coverage_complete).then_some(page))
    }
}

fn notice_page(
    tx: &Transaction<'_>,
    who: &WorkWho,
    query: &WorkDependencyNoticeQuery,
    owner_queue: bool,
    now: i64,
) -> Result<WorkDependencyNoticePage> {
    let through = query.through.unwrap_or(tx.query_row(
            "SELECT COALESCE(MAX(n.event_seq),0) FROM hub_work_dependency_notices n JOIN hub_work_items w ON w.id=n.work WHERE w.provider=?1 AND w.host=?2 AND w.repository=?3 AND w.repository_id IS ?4",
            params![who.owner.identity.provider,who.owner.identity.host,who.repository,who.repository_id],
            |r| r.get(0),
        )?);
    // Select only notices addressed to a Work this exact session currently leads. No source
    // relation, helper role or old lead credential grants mailbox access.
    let rows=tx.prepare("SELECT n.event_seq,n.snapshot,w.snapshot FROM hub_work_dependency_notices n JOIN hub_work_items w ON w.id=n.work WHERE n.event_seq>?1 AND n.event_seq<=?2 AND w.provider=?3 AND w.host=?4 AND w.repository=?5 AND w.repository_id IS ?6 AND (NOT ?10 OR w.state<>'finished') AND EXISTS(SELECT 1 FROM json_each(n.snapshot,'$.recipients') r JOIN hub_work_items recipient ON recipient.id=r.value WHERE ((?10 AND recipient.owner=?11 AND recipient.id<>n.work) OR (NOT ?10 AND recipient.lead_token=?7 AND recipient.lead_session=?8))) ORDER BY n.event_seq LIMIT ?9")?.query_map(params![query.after,through,who.owner.identity.provider,who.owner.identity.host,who.repository,who.repository_id,who.token,query.session,(query.limit+1) as i64,owner_queue,who.owner.id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let coverage_complete: bool = !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM hub_work_dependency_notices n JOIN hub_work_items w ON w.id=n.work WHERE w.provider=?1 AND w.host=?2 AND w.repository=?3 AND w.repository_id IS ?4 AND json_extract(n.snapshot,'$.notification_fanout_complete')=0)",
            params![who.owner.identity.provider,who.owner.identity.host,who.repository,who.repository_id], |r|r.get::<_,bool>(0))?;
    let mut complete = rows.len() <= query.limit;
    let mut bytes = 0;
    let mut notices = Vec::new();
    for (sequence, record, snapshot) in rows.into_iter().take(query.limit) {
        let item: WorkItem = json_value(&snapshot)?;
        let declared = declaration_view(tx, &item)?;
        let notice = WorkDependencyNotice {
            sequence,
            recorded: json_value(&record)?,
            current: projection(tx, who, &declared, true, now)?,
        };
        let size = json_string(&notice)?.len();
        if bytes + size > MAX_DEPENDENCY_PAGE_BYTES {
            complete = false;
            break;
        }
        bytes += size;
        notices.push(notice);
    }
    let next_after = if complete {
        through
    } else {
        notices.last().map_or(query.after, |n| n.sequence)
    };
    Ok(WorkDependencyNoticePage {
        through,
        next_after,
        complete,
        notices,
        latest_only: true,
        atomic_snapshot: false,
        coverage_complete,
    })
}
