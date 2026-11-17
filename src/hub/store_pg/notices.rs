//! One transactional delivery ledger shared by source domains.
use super::*;
use crate::hub::agent_messages::{Message, MessageReceipt, ReceiptCoverage};
use crate::hub::intent::text;
use crate::hub::notices::*;
use crate::hub::work_dependencies::DependencyNoticeRecord;
use sha2::{Digest, Sha256};

fn owner_notice_digest(notice: &NewOwnerNotice) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(json_string(&(
            notice.source_kind,
            &notice.source_id,
            &notice.recipient,
            notice.route,
            &notice.body,
        ))?)
    ))
}

fn owner_notice_row(value: String) -> Result<OwnerNotice> {
    json_value(&value)
}

fn kind(source: &NoticeSource) -> Result<String> {
    json_string(&source.kind)
}

fn lock_owner(tx: &mut postgres::Transaction<'_>, owner: &str) -> Result<()> {
    let key = format!("notice-owner:{owner}");
    tx.query_one(
        "SELECT pg_advisory_xact_lock(hashtextextended($1,0))",
        &[&key],
    )?;
    Ok(())
}

fn save(tx: &mut postgres::Transaction<'_>, receipt: &NoticeReceipt) -> Result<()> {
    tx.execute(
        "UPDATE hub_notice_deliveries SET snapshot=$2 WHERE id=$1",
        &[&receipt.id, &json_string(receipt)?],
    )?;
    Ok(())
}

/// Call only after the source domain admitted the relationship in this transaction.
pub(super) fn record(
    tx: &mut postgres::Transaction<'_>,
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
    // Serialize creation on the recipient token, including concurrent old/new credential reads.
    lock_owner(tx, &recipient_who.owner.id)?;
    if let Some(row) = tx.query_opt(
        "SELECT snapshot FROM hub_notice_deliveries WHERE id=$1 FOR UPDATE",
        &[&receipt.id],
    )? {
        return json_value(&row.get::<_, String>(0));
    }
    let source_kind = kind(&receipt.source)?;
    let prior=tx.query("SELECT snapshot FROM hub_notice_deliveries WHERE owner=$1 AND provider=$2 AND host=$3
        AND (repository=$4 OR repository_id IS NOT NULL) AND repository_id IS NOT DISTINCT FROM $5 AND source_kind=$6 AND source_id=$7
        AND source_version=$8 AND session=$9 AND token<>$10 FOR UPDATE",
        &[&recipient_who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&who.repository,
            &who.repository_id,&source_kind,&receipt.source.id,&receipt.source.version,&receipt.recipient.session,&receipt.recipient.token])?;
    for row in prior {
        let previous: NoticeReceipt = json_value(&row.get::<_, String>(0))?;
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
    tx.execute("INSERT INTO hub_notice_deliveries(id,owner,provider,host,repository,repository_id,token,session,
        source_kind,source_id,source_version,snapshot) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
        &[&receipt.id,&recipient_who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&who.repository,
            &who.repository_id,&receipt.recipient.token,&receipt.recipient.session,&source_kind,&receipt.source.id,
            &receipt.source.version,&json_string(&receipt)?])?;
    Ok(receipt)
}

pub(super) fn resolve_source(
    tx: &mut postgres::Transaction<'_>,
    source: &NoticeSource,
    token: &str,
    session: &str,
    now: i64,
) -> Result<bool> {
    let rows = tx.query(
        "SELECT snapshot FROM hub_notice_deliveries WHERE source_kind=$1 AND source_id=$2
        AND source_version=$3 AND token=$4 AND session=$5 FOR UPDATE",
        &[
            &kind(source)?,
            &source.id,
            &source.version,
            &token,
            &session,
        ],
    )?;
    let mut newly = false;
    for row in rows {
        let mut receipt: NoticeReceipt = json_value(&row.get::<_, String>(0))?;
        newly |= receipt.read_at.is_none();
        receipt.read_at.get_or_insert(now);
        receipt.resolved_at.get_or_insert(now);
        save(tx, &receipt)?;
    }
    Ok(newly)
}

