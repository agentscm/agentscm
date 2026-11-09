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
