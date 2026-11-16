//! One transactional delivery ledger shared by source domains.
use super::*;
use crate::hub::intent::text;
use crate::hub::notices::*;
use crate::hub::work_dependencies::DependencyNoticeRecord;

fn kind(source: &NoticeSource) -> Result<String> {
    json_string(&source.kind)
}

fn save(tx: &Transaction<'_>, receipt: &NoticeReceipt) -> Result<()> {
    tx.execute(
        "UPDATE hub_notice_deliveries SET snapshot=?2 WHERE id=?1",
        params![receipt.id, json_string(receipt)?],
    )?;
    Ok(())
}

/// Call only after the source domain admitted the relationship in this transaction.
pub(super) fn record(
    tx: &Transaction<'_>,
    who: &WorkWho,
    source: NoticeSource,
    recipient: NoticeRecipient,
    now: i64,
) -> Result<NoticeReceipt> {
    text("source id", &source.id, 255)?;
    text("source version", &source.version, 255)?;
    text("recipient session", &recipient.session, 255)?;
    let recipient_who = work_who(tx, &recipient.token, now)?;
    if recipient_who.read_only
        || recipient_who.repository_id != who.repository_id
        || recipient_who.owner.identity.provider != who.owner.identity.provider
        || recipient_who.owner.identity.host != who.owner.identity.host
        || recipient_who.repository != who.repository
    {
        return Err(HubError::Unauthorized);
    }
    let mut receipt = NoticeReceipt::new(&who.repository, source, recipient, now);
    if let Some(snapshot) = tx
        .query_row(
            "SELECT snapshot FROM hub_notice_deliveries WHERE id=?1",
            [&receipt.id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return json_value(&snapshot);
    }
    let source_kind = kind(&receipt.source)?;
    // Rotating a credential cannot create another potentially-enqueued automatic turn.
    let prior = tx
        .prepare(
            "SELECT snapshot FROM hub_notice_deliveries WHERE owner=?1 AND provider=?2 AND host=?3
         AND (repository=?4 OR repository_id IS NOT NULL) AND repository_id IS ?5 AND source_kind=?6 AND source_id=?7
         AND source_version=?8 AND session=?9 AND token<>?10",
        )?
        .query_map(
            params![
                recipient_who.owner.id,
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                source_kind,
                receipt.source.id,
                receipt.source.version,
                receipt.recipient.session,
                receipt.recipient.token
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for snapshot in prior {
        let previous: NoticeReceipt = json_value(&snapshot)?;
        if previous
            .attempts
            .iter()
            .any(|attempt| attempt.outcome != AttemptOutcome::ProvenNoDispatch)
        {
            receipt.attempts = previous.attempts;
            receipt.deadline_at = previous.deadline_at;
            break;
        }
    }
    // Retain source-version attempt/ack history. Lifetime audit rows are not an admission
    // quota: current processing is bounded by the pending readers below.
    tx.execute(
        "INSERT INTO hub_notice_deliveries(id,owner,provider,host,repository,repository_id,token,
         session,source_kind,source_id,source_version,snapshot) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![receipt.id, recipient_who.owner.id, who.owner.identity.provider, who.owner.identity.host,
            who.repository, who.repository_id, receipt.recipient.token, receipt.recipient.session,
            source_kind, receipt.source.id, receipt.source.version, json_string(&receipt)?],
    )?;
    Ok(receipt)
}

pub(super) fn resolve_source(
    tx: &Transaction<'_>,
    source: &NoticeSource,
    token: &str,
    session: &str,
    now: i64,
) -> Result<bool> {
    let snapshots = tx
        .prepare(
            "SELECT snapshot FROM hub_notice_deliveries WHERE source_kind=?1 AND source_id=?2
         AND source_version=?3 AND token=?4 AND session=?5",
        )?
        .query_map(
            params![kind(source)?, source.id, source.version, token, session],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut newly = false;
    for snapshot in snapshots {
        let mut receipt: NoticeReceipt = json_value(&snapshot)?;
        newly |= receipt.read_at.is_none();
        receipt.read_at.get_or_insert(now);
        receipt.resolved_at.get_or_insert(now);
        save(tx, &receipt)?;
    }
    Ok(newly)
}

/// Terminal observation revokes idle transport, retaining pointers and attempt history.
pub(super) fn withdraw_for_terminal(
    tx: &Transaction<'_>,
    token: &str,
    session: &str,
    _incarnation: &str,
    now: i64,
) -> Result<()> {
    tx.execute("UPDATE hub_notice_deliveries SET snapshot=json_set(snapshot,'$.terminal_at',?3) WHERE token=?1 AND session=?2 AND json_extract(snapshot,'$.terminal_at') IS NULL AND json_extract(snapshot,'$.read_at') IS NULL AND json_extract(snapshot,'$.resolved_at') IS NULL AND COALESCE(json_extract(snapshot,'$.withdrawn'),0)=0", params![token,session,now])?;
    let who = work_who(tx, token, now)?;
    let snapshots=tx.prepare("SELECT snapshot FROM hub_work_items WHERE lead_token=?1 AND lead_session=?2 AND state<>'finished' ORDER BY id LIMIT 101")?
        .query_map(params![token,session],|row|row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
    if snapshots.len() > 100 {
        return Err(HubError::Unavailable(
            "Terminal Work notice coverage exceeds its bound.".into(),
        ));
    }
    for snapshot in snapshots {
        let item: WorkItem = json_value(&snapshot)?;
        let mut receipt = record(
            tx,
            &who,
            NoticeSource {
                kind: SourceKind::Operational,
                id: item.id.clone(),
                version: format!("ended:{_incarnation}"),
            },
            NoticeRecipient {
                token: token.into(),
                session: session.into(),
                work_id: Some(item.id),
                generation: Some(item.generation),
            },
            now,
        )?;
        receipt.terminal_at.get_or_insert(now);
        save(tx, &receipt)?;
    }
    Ok(())
}

fn load(tx: &Transaction<'_>, who: &WorkWho, id: &str, session: &str) -> Result<NoticeReceipt> {
    let snapshot = tx
        .query_row(
            "SELECT snapshot FROM hub_notice_deliveries WHERE id=?1 AND token=?2 AND session=?3
         AND owner=?4 AND provider=?5 AND host=?6 AND repository=?7 AND repository_id IS ?8",
            params![
                id,
                who.token,
                session,
                who.owner.id,
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            HubError::invalid_or_changed("Notice is not addressed to this session.".into())
        })?;
    json_value(&snapshot)
}

fn dependency_version(record: &DependencyNoticeRecord) -> String {
    format!(
        "{}:{}:{}",
        record.generation, record.declaration_seq, record.semantic_version
    )
}

fn message_mode(tx: &Transaction<'_>, who: &WorkWho) -> Result<crate::hub::agent_messages::Mode> {
    let mut mode = crate::hub::agent_messages::Mode::Manual;
    for key in [
        format!("agent_message_mode:{}", who.owner.id),
        format!(
            "agent_message_mode:{}:{}",
            who.owner.id,
            who.repository_id.unwrap_or_default()
        ),
    ] {
        if let Some(value) = tx
            .query_row(
                "SELECT value FROM hub_settings WHERE key=?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            && let Ok(current) = serde_json::from_str(&value)
        {
            mode = current;
        }
    }
    Ok(mode)
}

fn current(tx: &Transaction<'_>, who: &WorkWho, receipt: &NoticeReceipt, now: i64) -> Result<bool> {
    if let Some(id) = &receipt.recipient.work_id {
        let Some(item) = scoped_work(tx, who, id, now)? else {
            return Ok(false);
        };
        if Some(item.generation) != receipt.recipient.generation
            || item.lead_token != receipt.recipient.token
            || item.lead_session != receipt.recipient.session
            || !(matches!(item.state, WorkState::Active | WorkState::Blocked)
                || (item.state == WorkState::Paused && receipt.terminal_at.is_some()))
        {
            return Ok(false);
        }
    }
    if receipt.source.kind == SourceKind::HumanMessage {
        if message_mode(tx, who)? == crate::hub::agent_messages::Mode::Ignore {
            return Ok(false);
        }
        let Some(repository_id) = who.repository_id else {
            return Ok(false);
        };
        let key = format!("agent_messages:{}:{repository_id}", who.owner.id);
        let value = tx
            .query_row(
                "SELECT value FROM hub_settings WHERE key=?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let messages: Vec<crate::hub::agent_messages::Message> = value
            .as_deref()
            .map(json_value)
            .transpose()?
            .unwrap_or_default();
        return Ok(messages.iter().any(|message| {
            message.id == receipt.source.id
                && message.token == receipt.recipient.token
                && message.session == receipt.recipient.session
                && message.state == crate::hub::agent_messages::MessageState::Delivered
        }));
    }
    if receipt.source.kind == SourceKind::Coordination {
        if let Some(id) = &receipt.recipient.work_id {
            let held:bool=tx.query_row("SELECT reservation_basis IS NOT NULL AND COALESCE(reservation_expires_at>?2,0) FROM hub_work_items WHERE id=?1",params![id,now],|row|row.get(0))?;
            if !held {
                return Ok(false);
            }
        }
        let Some(request) = coordination_request_at(tx, who, &receipt.source.id, i64::MAX, now)?
        else {
            return Ok(false);
        };
        if !matches!(
            request.status,
            crate::hub::coordination::CoordinationStatus::Pending
                | crate::hub::coordination::CoordinationStatus::Responded
        ) {
            return Ok(false);
        }
        if request.requester.token == receipt.recipient.token
            && request.requester.session == receipt.recipient.session
        {
            return crate::hub::agent_messages::coordination_reply_source(&request)
                .map(|source| source == receipt.source);
        }
        return Ok(request.targets.iter().any(|target| {
            target.lead.token == receipt.recipient.token
                && target.lead.session == receipt.recipient.session
                && crate::hub::agent_messages::coordination_source(&request, target)
                    .is_ok_and(|source| source == receipt.source)
        }));
    }
    if receipt.source.kind == SourceKind::Operational
        && let Some(incarnation) = receipt.source.version.strip_prefix("ended:")
    {
        let view = session_lifecycle::current_for_domain(
            tx,
            &receipt.recipient.token,
            &receipt.recipient.session,
        )?;
        return Ok(view.is_some_and(|view| {
            view.state == crate::hub::session_lifecycle::LifecycleState::Terminal
                && view.incarnation == incarnation
        }));
    }
    if receipt.source.kind == SourceKind::Dependency {
        let value = tx
            .query_row(
                "SELECT snapshot FROM hub_work_dependency_notices WHERE work=?1",
                [&receipt.source.id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        return value
            .map(|value| {
                json_value::<DependencyNoticeRecord>(&value)
                    .map(|record| dependency_version(&record) == receipt.source.version)
            })
            .transpose()
            .map(|value| value.unwrap_or(false));
    }
    Ok(true)
}

fn refresh_items(
    tx: &Transaction<'_>,
    who: &WorkWho,
    ids: Vec<String>,
    limit: usize,
    now: i64,
) -> Result<NoticeRefreshPage> {
    let complete = ids.len() <= limit;
    let mut page = NoticeRefreshPage {
        next_after: None,
        complete,
        coverage_complete: true,
        refreshed: 0,
        notices: vec![],
    };
    for id in ids.into_iter().take(limit) {
        work_dependencies::refresh(tx, who, &id, now, false, None)?;
        let Some(item) = scoped_work(tx, who, &id, now)? else {
            continue;
        };
        let stored = tx
            .query_row(
                "SELECT snapshot FROM hub_work_dependency_notices WHERE work=?1",
                [&id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(stored) = stored {
            let dependency: DependencyNoticeRecord = json_value(&stored)?;
            page.coverage_complete &= dependency.notification_fanout_complete;
            if dependency.semantic_version > 0 {
                let recipient = NoticeRecipient {
                    token: item.lead_token,
                    session: item.lead_session,
                    work_id: Some(id.clone()),
                    generation: Some(item.generation),
                };
                let notice = record(
                    tx,
                    who,
                    NoticeSource {
                        kind: SourceKind::Dependency,
                        id: id.clone(),
                        version: dependency_version(&dependency),
                    },
                    recipient,
                    now,
                )?;
                page.notices.push(notice);
            }
        }
        page.refreshed += 1;
        page.next_after = Some(id);
    }
    if complete {
        page.next_after = None;
    }
    Ok(page)
}

fn owner_reader(
    tx: &Transaction<'_>,
    reader: &WorkLifecycleReader<'_>,
    now: i64,
) -> Result<WorkWho> {
    match reader {
        WorkLifecycleReader::Token(token) => work_who(tx, token, now),
        WorkLifecycleReader::Person {
            owner,
            repository,
            repository_id,
        } => {
            let live:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM hub_users WHERE id=?1 AND disabled=0 AND provider=?2 AND host=?3 AND forge_user_id=?4)",
                params![owner.id,owner.identity.provider,owner.identity.host,owner.identity.user_id],|row|row.get(0))?;
            if owner.disabled || !live {
                return Err(HubError::Unauthorized);
            }
            Ok(work_transition::person_who(
                owner,
                repository,
                *repository_id,
            ))
        }
    }
}

impl SqliteStore {
    pub(super) fn notice_record(
        &self,
        token: &str,
        source: &NoticeSource,
        recipient: &NoticeRecipient,
        now: i64,
    ) -> Result<NoticeReceipt> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let receipt = record(&tx, &who, source.clone(), recipient.clone(), now)?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(super) fn notice_resolve(
        &self,
        token: &str,
        session: &str,
        source: &NoticeSource,
        now: i64,
    ) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let newly = resolve_source(&tx, source, token, session, now)?;
        tx.commit()?;
        Ok(newly)
    }

    // Preserve the Store boundary's explicit identity, CAS snapshot, and receipt fences.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn notice_message_update(
        &self,
        token: &str,
        session: &str,
        setting_key: &str,
        expected: Option<&str>,
        replacement: &str,
        source: &NoticeSource,
        incarnation: Option<&str>,
        now: i64,
    ) -> Result<Option<crate::hub::agent_messages::Mode>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        session_lifecycle::check(&tx, token, session, incarnation, now)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only
            || source.kind != SourceKind::HumanMessage
            || who
                .repository_id
                .is_none_or(|id| setting_key != format!("agent_messages:{}:{id}", who.owner.id))
        {
            return Err(HubError::Unauthorized);
        }
        let current = tx
            .query_row(
                "SELECT value FROM hub_settings WHERE key=?1",
                [setting_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if current.as_deref() != expected {
            return Ok(None);
        }
        let messages: Vec<crate::hub::agent_messages::Message> = json_value(replacement)?;
        let accepted = messages.iter().any(|message| {
            message.id == source.id
                && message.state == crate::hub::agent_messages::MessageState::Accepted
        });
        let mode = message_mode(&tx, &who)?;
        if accepted && mode == crate::hub::agent_messages::Mode::Ignore {
            return Err(HubError::invalid_or_changed(
                "Hub message mode is ignore; do not act.".into(),
            ));
        }
        if !messages.iter().any(|message| {
            message.id == source.id && message.token == token && message.session == session
        }) {
            return Err(HubError::Unauthorized);
        }
        tx.execute(
            &store_sql::sqlite(store_sql::SET_SETTING),
            params![setting_key, replacement, now, who.owner.id],
        )?;
        resolve_source(&tx, source, token, session, now)?;
        tx.commit()?;
        Ok(Some(mode))
    }

    pub(super) fn notice_refresh(
        &self,
        token: &str,
        query: &DependencyRefresh,
        now: i64,
    ) -> Result<NoticeRefreshPage> {
        query.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        session_lifecycle::registered_incarnation(&tx, token, &query.session, &query.incarnation)?;
        let domain = session_name(&tx, token, Some(&query.session), now)?;
        let ids = tx
            .prepare(
                "SELECT w.id FROM hub_work_items w JOIN hub_work_declarations d ON d.work=w.id
            WHERE w.lead_token=?1 AND w.lead_session=?2 AND w.state IN ('active','blocked')
            AND w.id>?3 ORDER BY w.id LIMIT ?4",
            )?
            .query_map(
                params![
                    token,
                    domain,
                    query.after.as_deref().unwrap_or(""),
                    (query.limit + 1) as i64
                ],
                |row| row.get(0),
            )?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        let page = refresh_items(&tx, &who, ids, query.limit, now)?;
        tx.commit()?;
        Ok(page)
    }

    pub(super) fn notice_owner_refresh(
        &self,
        reader: &WorkLifecycleReader<'_>,
        after: Option<&str>,
        limit: usize,
        now: i64,
    ) -> Result<NoticeRefreshPage> {
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(HubError::Invalid(
                "Notice refresh limit must be 1-100.".into(),
            ));
        }
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = owner_reader(&tx, reader, now)?;
        let ids = tx.prepare("SELECT w.id FROM hub_work_items w JOIN hub_work_declarations d ON d.work=w.id
            WHERE w.owner=?1 AND w.provider=?2 AND w.host=?3 AND w.repository=?4 AND w.repository_id IS ?5
            AND w.state IN ('active','blocked') AND w.id>?6 ORDER BY w.id LIMIT ?7")?
            .query_map(params![who.owner.id,who.owner.identity.provider,who.owner.identity.host,who.repository,
                who.repository_id,after.unwrap_or(""),(limit+1) as i64], |row|row.get(0))?
            .collect::<std::result::Result<Vec<String>,_>>()?;
        let page = refresh_items(&tx, &who, ids, limit, now)?;
        tx.commit()?;
        Ok(page)
    }

    pub(super) fn notice_offer(
        &self,
        token: &str,
        query: &NoticeOfferRequest,
        now: i64,
    ) -> Result<Option<NoticeOffer>> {
        query.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        session_lifecycle::registered_incarnation(&tx, token, &query.session, &query.incarnation)?;
        let domain = session_name(&tx, token, Some(&query.session), now)?;
        let values=tx.prepare("SELECT snapshot FROM hub_notice_deliveries WHERE token=?1 AND session=?2 AND json_extract(snapshot,'$.read_at') IS NULL AND json_extract(snapshot,'$.resolved_at') IS NULL AND COALESCE(json_extract(snapshot,'$.withdrawn'),0)=0 ORDER BY id LIMIT 4097")?
            .query_map(params![token,domain],|row|row.get::<_,String>(0))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        let incomplete = values.len() > 4096;
        for value in values.into_iter().take(4096) {
            let mut receipt: NoticeReceipt = json_value(&value)?;
            if !receipt.pending() {
                continue;
            }
            // Ignore holds a human notice temporarily; it never withdraws its source.
            if receipt.source.kind == SourceKind::HumanMessage
                && message_mode(&tx, &who)? == crate::hub::agent_messages::Mode::Ignore
            {
                continue;
            }
            if !current(&tx, &who, &receipt, now)? {
                receipt.withdrawn = true;
                save(&tx, &receipt)?;
                continue;
            }
            let attempt_id = if query.automatic {
                let id = new_id("nat")?;
                if !receipt.claim(&query.incarnation, id.clone(), now) {
                    continue;
                }
                Some(id)
            } else {
                if receipt.offered_at.is_some_and(|at| now - at < 60) {
                    continue;
                }
                receipt.offered_at = Some(now);
                None
            };
            save(&tx, &receipt)?;
            let offer = NoticeOffer {
                delivery_id: receipt.id.clone(),
                attempt_id,
                incarnation: query.incarnation.clone(),
                text: receipt.pointer(),
            };
            tx.commit()?;
            return Ok(Some(offer));
        }
        // Preserve stale withdrawals before reporting incomplete coverage, so another
        // ordinary call can make progress through an old unresolved backlog.
        tx.commit()?;
        if incomplete {
            return Err(HubError::Unavailable(
                "Notice offer coverage exceeds this call's bound; retry to continue.".into(),
            ));
        }
        Ok(None)
    }

    pub(super) fn notice_transport(
        &self,
        token: &str,
        report: &NoticeTransportReport,
        now: i64,
    ) -> Result<NoticeReceipt> {
        report.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only || report.observed_at > now.saturating_add(300) {
            return Err(HubError::Unauthorized);
        }
        // Late outcome reports retain the original attempt fence even after terminal observation.
        let domain = session_name(&tx, token, Some(&report.session), now)?;
        let mut receipt = load(&tx, &who, &report.delivery_id, &domain)?;
        receipt.report(report)?;
        save(&tx, &receipt)?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(super) fn dependency_notice_ack(
        &self,
        token: &str,
        ack: &DependencyNoticeAck,
        now: i64,
    ) -> Result<NoticeAcknowledgement> {
        ack.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        session_lifecycle::registered_incarnation(&tx, token, &ack.session, &ack.incarnation)?;
        let domain = session_name(&tx, token, Some(&ack.session), now)?;
        work_dependencies::refresh(&tx, &who, &ack.work_id, now, false, None)?;
        let mut receipt = load(&tx, &who, &ack.delivery_id, &domain)?;
        let snapshot: String = tx.query_row(
            "SELECT snapshot FROM hub_work_dependency_notices WHERE work=?1",
            [&ack.work_id],
            |row| row.get(0),
        )?;
        let record: DependencyNoticeRecord = json_value(&snapshot)?;
        if receipt.withdrawn
            || receipt.source.kind != SourceKind::Dependency
            || receipt.source.id != ack.work_id
            || record.generation != ack.generation
            || record.declaration_seq != ack.declaration_seq
            || record.semantic_version != ack.semantic_version
            || receipt.source.version != dependency_version(&record)
            || !current(&tx, &who, &receipt, now)?
        {
            return Err(HubError::invalid_or_changed(
                "Dependency changed; reread agentscm_work_dependency_notices.".into(),
            ));
        }
        let newly_acknowledged = receipt.read_at.is_none();
        receipt.read_at.get_or_insert(now);
        save(&tx, &receipt)?;
        tx.commit()?;
        Ok(NoticeAcknowledgement {
            newly_acknowledged,
            receipt,
            current: record,
        })
    }

    /// Person readers already passed the same repository-role gate as other scoped reads.
    pub(super) fn notice_owner_pending(
        &self,
        reader: &WorkLifecycleReader<'_>,
        now: i64,
    ) -> Result<Vec<NoticeReceipt>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = owner_reader(&tx, reader, now)?;
        let values=tx.prepare("SELECT snapshot FROM hub_notice_deliveries WHERE owner=?1 AND provider=?2 AND host=?3
            AND repository=?4 AND repository_id IS ?5 AND json_extract(snapshot,'$.read_at') IS NULL AND json_extract(snapshot,'$.resolved_at') IS NULL AND COALESCE(json_extract(snapshot,'$.withdrawn'),0)=0 ORDER BY id LIMIT 4097")?
            .query_map(params![who.owner.id,who.owner.identity.provider,who.owner.identity.host,who.repository,who.repository_id],|row|row.get::<_,String>(0))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        if values.len() > 4096 {
            return Err(HubError::Unavailable(
                "Owner notice coverage exceeds its bound.".into(),
            ));
        }
        let mut out = vec![];
        for value in values {
            let receipt: NoticeReceipt = json_value(&value)?;
            if receipt.pending() && current(&tx, &who, &receipt, now)? {
                out.push(receipt);
            }
        }
        Ok(out)
    }

    pub(super) fn notice_owner_terminal(
        &self,
        reader: &WorkLifecycleReader<'_>,
        ids: &[String],
        now: i64,
    ) -> Result<Vec<String>> {
        OperationalSelect::Many(ids).ids()?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = owner_reader(&tx, reader, now)?;
        let mut out = vec![];
        for id in ids {
            let Some(item) = scoped_work(&tx, &who, id, now)? else {
                continue;
            };
            if item.owner != who.owner.id || item.state == WorkState::Finished {
                continue;
            }
            if session_lifecycle::current_for_domain(&tx, &item.lead_token, &item.lead_session)?
                .is_some_and(|view| {
                    view.state == crate::hub::session_lifecycle::LifecycleState::Terminal
                })
            {
                out.push(id.clone());
            }
        }
        Ok(out)
    }

    pub(super) fn notice_pending(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Vec<NoticeReceipt>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let values=tx.prepare("SELECT snapshot FROM hub_notice_deliveries WHERE token=?1 AND session=?2 AND json_extract(snapshot,'$.read_at') IS NULL AND json_extract(snapshot,'$.resolved_at') IS NULL AND COALESCE(json_extract(snapshot,'$.withdrawn'),0)=0 ORDER BY id LIMIT 4097")?
            .query_map(params![token,session],|row|row.get::<_,String>(0))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        if values.len() > 4096 {
            return Err(HubError::Unavailable(
                "Session notice coverage exceeds its bound.".into(),
            ));
        }
        let mut notices = vec![];
        for value in values {
            let receipt: NoticeReceipt = json_value(&value)?;
            if receipt.pending() && current(&tx, &who, &receipt, now)? {
                notices.push(receipt);
            }
        }
        Ok(notices)
    }
}
