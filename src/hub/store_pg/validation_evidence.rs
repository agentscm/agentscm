use super::{PgStore, work_who};
use crate::hub::validation_evidence::{
    DurableValidationBundle, ValidationEvidencePage, ValidationEvidenceReceipt,
    ValidationSummaryConfig, page, validate_page,
};
use crate::hub::{HubError, Result};

fn authorize(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    repository: &str,
    now: i64,
) -> Result<(super::WorkWho, String)> {
    let who = work_who(tx, token, now)?;
    let row = tx
        .query_opt(
            "SELECT c.provider,r.host,r.full_name,r.repository_id FROM hub_repositories r
         JOIN hub_connections c ON c.id=r.connection WHERE r.id=$1 FOR SHARE OF r,c",
            &[&repository],
        )?
        .ok_or(HubError::Unauthorized)?;
    let provider: String = row.get(0);
    let host: String = row.get(1);
    let full_name: String = row.get(2);
    let repository_id: i64 = row.get(3);
    if who.owner.identity.provider != provider
        || who.owner.identity.host != host
        || who.repository != full_name
        || who.repository_id != Some(repository_id)
    {
        return Err(HubError::Unauthorized);
    }
    Ok((who, format!("{host}/{full_name}")))
}

fn receipt(row: &postgres::Row, repository: &str, proposal: i64) -> ValidationEvidenceReceipt {
    ValidationEvidenceReceipt {
        repository: repository.into(),
        proposal,
        generation: row.get(0),
        bundle_sha256: row.get(1),
        received_at: row.get(2),
        received_by: row.get(3),
        state: "received_unverified".into(),
    }
}

pub(super) fn configure(store: &PgStore, config: &ValidationSummaryConfig) -> Result<()> {
    store.run(false, |client| {
        let mut tx = client.transaction()?;
        super::lock_repository(&mut tx, &config.repository)?;
        let matched = tx.query_opt(
            "SELECT 1 FROM hub_repositories WHERE id=$1 AND connection=$2",
            &[&config.repository, &config.connection],
        )?;
        if matched.is_none() {
            return Err(HubError::Invalid("summary connection does not own repository".into()));
        }
        tx.execute(
            "INSERT INTO hub_validation_summary_config(repository,connection,enabled) VALUES($1,$2,$3)
             ON CONFLICT(repository) DO UPDATE SET connection=excluded.connection,enabled=excluded.enabled",
            &[&config.repository, &config.connection, &(if config.enabled { 1_i64 } else { 0_i64 })],
        )?;
        tx.commit()?;
        Ok(())
    })
}

pub(super) fn config(store: &PgStore, repository: &str) -> Result<Option<ValidationSummaryConfig>> {
    store.run(false, |client| {
        Ok(client
            .query_opt(
                "SELECT connection,enabled FROM hub_validation_summary_config WHERE repository=$1",
                &[&repository],
            )?
            .map(|row| ValidationSummaryConfig {
                repository: repository.into(),
                connection: row.get(0),
                enabled: row.get::<_, i64>(1) != 0,
            }))
    })
}

pub(super) fn receive(
    store: &PgStore,
    token: &str,
    proposal: i64,
    bundle: &DurableValidationBundle,
    now: i64,
) -> Result<ValidationEvidenceReceipt> {
    validate_page(proposal, 0, 1)?;
    let repository = bundle.repository.as_str();
    store.run(false, |client| {
        let mut tx = client.transaction()?;
        super::lock_repository(&mut tx, repository)?;
        let (who, canonical) = authorize(&mut tx, token, repository, now)?;
        if who.read_only { return Err(HubError::Unauthorized); }
        if bundle.canonical_repository != canonical {
            return Err(HubError::Invalid("signed repository binding differs".into()));
        }
        let enabled = tx.query_opt(
            "SELECT s.enabled FROM hub_validation_summary_config s JOIN hub_repositories r
             ON r.id=s.repository AND r.connection=s.connection WHERE s.repository=$1 FOR SHARE OF s",
            &[&repository],
        )?.map(|row| row.get::<_, i64>(0));
        if enabled != Some(1) {
            return Err(HubError::Invalid("Hub validation summary is not enabled".into()));
        }
        tx.execute(
            "INSERT INTO hub_validation_proposals(repository,proposal,generation) VALUES($1,$2,0)
             ON CONFLICT DO NOTHING", &[&repository, &proposal],
        )?;
        // Serializes receipt generations and duplicate detection across Hub processes.
        tx.query_one(
            "SELECT generation FROM hub_validation_proposals WHERE repository=$1 AND proposal=$2 FOR UPDATE",
            &[&repository, &proposal],
        )?;
        if let Some(row) = tx.query_opt(
            "SELECT generation,bundle_sha256,received_at,received_by FROM hub_validation_evidence
             WHERE repository=$1 AND proposal=$2 AND bundle_sha256=$3",
            &[&repository, &proposal, &bundle.digest],
        )? {
            let result = receipt(&row, repository, proposal);
            tx.commit()?;
            return Ok(result);
        }
        let generation: i64 = tx.query_one(
            "UPDATE hub_validation_proposals SET generation=generation+1
             WHERE repository=$1 AND proposal=$2 RETURNING generation", &[&repository, &proposal],
        )?.get(0);
        tx.execute(
            "INSERT INTO hub_validation_evidence
             (repository,proposal,generation,bundle_sha256,received_at,received_by)
             VALUES($1,$2,$3,$4,$5,$6)",
            &[&repository, &proposal, &generation, &bundle.digest, &now, &who.owner.id],
        )?;
        let result = ValidationEvidenceReceipt {
            repository: repository.into(), proposal, generation,
            bundle_sha256: bundle.digest.clone(), received_at: now, received_by: who.owner.id,
            state: "received_unverified".into(),
        };
        tx.commit()?;
        Ok(result)
    })
}

