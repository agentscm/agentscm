use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::{
    ExportRecord, ExportState, Mirror, Ownership, read_record, record_bytes, sql_sequence,
};
use crate::model::{EventKind, IntegrationEvent};
use crate::{Error, Result};

const MAX_EXTERNAL_CHECKS: i64 = 256;
const MAX_EXTERNAL_RECORD_BYTES: usize = 32 * 1024;
// Counts selected external and underlying mapping payloads, including repeated
// mappings. No partial listing escapes when this budget is exhausted.
const MAX_LOOKUP_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalCheckOutcome {
    Passed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalCheckRequest {
    pub event_sequence: u64,
    pub result_id: String,
    pub tested_oid: String,
    pub outcome: ExternalCheckOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalCheck {
    pub record_kind: String,
    pub provenance: String,
    pub result_id: String,
    pub tested_oid: String,
    pub outcome: ExternalCheckOutcome,
    pub details: Option<String>,
    pub repository: String,
    pub event_sequence: u64,
    pub revision: String,
    pub tree: String,
}

impl Mirror {
    /// Retain an operator-declared result for an exact mirrored commit. This is
    /// deliberately separate from native check evidence and admission state.
    pub fn record_external_check(
        &mut self,
        request: &ExternalCheckRequest,
    ) -> Result<ExternalCheck> {
        self.validate()?;
        validate_request(request)?;
        let _lease = crate::lease::Lease::try_exclusive(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.owner.destination.join(super::LOCK))?,
        )
        .map_err(|_| Error::Conflict("mirror export is already running".into()))?;

        let tx = self
            .journal
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut remaining = MAX_LOOKUP_BYTES;
        let mapping = mirrored_mapping(&tx, &self.owner, request.event_sequence, &mut remaining)?;
        let commit = mapping
            .commit
            .as_deref()
            .ok_or_else(|| Error::Invalid("mirrored commit is absent".into()))?;
        let tree = mapping
            .tree
            .clone()
            .ok_or_else(|| Error::Invalid("mirrored tree is absent".into()))?;
        if request.tested_oid != commit {
            return Err(Error::Conflict(
                "tested OID differs from retained mirrored commit".into(),
            ));
        }
        let record = ExternalCheck {
            record_kind: "external_check_result".into(),
            provenance: "local_operator_unverified".into(),
            result_id: request.result_id.clone(),
            tested_oid: request.tested_oid.clone(),
            outcome: request.outcome,
            details: request.details.clone(),
            repository: mapping.repository,
            event_sequence: mapping.event.sequence,
            revision: mapping.event.after.revision,
            tree,
        };
        let bytes = serde_json::to_vec(&record)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 1 {
            tx.execute_batch(
                "CREATE TABLE external_checks(
                    result_id TEXT PRIMARY KEY,
                    event_sequence INTEGER NOT NULL,
                    record BLOB NOT NULL
                 ) STRICT;
                 CREATE INDEX external_checks_event ON external_checks(event_sequence, result_id);
                 PRAGMA user_version=2;",
            )?;
        } else if version != 2 {
            return Err(Error::Invalid("unsupported export journal".into()));
        }
        if let Some((event, existing)) = retained_check(&tx, &request.result_id, &mut remaining)? {
            validate_retained_check(
                &tx,
                &self.owner,
                &request.result_id,
                event,
                &existing,
                &mut remaining,
            )?;
            if existing != record {
                return Err(Error::Conflict(
                    "external result ID already has a different record".into(),
                ));
            }
            tx.commit()?;
            return Ok(existing);
        }
        let retained: i64 =
            tx.query_row("SELECT count(*) FROM external_checks", [], |row| row.get(0))?;
        if retained >= MAX_EXTERNAL_CHECKS {
            return Err(Error::Limit(
                "external check journal is full at 256 records".into(),
            ));
        }
        tx.execute(
            "INSERT INTO external_checks(result_id,event_sequence,record) VALUES (?1,?2,?3)",
            params![
                request.result_id,
                sql_sequence(request.event_sequence)?,
                bytes
            ],
        )?;
        tx.commit()?;
        Ok(record)
    }

    pub fn external_checks(&self, event_sequence: Option<u64>) -> Result<Vec<ExternalCheck>> {
        self.validate()?;
        let event = event_sequence.map(sql_sequence).transpose()?;
        // One read transaction keeps result keys, envelopes and underlying
        // mapping reads coherent while cooperative writers advance the journal.
        let tx = self.journal.unchecked_transaction()?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 1 {
            tx.commit()?;
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        let mut remaining = MAX_LOOKUP_BYTES;
        {
            // octet_length uses the stored byte length even for TEXT keys;
            // CASE prevents an oversized key from being selected into Rust.
            let mut statement = tx.prepare(
                "SELECT CASE WHEN octet_length(result_id)<=128 THEN result_id END, event_sequence
                 FROM external_checks WHERE (?1 IS NULL OR event_sequence=?1)
                 ORDER BY result_id LIMIT 257",
            )?;
            for (index, row) in statement
                .query_map([event], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
                })?
                .enumerate()
            {
                if index == MAX_EXTERNAL_CHECKS as usize {
                    return Err(Error::Limit(
                        "external check lookup exceeds 256 records".into(),
                    ));
                }
                let (identity, event) = row?;
                let identity = identity
                    .ok_or_else(|| Error::Limit("external result key exceeds 128 bytes".into()))?;
                let (stored_event, record) = retained_check(&tx, &identity, &mut remaining)?
                    .ok_or_else(|| Error::Invalid("selected external result is absent".into()))?;
                if i64::try_from(stored_event).ok() != Some(event) {
                    return Err(Error::Invalid("external result event key changed".into()));
                }
                validate_retained_check(
                    &tx,
                    &self.owner,
                    &identity,
                    stored_event,
                    &record,
                    &mut remaining,
                )?;
                result.push(record);
            }
        }
        tx.commit()?;
        Ok(result)
    }
}