/// Terminal observation revokes idle transport, retaining pointers and attempt history.
pub(super) fn withdraw_for_terminal(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
    _incarnation: &str,
    now: i64,
) -> Result<()> {
    let owner = work_who(tx, token, now)?.owner.id;
    lock_owner(tx, &owner)?;
    tx.execute("UPDATE hub_notice_deliveries SET snapshot=jsonb_set(snapshot::jsonb,'{terminal_at}',to_jsonb($3::bigint),true)::text WHERE token=$1 AND session=$2 AND (snapshot::jsonb->>'terminal_at') IS NULL AND (snapshot::jsonb->>'read_at') IS NULL AND (snapshot::jsonb->>'resolved_at') IS NULL AND COALESCE((snapshot::jsonb->>'withdrawn')::boolean,false)=false", &[&token,&session,&now])?;
    let who = work_who(tx, token, now)?;
    let rows=tx.query("SELECT snapshot FROM hub_work_items WHERE lead_token=$1 AND lead_session=$2 AND state<>'finished' ORDER BY id LIMIT 101",&[&token,&session])?;
    if rows.len() > 100 {
        return Err(HubError::Unavailable(
            "Terminal Work notice coverage exceeds its bound.".into(),
        ));
    }
    for row in rows {
        let item: WorkItem = json_value(&row.get::<_, String>(0))?;
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

fn load(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    id: &str,
    session: &str,
) -> Result<NoticeReceipt> {
    let row=tx.query_opt("SELECT snapshot FROM hub_notice_deliveries WHERE id=$1 AND token=$2 AND session=$3
        AND owner=$4 AND provider=$5 AND host=$6 AND repository=$7 AND repository_id IS NOT DISTINCT FROM $8 FOR UPDATE",
        &[&id,&who.token,&session,&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&who.repository,&who.repository_id])?
        .ok_or_else(||HubError::invalid_or_changed("Notice is not addressed to this session.".into()))?;
    json_value(&row.get::<_, String>(0))
}

fn dependency_version(record: &DependencyNoticeRecord) -> String {
    format!(
        "{}:{}:{}",
        record.generation, record.declaration_seq, record.semantic_version
    )
}

fn message_mode(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
) -> Result<crate::hub::agent_messages::Mode> {
    let mut mode = crate::hub::agent_messages::Mode::Manual;
    for key in [
        format!("agent_message_mode:{}", who.owner.id),
        format!(
            "agent_message_mode:{}:{}",
            who.owner.id,
            who.repository_id.unwrap_or_default()
        ),
    ] {
        // Lock absent defaults too. JSON null preserves fallback semantics and lets mode
        // setters serialize on this row instead of racing an unprotected missing setting.
        tx.execute("INSERT INTO hub_settings(key,value,updated_at,updated_by) VALUES ($1,'null',0,$2) ON CONFLICT(key) DO NOTHING", &[&key, &who.owner.id])?;
        let value: String = tx
            .query_one(
                "SELECT value FROM hub_settings WHERE key=$1 FOR SHARE",
                &[&key],
            )?
            .get(0);
        if let Ok(current) = serde_json::from_str(&value) {
            mode = current;
        }
    }
    Ok(mode)
}

fn current(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    receipt: &NoticeReceipt,
    now: i64,
) -> Result<bool> {
    if let Some(id) = &receipt.recipient.work_id {
        let Some(item) = scoped_work(tx, who, id, now, false)? else {
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
            .query_opt("SELECT value FROM hub_settings WHERE key=$1", &[&key])?
            .map(|row| row.get::<_, String>(0));
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
            let held:bool=tx.query_one("SELECT reservation_basis IS NOT NULL AND COALESCE(reservation_expires_at>$2,false) FROM hub_work_items WHERE id=$1",&[id,&now])?.get(0);
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
        let value = tx.query_opt(
            "SELECT snapshot FROM hub_work_dependency_notices WHERE work=$1",
            &[&receipt.source.id],
        )?;
        return value
            .map(|row| {
                json_value::<DependencyNoticeRecord>(&row.get::<_, String>(0))
                    .map(|record| dependency_version(&record) == receipt.source.version)
            })
            .transpose()
            .map(|value| value.unwrap_or(false));
    }
    if receipt.source.kind == SourceKind::OwnerNotice {
        let row=tx.query_opt("SELECT snapshot FROM hub_owner_notices WHERE id=$1 AND recipient_owner=$2 AND provider=$3 AND host=$4 AND repository_id IS NOT DISTINCT FROM $5", &[&receipt.source.id,&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&who.repository_id])?;
        return row
            .map(|row| owner_notice_row(row.get(0)))
            .transpose()
            .map(|notice| notice.is_some_and(|notice| notice.acknowledged_at.is_none()));
    }
    Ok(true)
}

fn refresh_items(
    tx: &mut postgres::Transaction<'_>,
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
        let Some(item) = scoped_work(tx, who, &id, now, false)? else {
            continue;
        };
        if let Some(row) = tx.query_opt(
            "SELECT snapshot FROM hub_work_dependency_notices WHERE work=$1",
            &[&id],
        )? {
            let dependency: DependencyNoticeRecord = json_value(&row.get::<_, String>(0))?;
            page.coverage_complete &= dependency.notification_fanout_complete;
            if dependency.semantic_version > 0 {
                let recipient = NoticeRecipient {
                    token: item.lead_token,
                    session: item.lead_session,
                    work_id: Some(id.clone()),
                    generation: Some(item.generation),
                };
                page.notices.push(record(
                    tx,
                    who,
                    NoticeSource {
                        kind: SourceKind::Dependency,
                        id: id.clone(),
                        version: dependency_version(&dependency),
                    },
                    recipient,
                    now,
                )?);
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
    tx: &mut postgres::Transaction<'_>,
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
            let live:bool=tx.query_one("SELECT EXISTS(SELECT 1 FROM hub_users WHERE id=$1 AND NOT disabled AND provider=$2 AND host=$3 AND forge_user_id=$4)",
                &[&owner.id,&owner.identity.provider,&owner.identity.host,&owner.identity.user_id])?.get(0);
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

impl PgStore {
    pub(super) fn owner_notice_invalidation_put(
        &self,
        repository: &str,
        repository_id: i64,
        request: &NewOwnerNotice,
        now: i64,
    ) -> Result<OwnerNotice> {
        request.validate()?;
        if request.source_kind != OwnerNoticeSourceKind::AttestationInvalidation {
            return Err(HubError::Invalid(
                "Automatic owner notices must be attestation invalidations.".into(),
            ));
        }
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let recipient_exists: bool = tx.query_one(
                "SELECT EXISTS(SELECT 1 FROM hub_users WHERE id=$1 AND provider=$2 AND host=$3 AND NOT disabled)",
                &[&request.recipient.owner, &request.recipient.provider, &request.recipient.host],
            )?.get(0);
            if !recipient_exists {
                return Err(HubError::Unauthorized);
            }
            lock_owner(&mut tx, &request.recipient.owner)?;
            let digest = owner_notice_digest(request)?;
            let source_kind = json_string(&request.source_kind)?;
            if let Some(row) = tx.query_opt(
                "SELECT request_digest,snapshot FROM hub_owner_notices WHERE recipient_owner=$1 AND provider=$2 AND host=$3 AND repository_id=$4 AND source_kind=$5 AND source_id=$6 FOR UPDATE",
                &[&request.recipient.owner, &request.recipient.provider, &request.recipient.host, &repository_id, &source_kind, &request.source_id],
            )? {
                let prior_digest: String = row.get(0);
                return if prior_digest == digest {
                    owner_notice_row(row.get(1))
                } else {
                    Err(HubError::invalid_or_changed(
                        "Owner notice source already has another request.".into(),
                    ))
                };
            }
            let notice = OwnerNotice {
                id: new_id("ont")?,
                idempotency_key: request.idempotency_key.clone(),
                source_kind: request.source_kind,
                source_id: request.source_id.clone(),
                sender_owner: "hub".into(),
                sender_token: "hub:validation".into(),
                recipient: request.recipient.clone(),
                route: request.route,
                repository: repository.into(),
                repository_id: Some(repository_id),
                body: request.body.clone(),
                created_at: now,
                delivery_attempts: vec![],
                delivered_at: None,
                delivery: None,
                acknowledged_at: None,
                acted_at: None,
            };
            tx.execute(
                "INSERT INTO hub_owner_notices(id,idempotency_key,request_digest,sender_owner,sender_token,recipient_owner,provider,host,repository,repository_id,source_kind,source_id,created_at,snapshot) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
                &[&notice.id, &notice.idempotency_key, &digest, &notice.sender_owner, &notice.sender_token, &notice.recipient.owner, &notice.recipient.provider, &notice.recipient.host, &notice.repository, &repository_id, &source_kind, &notice.source_id, &now, &json_string(&notice)?],
            )?;
            tx.commit()?;
            Ok(notice)
        })
    }

    pub(super) fn owner_notice_put(
        &self,
        token: &str,
        request: &NewOwnerNotice,
        now: i64,
    ) -> Result<OwnerNotice> {
        request.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            let repository_id = who.repository_id.ok_or(HubError::Unauthorized)?;
            if who.read_only
                || request.recipient.provider != who.owner.identity.provider
                || request.recipient.host != who.owner.identity.host
            {
                return Err(HubError::Unauthorized);
            }
            let recipient_exists: bool = tx.query_one(
                "SELECT EXISTS(SELECT 1 FROM hub_users WHERE id=$1 AND provider=$2 AND host=$3 AND NOT disabled)",
                &[&request.recipient.owner, &request.recipient.provider, &request.recipient.host],
            )?.get(0);
            if !recipient_exists {
                return Err(HubError::Unauthorized);
            }
            // Serialize admission for this sender/recipient even before either unique key exists.
            lock_owner(&mut tx, &who.owner.id)?;
            lock_owner(&mut tx, &request.recipient.owner)?;
            let digest = owner_notice_digest(request)?;
            if let Some(row) = tx.query_opt(
                "SELECT request_digest,snapshot FROM hub_owner_notices WHERE sender_owner=$1 AND provider=$2 AND host=$3 AND repository_id=$4 AND idempotency_key=$5 FOR UPDATE",
                &[&who.owner.id, &who.owner.identity.provider, &who.owner.identity.host, &repository_id, &request.idempotency_key],
            )? {
                let prior_digest: String = row.get(0);
                return if prior_digest == digest {
                    owner_notice_row(row.get(1))
                } else {
                    Err(HubError::invalid_or_changed(
                        "Owner notice idempotency key already has another request.".into(),
                    ))
                };
            }
            let source_kind = json_string(&request.source_kind)?;
            if let Some(row) = tx.query_opt(
                "SELECT request_digest,snapshot FROM hub_owner_notices WHERE recipient_owner=$1 AND provider=$2 AND host=$3 AND repository_id=$4 AND source_kind=$5 AND source_id=$6 FOR UPDATE",
                &[&request.recipient.owner, &request.recipient.provider, &request.recipient.host, &repository_id, &source_kind, &request.source_id],
            )? {
                let prior_digest: String = row.get(0);
                return if prior_digest == digest {
                    owner_notice_row(row.get(1))
                } else {
                    Err(HubError::invalid_or_changed(
                        "Owner notice source already has another request.".into(),
                    ))
                };
            }
            let recent: i64 = tx.query_one(
                "SELECT count(*) FROM hub_owner_notices WHERE sender_token=$1 AND recipient_owner=$2 AND provider=$3 AND host=$4 AND repository_id=$5 AND created_at>=$6",
                &[&token, &request.recipient.owner, &request.recipient.provider, &request.recipient.host, &repository_id, &now.saturating_sub(OWNER_NOTICE_RATE_WINDOW_SECS)],
            )?.get(0);
            if recent >= MAX_OWNER_NOTICES_PER_RECIPIENT_WINDOW as i64 {
                return Err(HubError::Unavailable(
                    "Owner notice recipient rate limit reached.".into(),
                ));
            }
            let notice = OwnerNotice {
                id: new_id("ont")?,
                idempotency_key: request.idempotency_key.clone(),
                source_kind: request.source_kind,
                source_id: request.source_id.clone(),
                sender_owner: who.owner.id.clone(),
                sender_token: token.into(),
                recipient: request.recipient.clone(),
                route: request.route,
                repository: who.repository,
                repository_id: Some(repository_id),
                body: request.body.clone(),
                created_at: now,
                delivery_attempts: vec![],
                delivered_at: None,
                delivery: None,
                acknowledged_at: None,
                acted_at: None,
            };
            tx.execute(
                "INSERT INTO hub_owner_notices(id,idempotency_key,request_digest,sender_owner,sender_token,recipient_owner,provider,host,repository,repository_id,source_kind,source_id,created_at,snapshot) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
                &[&notice.id, &notice.idempotency_key, &digest, &notice.sender_owner, &token, &notice.recipient.owner, &notice.recipient.provider, &notice.recipient.host, &notice.repository, &repository_id, &source_kind, &notice.source_id, &now, &json_string(&notice)?],
            )?;
            tx.commit()?;
            Ok(notice)
        })
    }

    pub(super) fn owner_notice_delivered(
        &self,
        token: &str,
        id: &str,
        proof: &OwnerNoticeDeliveryProof,
        now: i64,
    ) -> Result<OwnerNotice> {
        text("owner notice id", id, 255)?;
        proof.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            let row = tx.query_opt(
                "SELECT snapshot FROM hub_owner_notices WHERE id=$1 AND sender_token=$2 AND repository_id=$3 FOR UPDATE",
                &[&id, &token, &who.repository_id],
            )?.ok_or(HubError::Unauthorized)?;
            let mut notice = owner_notice_row(row.get(0))?;
            match (&notice.delivery, notice.delivered_at) {
                (Some(existing), Some(_)) if existing == proof => {}
                (Some(_), Some(_)) => return Err(HubError::invalid_or_changed(
                    "Owner notice already has another delivery proof.".into(),
                )),
                _ => {
                    notice.delivery = Some(proof.clone());
                    notice.delivered_at = Some(now);
                    tx.execute("UPDATE hub_owner_notices SET snapshot=$2 WHERE id=$1", &[&id, &json_string(&notice)?])?;
                }
            }
            tx.commit()?;
            Ok(notice)
        })
    }

    pub(super) fn owner_notice_delivery_claim(
        &self,
        reader: &WorkLifecycleReader<'_>,
        id: &str,
        channel: &str,
        now: i64,
    ) -> Result<Option<OwnerNoticeDeliveryClaim>> {
        text("owner notice id", id, 255)?;
        text("owner notice delivery channel", channel, 32)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = owner_reader(&mut tx, reader, now)?;
            let repository_id = who.repository_id.ok_or(HubError::Unauthorized)?;
            lock_owner(&mut tx, &who.owner.id)?;
            let row = tx.query_opt(
                "SELECT snapshot FROM hub_owner_notices WHERE id=$1 AND recipient_owner=$2 AND provider=$3 AND host=$4 AND repository_id=$5 FOR UPDATE",
                &[&id, &who.owner.id, &who.owner.identity.provider, &who.owner.identity.host, &repository_id],
            )?.ok_or(HubError::Unauthorized)?;
            let mut notice = owner_notice_row(row.get(0))?;
            if !notice.delivery_eligible() {
                tx.commit()?;
                return Ok(None);
            }
            let attempt_id = new_id("ona")?;
            notice.delivery_attempts.push(OwnerNoticeDeliveryAttempt {
                id: attempt_id.clone(),
                channel: channel.into(),
                outcome: AttemptOutcome::PossiblyEnqueued,
                created_at: now,
                reported_at: None,
            });
            tx.execute("UPDATE hub_owner_notices SET snapshot=$2 WHERE id=$1", &[&id, &json_string(&notice)?])?;
            tx.commit()?;
            Ok(Some(OwnerNoticeDeliveryClaim { notice, attempt_id }))
        })
    }

    pub(super) fn owner_notice_delivery_report(
        &self,
        reader: &WorkLifecycleReader<'_>,
        id: &str,
        report: &OwnerNoticeDeliveryReport,
        now: i64,
    ) -> Result<OwnerNotice> {
        text("owner notice id", id, 255)?;
        report.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = owner_reader(&mut tx, reader, now)?;
            lock_owner(&mut tx, &who.owner.id)?;
            let row = tx.query_opt(
                "SELECT snapshot FROM hub_owner_notices WHERE id=$1 AND recipient_owner=$2 AND provider=$3 AND host=$4 AND repository_id=$5 FOR UPDATE",
                &[&id, &who.owner.id, &who.owner.identity.provider, &who.owner.identity.host, &who.repository_id],
            )?.ok_or(HubError::Unauthorized)?;
            let mut notice = owner_notice_row(row.get(0))?;
            let attempt = notice.delivery_attempts.iter_mut().find(|attempt| attempt.id == report.attempt_id)
                .ok_or_else(|| HubError::invalid_or_changed("Owner notice delivery attempt changed.".into()))?;
            if attempt.reported_at.is_some() {
                let same = attempt.outcome == report.outcome
                    && (report.proof.is_none() || notice.delivery.as_ref() == report.proof.as_ref());
                if !same {
                    return Err(HubError::invalid_or_changed("Owner notice delivery attempt already has another outcome.".into()));
                }
                tx.commit()?;
                return Ok(notice);
            }
            attempt.outcome = report.outcome;
            attempt.reported_at = Some(now);
            if let Some(proof) = &report.proof {
                notice.delivery = Some(proof.clone());
                notice.delivered_at = Some(now);
            }
            tx.execute("UPDATE hub_owner_notices SET snapshot=$2 WHERE id=$1", &[&id, &json_string(&notice)?])?;
            tx.commit()?;
            Ok(notice)
        })
    }

    pub(super) fn owner_notice_list(
        &self,
        reader: &WorkLifecycleReader<'_>,
        after: Option<&str>,
        limit: usize,
        now: i64,
    ) -> Result<OwnerNoticePage> {
        if !(1..=MAX_OWNER_NOTICE_PAGE).contains(&limit) {
            return Err(HubError::Invalid(
                "Owner notice limit must be 1-100.".into(),
            ));
        }
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = owner_reader(&mut tx, reader, now)?;
            let repository_id = who.repository_id.ok_or(HubError::Unauthorized)?;
            let mut values = tx.query(
                "SELECT snapshot FROM hub_owner_notices WHERE recipient_owner=$1 AND provider=$2 AND host=$3 AND repository_id=$4 AND id>$5 ORDER BY id LIMIT $6",
                &[&who.owner.id, &who.owner.identity.provider, &who.owner.identity.host, &repository_id, &after.unwrap_or(""), &((limit + 1) as i64)],
            )?.into_iter().map(|row| row.get::<_, String>(0)).collect::<Vec<_>>();
            let has_more = values.len() > limit;
            values.truncate(limit);
            let notices = values.into_iter().map(owner_notice_row).collect::<Result<Vec<_>>>()?;
            let next_after = has_more.then(|| notices.last().expect("nonempty page").id.clone());
            tx.commit()?;
            Ok(OwnerNoticePage { notices, next_after })
        })
    }

    pub(super) fn owner_notice_get(&self, token: &str, id: &str, now: i64) -> Result<OwnerNotice> {
        text("owner notice id", id, 255)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            let repository_id = who.repository_id.ok_or(HubError::Unauthorized)?;
            let row = tx.query_opt(
                "SELECT snapshot FROM hub_owner_notices WHERE id=$1 AND repository_id=$2 AND provider=$3 AND host=$4 AND (sender_owner=$5 OR recipient_owner=$5)",
                &[&id, &repository_id, &who.owner.identity.provider, &who.owner.identity.host, &who.owner.id],
            )?.ok_or(HubError::Unauthorized)?;
            let notice = owner_notice_row(row.get(0))?;
            tx.commit()?;
            Ok(notice)
        })
    }

    pub(super) fn owner_notice_recipient_status(
        &self,
        reader: &WorkLifecycleReader<'_>,
        id: &str,
        status: OwnerNoticeRecipientStatus,
        now: i64,
    ) -> Result<OwnerNotice> {
        text("owner notice id", id, 255)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = owner_reader(&mut tx, reader, now)?;
            let row = tx.query_opt(
                "SELECT snapshot FROM hub_owner_notices WHERE id=$1 AND recipient_owner=$2 AND provider=$3 AND host=$4 AND repository_id=$5 FOR UPDATE",
                &[&id, &who.owner.id, &who.owner.identity.provider, &who.owner.identity.host, &who.repository_id],
            )?.ok_or(HubError::Unauthorized)?;
            let mut notice = owner_notice_row(row.get(0))?;
            match status {
                OwnerNoticeRecipientStatus::Acknowledged => notice.acknowledged_at.get_or_insert(now),
                OwnerNoticeRecipientStatus::Acted => {
                    notice.acknowledged_at.get_or_insert(now);
                    notice.acted_at.get_or_insert(now)
                }
            };
            tx.execute("UPDATE hub_owner_notices SET snapshot=$2 WHERE id=$1", &[&id, &json_string(&notice)?])?;
            tx.commit()?;
            Ok(notice)
        })
    }

    pub(super) fn notice_message_receipts(
        &self,
        owner: &User,
        repository: &str,
        repository_id: i64,
        messages: &[Message],
        now: i64,
    ) -> Result<Vec<MessageReceipt>> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let reader = WorkLifecycleReader::Person {
                owner,
                repository,
                repository_id,
            };
            // Owner validation must succeed before unavailable evidence can preserve a legacy page.
            owner_reader(&mut tx, &reader, now)?;
            if repository_id <= 0 || messages.len() > crate::hub::agent_messages::MAX_MESSAGES {
                return Err(HubError::Invalid("Invalid message receipt batch.".into()));
            }
            let mut read = || -> Result<Vec<MessageReceipt>> {
                let source_kind = json_string(&SourceKind::HumanMessage)?;
                let statement = tx.prepare(
                    "SELECT id,snapshot FROM hub_notice_deliveries
                     WHERE token=$1 AND session=$2 AND owner=$3 AND provider=$4 AND host=$5
                     AND repository_id=$6 AND source_kind=$7 AND source_id=$8 AND source_version='1'
                     ORDER BY id LIMIT 2",
                )?;
                let mut out = Vec::with_capacity(messages.len());
                for message in messages {
                    let eligible = match work_who(&mut tx, &message.token, now) {
                        Ok(who) => {
                            !who.read_only
                                && who.owner.id == owner.id
                                && who.owner.identity == owner.identity
                                && who.repository == repository
                                && who.repository_id == Some(repository_id)
                        }
                        Err(HubError::Unauthorized) => false,
                        Err(error) => return Err(error),
                    };
                    if !eligible {
                        out.push(MessageReceipt::unknown(ReceiptCoverage::Unavailable));
                        continue;
                    }
                    let rows = tx.query(
                        &statement,
                        &[
                            &message.token,
                            &message.session,
                            &owner.id,
                            &owner.identity.provider,
                            &owner.identity.host,
                            &repository_id,
                            &source_kind,
                            &message.id,
                        ],
                    )?;
                    out.push(match rows.as_slice() {
                        [] => MessageReceipt::unknown(ReceiptCoverage::LegacyUnknown),
                        [row] => MessageReceipt::from_snapshot(
                            &row.get::<_, String>(0),
                            &row.get::<_, String>(1),
                            message,
                        )?,
                        _ => MessageReceipt::unknown(ReceiptCoverage::Unavailable),
                    });
                }
                Ok(out)
            };
            Ok(read().unwrap_or_else(|_| {
                messages
                    .iter()
                    .map(|_| MessageReceipt::unknown(ReceiptCoverage::Unavailable))
                    .collect()
            }))
        })
    }

    pub(super) fn notice_record(
        &self,
        token: &str,
        source: &NoticeSource,
        recipient: &NoticeRecipient,
        now: i64,
    ) -> Result<NoticeReceipt> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            let receipt = record(&mut tx, &who, source.clone(), recipient.clone(), now)?;
            tx.commit()?;
            Ok(receipt)
        })
    }

    pub(super) fn notice_resolve(
        &self,
        token: &str,
        session: &str,
        source: &NoticeSource,
        now: i64,
    ) -> Result<bool> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            let newly = resolve_source(&mut tx, source, token, session, now)?;
            tx.commit()?;
            Ok(newly)
        })
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
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            session_lifecycle::check(&mut tx, token, session, incarnation, now)?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only
                || source.kind != SourceKind::HumanMessage
                || who
                    .repository_id
                    .is_none_or(|id| setting_key != format!("agent_messages:{}:{id}", who.owner.id))
            {
                return Err(HubError::Unauthorized);
            }
            let current = tx
                .query_opt(
                    "SELECT value FROM hub_settings WHERE key=$1 FOR UPDATE",
                    &[&setting_key],
                )?
                .map(|row| row.get::<_, String>(0));
            if current.as_deref() != expected {
                return Ok(None);
            }
            let messages: Vec<crate::hub::agent_messages::Message> = json_value(replacement)?;
            let accepted = messages.iter().any(|message| {
                message.id == source.id
                    && message.state == crate::hub::agent_messages::MessageState::Accepted
            });
            let mode = message_mode(&mut tx, &who)?;
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
                store_sql::SET_SETTING,
                &[&setting_key, &replacement, &now, &who.owner.id],
            )?;
            resolve_source(&mut tx, source, token, session, now)?;
            tx.commit()?;
            Ok(Some(mode))
        })
    }

    pub(super) fn notice_refresh(
        &self,
        token: &str,
        query: &DependencyRefresh,
        now: i64,
    ) -> Result<NoticeRefreshPage> {
        query.validate()?;
        self.run(false,|client| {
            let mut tx=client.transaction()?;
            let who=work_who(&mut tx,token,now)?;
            if who.read_only{return Err(HubError::Unauthorized);}
            session_lifecycle::registered_incarnation(&mut tx,token,&query.session,&query.incarnation)?;
        let domain = session_name(&mut tx,token,Some(&query.session),now)?;
            let ids=tx.query("SELECT w.id FROM hub_work_items w JOIN hub_work_declarations d ON d.work=w.id
                WHERE w.lead_token=$1 AND w.lead_session=$2 AND w.state IN ('active','blocked') AND w.id>$3 ORDER BY w.id LIMIT $4",
                &[&token,&domain,&query.after.as_deref().unwrap_or(""),&((query.limit+1) as i64)])?
                .iter().map(|row|row.get(0)).collect();
            let page=refresh_items(&mut tx,&who,ids,query.limit,now)?;
            let rows=tx.query("SELECT snapshot FROM hub_owner_notices WHERE recipient_owner=$1 AND provider=$2 AND host=$3 AND repository_id IS NOT DISTINCT FROM $4 AND (snapshot::jsonb->>'acknowledged_at') IS NULL ORDER BY id LIMIT 101", &[&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&who.repository_id])?;
            if rows.len()>100{return Err(HubError::Unavailable("Owner notice refresh coverage exceeds its bound.".into()));}
            for row in rows {
                let notice=owner_notice_row(row.get(0))?;
                record(&mut tx,&who,NoticeSource{kind:SourceKind::OwnerNotice,id:notice.id,version:notice.created_at.to_string()},NoticeRecipient{token:token.into(),session:domain.clone(),work_id:None,generation:None},now)?;
            }
            tx.commit()?;
            Ok(page)
        })
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
        self.run(false,|client| {
            let mut tx=client.transaction()?;
            let who=owner_reader(&mut tx,reader,now)?;
            let ids=tx.query("SELECT w.id FROM hub_work_items w JOIN hub_work_declarations d ON d.work=w.id
                WHERE w.owner=$1 AND w.provider=$2 AND w.host=$3 AND w.repository=$4 AND w.repository_id IS NOT DISTINCT FROM $5
                AND w.state IN ('active','blocked') AND w.id>$6 ORDER BY w.id LIMIT $7",
                &[&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&who.repository,&who.repository_id,&after.unwrap_or(""),&((limit+1) as i64)])?
                .iter().map(|row|row.get(0)).collect();
            let page=refresh_items(&mut tx,&who,ids,limit,now)?;
            tx.commit()?;
            Ok(page)
        })
    }

    pub(super) fn notice_offer(
        &self,
        token: &str,
        query: &NoticeOfferRequest,
        now: i64,
    ) -> Result<Option<NoticeOffer>> {
        query.validate()?;
        self.run(false,|client| {
            let mut tx=client.transaction()?;
            let who=work_who(&mut tx,token,now)?;
            if who.read_only{return Err(HubError::Unauthorized);}
            session_lifecycle::registered_incarnation(&mut tx,token,&query.session,&query.incarnation)?;
        let domain = session_name(&mut tx,token,Some(&query.session),now)?;
            // Acquire consent rows before receipt rows, matching message acceptance order.
            let human_mode=message_mode(&mut tx,&who)?;
            lock_owner(&mut tx,&who.owner.id)?;
            let rows=tx.query("SELECT snapshot FROM hub_notice_deliveries WHERE token=$1 AND session=$2 AND (snapshot::jsonb->>'read_at') IS NULL AND (snapshot::jsonb->>'resolved_at') IS NULL AND COALESCE((snapshot::jsonb->>'withdrawn')::boolean,false)=false ORDER BY id LIMIT 4097 FOR UPDATE",&[&token,&domain])?;
            let incomplete=rows.len()>4096;
            for row in rows.into_iter().take(4096) {
                let mut receipt:NoticeReceipt=json_value(&row.get::<_,String>(0))?;
                if !receipt.pending(){continue;}
                // Ignore holds a human notice temporarily; it never withdraws its source.
                if receipt.source.kind == SourceKind::HumanMessage && human_mode == crate::hub::agent_messages::Mode::Ignore {continue;}
                if !current(&mut tx,&who,&receipt,now)?{receipt.withdrawn=true;save(&mut tx,&receipt)?;continue;}
                let attempt_id=if query.automatic {
                    let id=new_id("nat")?;
                    if !receipt.claim(&query.incarnation,id.clone(),now){continue;}
                    Some(id)
                }else{
                    if receipt.offered_at.is_some_and(|at|now-at<60){continue;}
                    receipt.offered_at=Some(now);None
                };
                save(&mut tx,&receipt)?;
                let offer=NoticeOffer{delivery_id:receipt.id.clone(),attempt_id,incarnation:query.incarnation.clone(),text:receipt.pointer()};
                tx.commit()?;
                return Ok(Some(offer));
            }
            // Preserve stale withdrawals before reporting incomplete coverage, so another
            // ordinary call can make progress through an old unresolved backlog.
            tx.commit()?;
            if incomplete {return Err(HubError::Unavailable("Notice offer coverage exceeds this call's bound; retry to continue.".into()));}
            Ok(None)
        })
    }

    pub(super) fn notice_transport(
        &self,
        token: &str,
        report: &NoticeTransportReport,
        now: i64,
    ) -> Result<NoticeReceipt> {
        report.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only || report.observed_at > now.saturating_add(300) {
                return Err(HubError::Unauthorized);
            }
            let domain = session_name(&mut tx, token, Some(&report.session), now)?;
            let mut receipt = load(&mut tx, &who, &report.delivery_id, &domain)?;
            receipt.report(report)?;
            save(&mut tx, &receipt)?;
            tx.commit()?;
            Ok(receipt)
        })
    }

    pub(super) fn dependency_notice_ack(
        &self,
        token: &str,
        ack: &DependencyNoticeAck,
        now: i64,
    ) -> Result<NoticeAcknowledgement> {
        ack.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            session_lifecycle::registered_incarnation(
                &mut tx,
                token,
                &ack.session,
                &ack.incarnation,
            )?;
            let domain = session_name(&mut tx, token, Some(&ack.session), now)?;
            work_dependencies::refresh(&mut tx, &who, &ack.work_id, now, false, None)?;
            let mut receipt = load(&mut tx, &who, &ack.delivery_id, &domain)?;
            let row = tx.query_one(
                "SELECT snapshot FROM hub_work_dependency_notices WHERE work=$1",
                &[&ack.work_id],
            )?;
            let record: DependencyNoticeRecord = json_value(&row.get::<_, String>(0))?;
            if receipt.withdrawn
                || receipt.source.kind != SourceKind::Dependency
                || receipt.source.id != ack.work_id
                || record.generation != ack.generation
                || record.declaration_seq != ack.declaration_seq
                || record.semantic_version != ack.semantic_version
                || receipt.source.version != dependency_version(&record)
                || !current(&mut tx, &who, &receipt, now)?
            {
                return Err(HubError::invalid_or_changed(
                    "Dependency changed; reread agentscm_work_dependency_notices.".into(),
                ));
            }
            let newly_acknowledged = receipt.read_at.is_none();
            receipt.read_at.get_or_insert(now);
            save(&mut tx, &receipt)?;
            tx.commit()?;
            Ok(NoticeAcknowledgement {
                newly_acknowledged,
                receipt,
                current: record,
            })
        })
    }

    /// Person readers already passed the same repository-role gate as other scoped reads.
    pub(super) fn notice_owner_pending(
        &self,
        reader: &WorkLifecycleReader<'_>,
        now: i64,
    ) -> Result<Vec<NoticeReceipt>> {
        self.run(false,|client|{let mut tx=client.transaction()?;let who=owner_reader(&mut tx,reader,now)?;
            let rows=tx.query("SELECT snapshot FROM hub_notice_deliveries WHERE owner=$1 AND provider=$2 AND host=$3
                AND repository=$4 AND repository_id IS NOT DISTINCT FROM $5 AND (snapshot::jsonb->>'read_at') IS NULL AND (snapshot::jsonb->>'resolved_at') IS NULL AND COALESCE((snapshot::jsonb->>'withdrawn')::boolean,false)=false ORDER BY id LIMIT 4097",
                &[&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&who.repository,&who.repository_id])?;
            if rows.len()>4096{return Err(HubError::Unavailable("Owner notice coverage exceeds its bound.".into()));}
            let mut out=vec![];for row in rows{let receipt:NoticeReceipt=json_value(&row.get::<_,String>(0))?;
                if receipt.pending()&&current(&mut tx,&who,&receipt,now)?{out.push(receipt);}}
            tx.commit()?;Ok(out)})
    }

    pub(super) fn notice_owner_terminal(
        &self,
        reader: &WorkLifecycleReader<'_>,
        ids: &[String],
        now: i64,
    ) -> Result<Vec<String>> {
        OperationalSelect::Many(ids).ids()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = owner_reader(&mut tx, reader, now)?;
            let mut out = vec![];
            for id in ids {
                let Some(item) = scoped_work(&mut tx, &who, id, now, false)? else {
                    continue;
                };
                if item.owner != who.owner.id || item.state == WorkState::Finished {
                    continue;
                }
                if session_lifecycle::current_for_domain(
                    &mut tx,
                    &item.lead_token,
                    &item.lead_session,
                )?
                .is_some_and(|view| {
                    view.state == crate::hub::session_lifecycle::LifecycleState::Terminal
                }) {
                    out.push(id.clone());
                }
            }
            tx.commit()?;
            Ok(out)
        })
    }

    pub(super) fn notice_pending(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Vec<NoticeReceipt>> {
        self.run(false,|client| {
            let mut tx=client.transaction()?;
            let who=work_who(&mut tx,token,now)?;
            let rows=tx.query("SELECT snapshot FROM hub_notice_deliveries WHERE token=$1 AND session=$2 AND (snapshot::jsonb->>'read_at') IS NULL AND (snapshot::jsonb->>'resolved_at') IS NULL AND COALESCE((snapshot::jsonb->>'withdrawn')::boolean,false)=false ORDER BY id LIMIT 4097",&[&token,&session])?;
            if rows.len()>4096{return Err(HubError::Unavailable("Session notice coverage exceeds its bound.".into()));}
            let mut notices=vec![];
            for row in rows {
                let receipt:NoticeReceipt=json_value(&row.get::<_,String>(0))?;
                if receipt.pending()&&current(&mut tx,&who,&receipt,now)?{notices.push(receipt);}
            }
            tx.commit()?;
            Ok(notices)
        })
    }
}
