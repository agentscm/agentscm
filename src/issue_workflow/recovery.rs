//! Read-only recovery validation of the exact unshipped AGIW/schema-1 journal.
//!
//! This file is a child of `claims` so recovery reuses the journal's canonical
//! schema and typed receipt readers. It never opens an IssueJournal, takes a
//! second shared lease, changes pragmas, initializes state, or grants a fence.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::{Error, Result};

use super::{
    APPLICATION_ID, MAX_ID, MAX_JSON, MAX_ROWS, SCHEMA, TABLES, normalize_ddl, read_claim,
    read_intake, read_operation, validate_id,
};

/// Retained structural binding matched to the caller's explicit expectations.
/// This does not authenticate the configuration source or prove an old writer
/// was disabled. The recovery operator must still reconcile external workers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryBinding {
    pub native_repository: String,
    pub installation_id: String,
}

/// Discover only the native journal's exact namespace. An absent namespace is
/// optional; an empty, partial, linked or extended namespace is unsupported.
/// DELETE mode has no retained companions in this quiesced profile. In
/// particular, neither an orphan WAL/SHM nor a hot rollback journal is adopted.
pub fn database_path(data: &Path) -> Result<Option<PathBuf>> {
    let directory = data.join("issue-workflow");
    match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(Error::Invalid("invalid issue recovery namespace".into())),
    }
    let mut main = None;
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if entry.file_name() != "state.sqlite"
            || !metadata.is_file()
            || metadata.file_type().is_symlink()
        {
            return Err(Error::Invalid(
                "unsupported issue recovery namespace entry".into(),
            ));
        }
        main = Some(entry.path());
    }
    let path = main.ok_or_else(|| Error::Invalid("incomplete issue recovery namespace".into()))?;
    require_delete_header(&path)?;
    Ok(Some(path))
}

fn require_delete_header(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid("invalid issue recovery database".into()));
    }
    let mut header = [0u8; 20];
    File::open(path)?.read_exact(&mut header)?;
    // These persistent header bytes remain WAL (2/2) even if an immutable
    // SQLite connection presents a rollback-mode view of a checkpointed file.
    if &header[..16] != b"SQLite format 3\0" || header[18..20] != [1, 1] {
        return Err(Error::Invalid(
            "issue recovery requires DELETE journal mode".into(),
        ));
    }
    Ok(())
}

/// Validate a quiesced file or sealed recovery copy using one read snapshot.
/// The caller owns the existing exclusive recovery lease, SQLite checkpoint /
/// locking procedure, generic integrity check, and exact byte inventory.
/// The configured installation ID must come from the trusted caller, never
/// from this journal or an untrusted request. The configured recovery APIs supply
/// it explicitly; legacy entrypoints retain their default namespace refusal.
/// This validator reads every typed record; SQLite identity alone is not enough.
/// Only the real writer's DELETE/FULL profile is supported; WAL is refused.
pub fn validate_recovery(
    db: &Connection,
    native_repository: &str,
    configured_installation_id: &str,
) -> Result<RecoveryBinding> {
    super::super::validate_native_repository(native_repository)?;
    validate_id(configured_installation_id)?;
    let path = db
        .path()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| Error::Invalid("issue recovery requires a file-backed journal".into()))?;
    require_delete_header(Path::new(path))?;
    let tx = db.unchecked_transaction()?;
    let application: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let schema: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let mode: String = tx.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    let synchronous: i64 = tx.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    if application != APPLICATION_ID || schema != SCHEMA {
        return Err(Error::Invalid(
            "unsupported issue recovery SQLite identity/schema".into(),
        ));
    }
    // synchronous is a property of this connection, not historical durability
    // evidence. The actual writer separately sets and checks FULL and FK=ON.
    if !mode.eq_ignore_ascii_case("delete") || synchronous != 2 {
        return Err(Error::Invalid("issue recovery requires DELETE/FULL".into()));
    }
    validate_schema(&tx)?;
    let binding = read_binding(&tx, native_repository, configured_installation_id)?;
    validate_bounds(&tx)?;
    if tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
        return Err(Error::Invalid(
            "issue recovery foreign key violation".into(),
        ));
    }
    // Aggregate and byte bounds precede every key allocation and typed reader.
    // Existing readers validate scalar/JSON equality, native and installation
    // scope, canonical keys, receipt checksums, transitions and required links.
    {
        let mut statement = tx.prepare("SELECT key FROM intakes")?;
        let keys = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for key in keys {
            if read_intake(&tx, &key, native_repository, configured_installation_id)?.is_none() {
                return Err(Error::Invalid("missing issue recovery intake".into()));
            }
        }
    }
    {
        let mut statement = tx.prepare("SELECT issue_key FROM claims")?;
        let keys = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for key in keys {
            if read_claim(&tx, &key, native_repository)?.is_none() {
                return Err(Error::Invalid("missing issue recovery claim".into()));
            }
        }
    }
    {
        let mut statement = tx.prepare("SELECT id FROM operations")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in ids {
            if read_operation(&tx, &id, native_repository, configured_installation_id)?.is_none() {
                return Err(Error::Invalid("missing issue recovery operation".into()));
            }
        }
    }
    tx.commit()?;
    Ok(binding)
}