pub(super) fn history(
    store: &PgStore,
    token: &str,
    repository: &str,
    proposal: i64,
    after: i64,
    limit: usize,
    now: i64,
) -> Result<ValidationEvidencePage> {
    validate_page(proposal, after, limit)?;
    store.run(false, |client| {
        let mut tx = client.transaction()?;
        authorize(&mut tx, token, repository, now)?;
        let entries = tx.query(
            "SELECT generation,bundle_sha256,received_at,received_by FROM hub_validation_evidence
             WHERE repository=$1 AND proposal=$2 AND generation>$3 ORDER BY generation LIMIT $4",
            &[&repository, &proposal, &after, &(limit as i64 + 1)],
        )?.iter().map(|row| receipt(row, repository, proposal)).collect();
        tx.commit()?;
        Ok(page(entries, limit))
    })
}

pub(super) fn snapshot(
    store: &PgStore,
    repository: &str,
    proposal: i64,
    limit: usize,
) -> Result<crate::hub::validation_evidence::ValidationSnapshot> {
    use crate::hub::validation_evidence::{ValidationSnapshot, validate_snapshot_limit};
    validate_snapshot_limit(proposal, limit)?;
    store.run(false, |client| {
        let mut tx = client.transaction()?;
        super::lock_repository(&mut tx, repository)?;
        let config = tx.query_opt("SELECT connection,enabled FROM hub_validation_summary_config WHERE repository=$1", &[&repository])?.map(|row|ValidationSummaryConfig {repository:repository.into(),connection:row.get(0),enabled:row.get::<_,i64>(1)!=0});
        let generation = tx.query_opt("SELECT generation FROM hub_validation_proposals WHERE repository=$1 AND proposal=$2", &[&repository,&proposal])?.map_or(0,|row|row.get::<_,i64>(0));
        let entries = tx.query("SELECT generation,bundle_sha256,received_at,received_by FROM hub_validation_evidence WHERE repository=$1 AND proposal=$2 ORDER BY generation LIMIT $3", &[&repository,&proposal,&(limit as i64+1)])?.iter().map(|row|receipt(row, repository, proposal)).collect::<Vec<_>>();
        if entries.len()>limit {return Err(HubError::Invalid("validation snapshot exceeds receipt limit".into()));}
        crate::hub::validation_evidence::validate_snapshot_inventory(generation, &entries)?;
        tx.commit()?;
        Ok(ValidationSnapshot {config,generation,entries})
    })
}

pub(super) fn publication(
    store: &PgStore,
    repository: &str,
    proposal: i64,
) -> Result<Option<crate::hub::validation_evidence::ValidationPublication>> {
    crate::hub::validation_evidence::validate_snapshot_limit(proposal, 1)?;
    store.run(false, |client| {
        client
            .query_opt(
                "SELECT state FROM hub_validation_publication WHERE repository=$1 AND proposal=$2",
                &[&repository, &proposal],
            )?
            .map(|row| {
                serde_json::from_str(&row.get::<_, String>(0)).map_err(|_| {
                    HubError::Unavailable("validation publication journal corrupt".into())
                })
            })
            .transpose()
    })
}

pub(super) fn compare_publication(
    store: &PgStore,
    repository: &str,
    proposal: i64,
    expected_revision: i64,
    next: &crate::hub::validation_evidence::ValidationPublication,
) -> Result<bool> {
    use crate::hub::validation_evidence::{
        ValidationPublication, publication_transition, validate_snapshot_limit,
    };
    validate_snapshot_limit(proposal, 1)?;
    store.run(false, |client| {
        let mut tx = client.transaction()?;
        super::lock_repository(&mut tx, repository)?;
        let state = tx.query_opt("SELECT revision,state FROM hub_validation_publication WHERE repository=$1 AND proposal=$2", &[&repository,&proposal])?;
        if state.as_ref().map_or(0,|row|row.get::<_,i64>(0)) != expected_revision {return Ok(false);}
        let old: Option<ValidationPublication> = state.map(|row|serde_json::from_str(&row.get::<_,String>(1)).map_err(|_|HubError::Unavailable("validation publication journal corrupt".into()))).transpose()?;
        let fence=publication_transition(old.as_ref(),expected_revision,next)?;
        if fence {
            let config=tx.query_opt("SELECT s.connection,s.enabled FROM hub_validation_summary_config s JOIN hub_repositories r ON r.id=s.repository AND r.connection=s.connection WHERE s.repository=$1 FOR SHARE OF s,r", &[&repository])?.map(|row|(row.get::<_,String>(0),row.get::<_,i64>(1)));
            let generation=tx.query_opt("SELECT generation FROM hub_validation_proposals WHERE repository=$1 AND proposal=$2 FOR UPDATE", &[&repository,&proposal])?.map_or(0,|row|row.get::<_,i64>(0));
            if config!=Some((next.connection.clone(),1)) || generation!=next.evidence_generation {return Ok(false);}
        }
        let bytes=serde_json::to_string(next).map_err(|_|HubError::Invalid("invalid validation publication".into()))?;
        tx.execute("INSERT INTO hub_validation_publication(repository,proposal,revision,state) VALUES($1,$2,$3,$4) ON CONFLICT(repository,proposal) DO UPDATE SET revision=excluded.revision,state=excluded.state", &[&repository,&proposal,&next.revision,&bytes])?;
        tx.commit()?;
        Ok(true)
    })
}