fn mirrored_mapping(
    database: &Connection,
    owner: &Ownership,
    event: u64,
    remaining: &mut usize,
) -> Result<ExportRecord> {
    let record = read_record(database, owner, event, remaining)?
        .ok_or_else(|| Error::NotFound("mirrored integration event".into()))?;
    // The common reader has already checked every owner/event/request/state
    // invariant and exact commit/tree OIDs. Results require completed mappings.
    if record.state != ExportState::Mirrored {
        return Err(Error::Conflict(
            "external result requires an exact completed owned mapping".into(),
        ));
    }
    Ok(record)
}

fn retained_check(
    database: &Connection,
    identity: &str,
    remaining: &mut usize,
) -> Result<Option<(u64, ExternalCheck)>> {
    let limit = (*remaining).min(MAX_EXTERNAL_RECORD_BYTES);
    let row: Option<(i64, i64, Option<Vec<u8>>)> = database
        .query_row(
            "SELECT event_sequence, length(record), CASE WHEN length(record)<=?2 THEN record END
         FROM external_checks WHERE result_id=?1",
            params![identity, limit as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(event, length, bytes)| {
        let event = u64::try_from(event)
            .map_err(|_| Error::Invalid("invalid external result event key".into()))?;
        sql_sequence(event)?;
        let bytes = record_bytes(length, bytes, MAX_EXTERNAL_RECORD_BYTES, remaining)?;
        Ok((event, serde_json::from_slice(&bytes)?))
    })
    .transpose()
}

fn validate_retained_check(
    database: &Connection,
    owner: &Ownership,
    identity: &str,
    event: u64,
    record: &ExternalCheck,
    remaining: &mut usize,
) -> Result<()> {
    validate_fields(
        &record.result_id,
        &record.tested_oid,
        record.details.as_deref(),
    )?;
    if record.record_kind != "external_check_result"
        || record.provenance != "local_operator_unverified"
        || record.result_id != identity
        || record.event_sequence != event
        || record.repository != owner.repository
    {
        return Err(Error::Invalid(
            "stored external result differs from its keys or owner".into(),
        ));
    }
    let mapping = mirrored_mapping(database, owner, event, remaining)?;
    if record.revision != mapping.event.after.revision
        || mapping.commit.as_deref() != Some(record.tested_oid.as_str())
        || mapping.tree.as_deref() != Some(record.tree.as_str())
    {
        return Err(Error::Invalid(
            "stored external result differs from its keys or mapping".into(),
        ));
    }
    Ok(())
}

// Structural consistency only: this does not authenticate or re-admit an event.
pub(super) fn validate_event(event: &IntegrationEvent, repository: &str) -> Result<()> {
    sql_sequence(event.sequence)?;
    for value in [&event.operation_id, &event.target, &event.approval_id] {
        if value.is_empty()
            || value.len() > 128
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            })
        {
            return Err(Error::Invalid("invalid retained event identifier".into()));
        }
    }
    crate::store::validate_id(&event.context_digest)?;
    crate::store::validate_id(&event.after.revision)?;
    if event.after.version == 0
        || i64::try_from(event.after.version).is_err()
        || i64::try_from(event.policy_epoch).is_err()
    {
        return Err(Error::Invalid(
            "invalid retained event version or epoch".into(),
        ));
    }
    let expected = match &event.before {
        Some(before) => {
            crate::store::validate_id(&before.revision)?;
            if before.version == 0 {
                return Err(Error::Invalid("invalid retained before version".into()));
            }
            before.version.checked_add(1)
        }
        None => Some(1),
    };
    if expected != Some(event.after.version) {
        return Err(Error::Invalid(
            "invalid retained target version transition".into(),
        ));
    }
    match (&event.change, event.change_version, event.semantics_version) {
        (None, None, 1) if event.kind == EventKind::Integrate => {}
        (Some(change), Some(version), 2) => {
            change.validate()?;
            if change.repository != repository || version == 0 || i64::try_from(version).is_err() {
                return Err(Error::Invalid(
                    "invalid retained event change binding".into(),
                ));
            }
        }
        _ => {
            return Err(Error::Invalid(
                "unsupported retained event semantics or change binding".into(),
            ));
        }
    }
    if event.kind == EventKind::Initialize && event.before.is_some() {
        return Err(Error::Invalid(
            "initialization event has a prior target".into(),
        ));
    }
    Ok(())
}

fn validate_request(request: &ExternalCheckRequest) -> Result<()> {
    sql_sequence(request.event_sequence)?;
    validate_fields(
        &request.result_id,
        &request.tested_oid,
        request.details.as_deref(),
    )
}

fn validate_fields(result_id: &str, tested_oid: &str, details: Option<&str>) -> Result<()> {
    super::git::validate_oid(tested_oid)?;
    if result_id.is_empty()
        || result_id.len() > 128
        || !result_id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return Err(Error::Invalid("invalid external result identity".into()));
    }
    if details.is_some_and(|details| details.len() > 4096 || details.contains('\0')) {
        return Err(Error::Invalid("invalid external check details".into()));
    }
    Ok(())
}