fn validate_schema(db: &Connection) -> Result<()> {
    let objects: i64 = db.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name NOT GLOB 'sqlite_*'",
        [],
        |row| row.get(0),
    )?;
    if objects != TABLES.len() as i64 {
        return Err(Error::Invalid(
            "issue recovery schema contract mismatch".into(),
        ));
    }
    for (name, canonical) in TABLES {
        // Inspect length before fetching potentially damaged sqlite_master SQL.
        let shape: Option<(String, Option<i64>)> = db
            .query_row(
                "SELECT type,length(CAST(sql AS BLOB)) FROM sqlite_master WHERE name=?1",
                [name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if !shape.is_some_and(|(kind, bytes)| {
            kind == "table" && bytes.is_some_and(|bytes| bytes > 0 && bytes <= MAX_JSON as i64)
        }) {
            return Err(Error::Invalid(
                "issue recovery schema contract mismatch".into(),
            ));
        }
        let ddl: String = db.query_row(
            "SELECT sql FROM sqlite_master WHERE name=?1",
            [name],
            |row| row.get(0),
        )?;
        if normalize_ddl(&ddl) != normalize_ddl(canonical) {
            return Err(Error::Invalid(
                "issue recovery schema contract mismatch".into(),
            ));
        }
    }
    Ok(())
}

fn read_binding(
    db: &Connection,
    native_repository: &str,
    configured_installation_id: &str,
) -> Result<RecoveryBinding> {
    let count: i64 = db.query_row("SELECT count(*) FROM metadata", [], |row| row.get(0))?;
    let bounded: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE singleton=1 AND typeof(native_repository)='text' AND length(CAST(native_repository AS BLOB))=32 AND typeof(installation_id)='text' AND length(CAST(installation_id AS BLOB)) BETWEEN 1 AND ?1)",
        [MAX_ID as i64], |row| row.get(0),
    )?;
    if count != 1 || !bounded {
        return Err(Error::Invalid("invalid issue recovery metadata".into()));
    }
    let (native, installation): (String, String) = db.query_row(
        "SELECT native_repository,installation_id FROM metadata WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if native != native_repository {
        return Err(Error::Invalid(
            "issue recovery native repository mismatch".into(),
        ));
    }
    validate_id(&installation)?;
    if installation != configured_installation_id {
        return Err(Error::Invalid(
            "issue recovery configured installation mismatch".into(),
        ));
    }
    Ok(RecoveryBinding {
        native_repository: native,
        installation_id: installation,
    })
}

fn validate_bounds(tx: &Transaction<'_>) -> Result<()> {
    let rows: i64 = tx.query_row(
        "SELECT (SELECT count(*) FROM intakes)+(SELECT count(*) FROM claims)+(SELECT count(*) FROM operations)",
        [], |row| row.get(0),
    )?;
    if rows > MAX_ROWS {
        return Err(Error::Invalid(
            "issue recovery aggregate row cap exceeded".into(),
        ));
    }
    // Fixed queries for the three actual relations; no schema discovery or
    // unbounded JSON/scalar fetch occurs before these byte-length checks.
    let intake_bad: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM intakes WHERE length(CAST(key AS BLOB))!=64 OR length(CAST(request_digest AS BLOB))!=64 OR generation<=0 OR generation>=9223372036854775807 OR length(CAST(create_state AS BLOB))>?1 OR assessment_conflicted NOT IN (0,1) OR (selected_issue_key IS NOT NULL AND length(CAST(selected_issue_key AS BLOB)) NOT BETWEEN 1 AND ?1) OR (create_operation IS NOT NULL AND length(CAST(create_operation AS BLOB)) NOT BETWEEN 1 AND ?1) OR (claimed_issue_key IS NOT NULL AND length(CAST(claimed_issue_key AS BLOB)) NOT BETWEEN 1 AND ?1) OR typeof(record_json)!='blob' OR length(record_json) NOT BETWEEN 1 AND ?2)",
        params![MAX_ID as i64, MAX_JSON as i64], |row| row.get(0),
    )?;
    let claim_bad: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM claims WHERE length(CAST(issue_key AS BLOB)) NOT BETWEEN 1 AND ?1 OR generation<=0 OR generation>=9223372036854775807 OR (worker_id IS NOT NULL AND length(CAST(worker_id AS BLOB)) NOT BETWEEN 1 AND ?1) OR (progress_digest IS NOT NULL AND length(CAST(progress_digest AS BLOB))!=64) OR typeof(record_json)!='blob' OR length(record_json) NOT BETWEEN 1 AND ?2)",
        params![MAX_ID as i64, MAX_JSON as i64], |row| row.get(0),
    )?;
    let operation_bad: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE length(CAST(id AS BLOB)) NOT BETWEEN 1 AND ?1 OR length(CAST(kind AS BLOB)) NOT BETWEEN 1 AND ?1 OR (intake_key IS NOT NULL AND length(CAST(intake_key AS BLOB))!=64) OR (issue_key IS NOT NULL AND length(CAST(issue_key AS BLOB)) NOT BETWEEN 1 AND ?1) OR (expected_generation IS NOT NULL AND (expected_generation<=0 OR expected_generation>=9223372036854775807)) OR (expected_intake_generation IS NOT NULL AND (expected_intake_generation<=0 OR expected_intake_generation>=9223372036854775807)) OR length(CAST(request_digest AS BLOB))!=64 OR length(CAST(outcome_digest AS BLOB))!=64 OR (selected_change_repository IS NOT NULL AND length(CAST(selected_change_repository AS BLOB))!=32) OR (selected_change_id IS NOT NULL AND length(CAST(selected_change_id AS BLOB)) NOT BETWEEN 1 AND ?1) OR typeof(outcome_json)!='blob' OR length(outcome_json) NOT BETWEEN 1 AND ?2)",
        params![MAX_ID as i64, MAX_JSON as i64], |row| row.get(0),
    )?;
    if intake_bad || claim_bad || operation_bad {
        return Err(Error::Invalid("issue recovery row bounds exceeded".into()));
    }
    Ok(())
}
