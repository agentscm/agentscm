use super::{SqliteStore, work_who};
use crate::hub::validation_evidence::{
    DurableValidationBundle, ValidationEvidencePage, ValidationEvidenceReceipt,
    ValidationSummaryConfig, page, validate_page,
};
use crate::hub::{HubError, Result};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
pub(super) fn configure(store: &SqliteStore, config: &ValidationSummaryConfig) -> Result<()> {
    let mut conn = store.conn();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let matched = tx
        .query_row(
            "SELECT 1 FROM hub_repositories WHERE id=?1 AND connection=?2",
            params![config.repository, config.connection],
            |row| row.get::<_, i32>(0),
        )
        .optional()?;
    if matched.is_none() {
        return Err(HubError::Invalid(
            "summary connection does not own repository".into(),
        ));
    }
    tx.execute(
        "INSERT INTO hub_validation_summary_config(repository,connection,enabled) VALUES(?1,?2,?3)
                 ON CONFLICT(repository) DO UPDATE SET
                 connection=excluded.connection,enabled=excluded.enabled",
        params![
            config.repository,
            config.connection,
            (if config.enabled { 1_i64 } else { 0_i64 })
        ],
    )?;
    tx.commit()?;
    Ok(())
}
pub(super) fn config(
    store: &SqliteStore,
    repository: &str,
) -> Result<Option<ValidationSummaryConfig>> {
    let mut conn = store.conn();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let result = tx
        .query_row(
            "SELECT connection,enabled FROM hub_validation_summary_config WHERE repository=?1",
            params![repository],
            |row| {
                Ok(ValidationSummaryConfig {
                    repository: repository.into(),
                    connection: row.get::<_, String>(0)?,
                    enabled: row.get::<_, i64>(1)? != 0,
                })
            },
        )
        .optional()?;
    tx.commit()?;
    Ok(result)
}
pub(super) fn receive(
    store: &SqliteStore,
    token: &str,
    proposal: i64,
    bundle: &DurableValidationBundle,
    now: i64,
) -> Result<ValidationEvidenceReceipt> {
    validate_page(proposal, 0, 1)?;
    let repository = bundle.repository.as_str();
    let mut conn = store.conn();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let who = work_who(&tx, token, now)?;
    let scoped = tx
        .query_row(
            "SELECT c.provider,r.host,r.full_name,r.repository_id FROM hub_repositories r JOIN
                 hub_connections c ON c.id=r.connection WHERE r.id=?1",
            params![repository],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or(HubError::Unauthorized)?;
    if who.owner.identity.provider != scoped.0
        || who.owner.identity.host != scoped.1
        || who.repository != scoped.2
        || who.repository_id != Some(scoped.3)
    {
        return Err(HubError::Unauthorized);
    }
    if who.read_only {
        return Err(HubError::Unauthorized);
    }
    if bundle.canonical_repository != format!("{}/{}", scoped.1, scoped.2) {
        return Err(HubError::Invalid(
            "signed repository binding differs".into(),
        ));
    }
    let enabled = tx
        .query_row(
            "SELECT s.enabled FROM hub_validation_summary_config s JOIN hub_repositories r ON
                 r.id=s.repository AND r.connection=s.connection WHERE s.repository=?1",
            params![repository],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if enabled != Some(1) {
        return Err(HubError::Invalid(
            "Hub validation summary is not enabled".into(),
        ));
    }
    tx.execute("INSERT INTO hub_validation_proposals(repository,proposal,generation) VALUES(?1,?2,0) ON CONFLICT DO NOTHING",params![repository,proposal])?;
    if let Some(receipt) = tx.query_row("SELECT generation,bundle_sha256,received_at,received_by FROM hub_validation_evidence WHERE
                 repository=?1 AND proposal=?2 AND bundle_sha256=?3",params![repository,proposal,bundle.digest], |row| Ok(ValidationEvidenceReceipt { repository: repository.into(), proposal, generation: row.get::<_, i64>(0)?, bundle_sha256: row.get::<_, String>(1)?, received_at: row.get::<_, i64>(2)?, received_by: row.get::<_, String>(3)?, state: "received_unverified".into() })).optional()? { tx.commit()?;
return Ok(receipt); }
    tx.execute("UPDATE hub_validation_proposals SET generation=generation+1 WHERE repository=?1 AND proposal=?2",params![repository,proposal])?;
    let generation = tx
        .query_row(
            "SELECT generation FROM hub_validation_proposals WHERE repository=?1 AND proposal=?2",
            params![repository, proposal],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| HubError::Unavailable("validation generation missing".into()))?;
    tx.execute("INSERT INTO
                 hub_validation_evidence(repository,proposal,generation,bundle_sha256,received_at,received_by)
                 VALUES(?1,?2,?3,?4,?5,?6)",params![repository,proposal,generation,bundle.digest,now,who.owner.id])?;
    let receipt = ValidationEvidenceReceipt {
        repository: repository.into(),
        proposal,
        generation,
        bundle_sha256: bundle.digest.clone(),
        received_at: now,
        received_by: who.owner.id,
        state: "received_unverified".into(),
    };
    tx.commit()?;
    Ok(receipt)
}
pub(super) fn history(
    store: &SqliteStore,
    token: &str,
    repository: &str,
    proposal: i64,
    after: i64,
    limit: usize,
    now: i64,
) -> Result<ValidationEvidencePage> {
    validate_page(proposal, after, limit)?;
    let mut conn = store.conn();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let who = work_who(&tx, token, now)?;
    let scoped = tx
        .query_row(
            "SELECT c.provider,r.host,r.full_name,r.repository_id FROM hub_repositories r JOIN
                 hub_connections c ON c.id=r.connection WHERE r.id=?1",
            params![repository],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or(HubError::Unauthorized)?;
    if who.owner.identity.provider != scoped.0
        || who.owner.identity.host != scoped.1
        || who.repository != scoped.2
        || who.repository_id != Some(scoped.3)
    {
        return Err(HubError::Unauthorized);
    }
    let entries = {
        let mut statement = tx.prepare("SELECT generation,bundle_sha256,received_at,received_by FROM hub_validation_evidence WHERE
                 repository=?1 AND proposal=?2 AND generation>?3 ORDER BY generation LIMIT ?4")?;
        statement
            .query_map(
                params![repository, proposal, after, (limit as i64 + 1)],
                |row| {
                    Ok(ValidationEvidenceReceipt {
                        repository: repository.into(),
                        proposal,
                        generation: row.get::<_, i64>(0)?,
                        bundle_sha256: row.get::<_, String>(1)?,
                        received_at: row.get::<_, i64>(2)?,
                        received_by: row.get::<_, String>(3)?,
                        state: "received_unverified".into(),
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    tx.commit()?;
    Ok(page(entries, limit))
}

pub(super) fn snapshot(
    store: &SqliteStore,
    repository: &str,
    proposal: i64,
    limit: usize,
) -> Result<crate::hub::validation_evidence::ValidationSnapshot> {
    use crate::hub::validation_evidence::{ValidationSnapshot, validate_snapshot_limit};
    validate_snapshot_limit(proposal, limit)?;
    let mut conn = store.conn();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let config = tx
        .query_row(
            "SELECT connection,enabled FROM hub_validation_summary_config WHERE repository=?1",
            params![repository],
            |row| {
                Ok(ValidationSummaryConfig {
                    repository: repository.into(),
                    connection: row.get(0)?,
                    enabled: row.get::<_, i64>(1)? != 0,
                })
            },
        )
        .optional()?;
    let generation = tx
        .query_row(
            "SELECT generation FROM hub_validation_proposals WHERE repository=?1 AND proposal=?2",
            params![repository, proposal],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    let entries = {
        let mut query = tx.prepare("SELECT generation,bundle_sha256,received_at,received_by FROM hub_validation_evidence WHERE repository=?1 AND proposal=?2 ORDER BY generation LIMIT ?3")?;
        query
            .query_map(params![repository, proposal, limit as i64 + 1], |row| {
                Ok(ValidationEvidenceReceipt {
                    repository: repository.into(),
                    proposal,
                    generation: row.get(0)?,
                    bundle_sha256: row.get(1)?,
                    received_at: row.get(2)?,
                    received_by: row.get(3)?,
                    state: "received_unverified".into(),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    if entries.len() > limit {
        return Err(HubError::Invalid(
            "validation snapshot exceeds receipt limit".into(),
        ));
    }
    crate::hub::validation_evidence::validate_snapshot_inventory(generation, &entries)?;
    tx.commit()?;
    Ok(ValidationSnapshot {
        config,
        generation,
        entries,
    })
}

pub(super) fn publication(
    store: &SqliteStore,
    repository: &str,
    proposal: i64,
) -> Result<Option<crate::hub::validation_evidence::ValidationPublication>> {
    crate::hub::validation_evidence::validate_snapshot_limit(proposal, 1)?;
    let conn = store.conn();
    let state = conn
        .query_row(
            "SELECT state FROM hub_validation_publication WHERE repository=?1 AND proposal=?2",
            params![repository, proposal],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    state
        .map(|s| {
            serde_json::from_str(&s)
                .map_err(|_| HubError::Unavailable("validation publication journal corrupt".into()))
        })
        .transpose()
}

pub(super) fn compare_publication(
    store: &SqliteStore,
    repository: &str,
    proposal: i64,
    expected_revision: i64,
    next: &crate::hub::validation_evidence::ValidationPublication,
) -> Result<bool> {
    use crate::hub::validation_evidence::{
        ValidationPublication, publication_transition, validate_snapshot_limit,
    };
    validate_snapshot_limit(proposal, 1)?;
    let mut conn = store.conn();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let state = tx.query_row("SELECT revision,state FROM hub_validation_publication WHERE repository=?1 AND proposal=?2", params![repository,proposal], |row| Ok((row.get::<_,i64>(0)?, row.get::<_,String>(1)?))).optional()?;
    if state.as_ref().map_or(0, |s| s.0) != expected_revision {
        return Ok(false);
    }
    let old: Option<ValidationPublication> = state
        .map(|s| {
            serde_json::from_str(&s.1)
                .map_err(|_| HubError::Unavailable("validation publication journal corrupt".into()))
        })
        .transpose()?;
    let fence = publication_transition(old.as_ref(), expected_revision, next)?;
    if fence {
        let config = tx.query_row("SELECT s.connection,s.enabled FROM hub_validation_summary_config s JOIN hub_repositories r ON r.id=s.repository AND r.connection=s.connection WHERE s.repository=?1",params![repository], |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?))).optional()?;
        let generation = tx.query_row("SELECT generation FROM hub_validation_proposals WHERE repository=?1 AND proposal=?2",params![repository,proposal],|row|row.get::<_,i64>(0)).optional()?.unwrap_or(0);
        if config != Some((next.connection.clone(), 1)) || generation != next.evidence_generation {
            return Ok(false);
        }
    }
    let bytes = serde_json::to_string(next)
        .map_err(|_| HubError::Invalid("invalid validation publication".into()))?;
    tx.execute("INSERT INTO hub_validation_publication(repository,proposal,revision,state) VALUES(?1,?2,?3,?4) ON CONFLICT(repository,proposal) DO UPDATE SET revision=excluded.revision,state=excluded.state",params![repository,proposal,next.revision,bytes])?;
    tx.commit()?;
    Ok(true)
}
