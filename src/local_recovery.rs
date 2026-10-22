//! Offline recovery for a physically isolated, trusted local operator.
//! Advisory leases cover this binary's local handles, not older binaries or editors.
use crate::lease::Lease;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::{EncodedObject, EncodedObjectReader, MAX_OBJECT_BYTES, SourceReader};
use crate::{Error, Result};

pub(crate) const WORKSPACE_METADATA_DIRECTORY: &str = ".agentscm-workspace";
const LOCK: &str = ".agentscm-local-access.lock";
const QUARANTINE: &str = ".agentscm-recovery/quarantine.json";
const MANIFEST: &str = "recovery-manifest.json";
const COMPLETE: &str = ".agentscm-recovery/restore-complete.json";
const INSTANCE: &str = ".agentscm-recovery/instance.json";
const MAX_ENTRIES: usize = 100_000;
const MAX_FILE: u64 = 256 * 1024 * 1024;
const MAX_TOTAL: u64 = 1024 * 1024 * 1024;
const MAX_MANIFEST: u64 = 32 * 1024 * 1024;

#[cfg(all(test, unix))]
mod crash_tests;

/// A lifetime lease. Clones retain the same open file description and lease.
#[derive(Clone, Debug)]
pub struct AccessGuard {
    _file: Arc<Lease>,
}

impl AccessGuard {
    pub fn shared(root: &Path) -> Result<Self> {
        Self::acquire(root, false, false, false)
    }

    /// Read-only callers require an already provisioned control file. Missing
    /// state fails closed; this opener never creates a lock or data directory.
    pub fn shared_existing(root: &Path) -> Result<Self> {
        Self::acquire_existing(root, false, false, false, true)
    }

    pub fn exclusive(root: &Path) -> Result<Self> {
        Self::acquire(root, true, false, false)
    }

    pub(crate) fn workspace(metadata: &Path) -> Result<Self> {
        Self::acquire(metadata, false, false, true)
    }

    /// Existing workspace lock only; never creates one.
    pub(crate) fn workspace_existing(metadata: &Path) -> Result<Self> {
        Self::acquire_existing(metadata, false, false, true, true)
    }

    fn acquire(root: &Path, exclusive: bool, quarantined: bool, workspace: bool) -> Result<Self> {
        Self::acquire_existing(root, exclusive, quarantined, workspace, false)
    }

    fn acquire_existing(
        root: &Path,
        exclusive: bool,
        quarantined: bool,
        workspace: bool,
        existing_only: bool,
    ) -> Result<Self> {
        require_directory(root)?;
        let root = fs::canonicalize(root)?;
        if !quarantined {
            check_quarantine(&root, workspace)?;
        }
        let file = if existing_only {
            require_regular(&root.join(LOCK))?;
            File::open(root.join(LOCK))?
        } else {
            new_options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(root.join(LOCK))?
        };
        let lease = if exclusive {
            Lease::try_exclusive(file)
        } else {
            Lease::try_shared(file)
        }
        .map_err(|error| {
            Error::Conflict(format!(
                "local access is active at {}: {error}",
                root.display()
            ))
        })?;
        if !quarantined {
            check_quarantine(&root, workspace)?;
        }
        Ok(Self {
            _file: Arc::new(lease),
        })
    }
}

pub(crate) fn check_before_create(path: &Path, workspace: bool) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    let mut resolved = loop {
        match fs::canonicalize(existing) {
            Ok(path) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // An unresolved parent traversal can make mkdir_all create a
                // temporary prefix outside the final canonical ancestry.
                let name = existing.file_name().ok_or_else(|| {
                    Error::Invalid(
                        "cannot resolve missing recovery path with parent traversal".into(),
                    )
                })?;
                missing.push(name.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    Error::Invalid("recovery path has no existing ancestor".into())
                })?;
            }
            Err(error) => return Err(error.into()),
        }
    };
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    check_quarantine(&resolved, workspace)
}

fn check_quarantine(root: &Path, workspace: bool) -> Result<()> {
    // Kind is provided by the managed caller, never inferred from user filenames.
    let skip = if workspace { 2 } else { 0 };
    for ancestor in root.ancestors().skip(skip) {
        match fs::symlink_metadata(ancestor.join(QUARANTINE)) {
            Ok(_) => {
                return Err(Error::Conflict(
                    "restored state requires explicit recovery activation".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRoot {
    pub original: String,
    pub destination: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InventoryEntry {
    pub path: String,
    pub directory: bool,
    pub bytes: u64,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryManifest {
    pub version: u32,
    pub profile: String,
    pub repository: String,
    pub policy_epoch: u64,
    pub roots: Vec<RecoveryRoot>,
    pub entries: Vec<InventoryEntry>,
}

/// Ordinary installation configuration supplied independently of journal metadata.
/// These identifiers confer no authorization, writer fencing or readiness.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssueWorkflowRecoveryConfig {
    version: u32,
    native_repository: String,
    installation_id: String,
}

impl IssueWorkflowRecoveryConfig {
    pub fn new(native_repository: &str, installation_id: &str) -> Result<Self> {
        let value = Self {
            version: 1,
            native_repository: native_repository.into(),
            installation_id: installation_id.into(),
        };
        value.validate()?;
        Ok(value)
    }

    /// Read only a bounded regular configuration file, never a journal.
    pub fn read(path: &Path) -> Result<Self> {
        Self::from_json(&read_bounded(path, 16 * 1024)?)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 16 * 1024 {
            return Err(Error::Limit(
                "issue recovery configuration exceeds 16 KiB".into(),
            ));
        }
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }

    pub fn native_repository(&self) -> &str {
        &self.native_repository
    }

    pub fn installation_id(&self) -> &str {
        &self.installation_id
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1
            || self.native_repository.len() != 32
            || !self
                .native_repository
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.installation_id.is_empty()
            || self.installation_id.len() > 128
            || !self
                .installation_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(Error::Invalid(
                "invalid configured issue recovery identity".into(),
            ));
        }
        Ok(())
    }
}

/// All writers, editors and older binaries must already be stopped and isolated.
/// Attachments are explicit: receipts, transfer state, and the entire Git mirror.
pub fn backup(
    data: &Path,
    output: &Path,
    workspaces: &[PathBuf],
    attachments: &[PathBuf],
    quiesced: bool,
) -> Result<RecoveryManifest> {
    backup_inner(data, output, workspaces, attachments, quiesced, None)
}

pub fn backup_with_issue_config(
    data: &Path,
    output: &Path,
    workspaces: &[PathBuf],
    attachments: &[PathBuf],
    quiesced: bool,
    config: &IssueWorkflowRecoveryConfig,
) -> Result<RecoveryManifest> {
    config.validate()?;
    backup_inner(
        data,
        output,
        workspaces,
        attachments,
        quiesced,
        Some(config),
    )
}

fn backup_inner(
    data: &Path,
    output: &Path,
    workspaces: &[PathBuf],
    attachments: &[PathBuf],
    quiesced: bool,
    issue_config: Option<&IssueWorkflowRecoveryConfig>,
) -> Result<RecoveryManifest> {
    if !quiesced {
        return Err(Error::Invalid(
            "backup requires --quiesced after physical writer shutdown/access isolation".into(),
        ));
    }
    if workspaces.len().saturating_add(attachments.len()) >= 256 {
        return Err(Error::Limit(
            "backup supports at most 256 selected roots".into(),
        ));
    }
    reject_link(data)?;
    let mut roots = vec![(fs::canonicalize(data)?, "data".to_owned())];
    for (index, root) in workspaces.iter().enumerate() {
        reject_link(root)?;
        let root = fs::canonicalize(root)?;
        require_regular(&root.join(WORKSPACE_METADATA_DIRECTORY).join("state.sqlite"))?;
        roots.push((root, format!("workspaces/{index}")));
    }
    for (index, root) in attachments.iter().enumerate() {
        // Canonicalization is for ancestor comparison, never permission to follow a link.
        reject_link(root)?;
        roots.push((fs::canonicalize(root)?, format!("attachments/{index}")));
    }
    let output = new_destination(output)?;
    for (index, (root, _)) in roots.iter().enumerate() {
        reject_link(root)?;
        if output.starts_with(root) || root.starts_with(&output) {
            return Err(Error::Invalid(
                "backup destination overlaps a selected source".into(),
            ));
        }
        if roots[..index]
            .iter()
            .any(|(other, _)| root.starts_with(other) || other.starts_with(root))
        {
            return Err(Error::Invalid("selected backup roots overlap".into()));
        }
    }
    // Nonblocking acquisition makes competing backup orders fail instead of deadlock.
    let mut guards = Vec::new();
    let mut lock_roots: Vec<_> = roots
        .iter()
        .filter_map(|(root, destination)| {
            lease_root(root, destination)
                .map(|lease| lease.map(|path| (path, destination.starts_with("workspaces/"))))
                .transpose()
        })
        .collect::<Result<_>>()?;
    lock_roots.sort();
    for (root, workspace) in lock_roots {
        guards.push(AccessGuard::acquire(&root, true, false, workspace)?);
    }
    let mut databases = Vec::new();
    for (root, destination) in &roots {
        for (path, kind) in database_paths(root, destination, issue_config)? {
            let db = quiesce_database(&path)?;
            validate_database(&db, kind)?;
            databases.push((path, kind, db));
        }
    }
    let authority = &databases
        .iter()
        .find(|(path, _, _)| path == &roots[0].0.join("state.sqlite"))
        .ok_or_else(|| Error::Invalid("missing local authority database".into()))?
        .2;
    validate_authority(authority)?;
    let repository: String = authority.query_row(
        "SELECT repository_id FROM authority_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    validate_issue_authority(&repository, issue_config)?;
    let policy_epoch = authority.query_row(
        "SELECT policy_epoch FROM authority_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    // No public Engine opener or second SQLite connection under the exclusive lease.
    verify_source(&roots[0].0, authority, issue_config)?;
    let objects = Objects(roots[0].0.join("objects"));
    let source = SourceReader::new(&objects);
    for (_, kind, db) in &databases {
        validate_journal(db, *kind, &repository, &source, issue_config)?;
    }
    // Validate every source entry and aggregate bound before publishing output.
    // Later I/O failure may leave a partial directory; unsupported input does not.
    let mut inventories = Vec::new();
    let mut count = 0usize;
    let mut bytes = 0u64;
    for (source, destination) in &roots {
        let entries = scan(
            source,
            true,
            &ignored_paths(source, destination, issue_config)?,
        )?;
        count = count
            .checked_add(entries.len())
            .ok_or_else(|| Error::Limit("backup entry overflow".into()))?;
        for entry in &entries {
            bytes = bytes
                .checked_add(entry.bytes)
                .ok_or_else(|| Error::Limit("backup size overflow".into()))?;
        }
        if count > MAX_ENTRIES || bytes > MAX_TOTAL {
            return Err(Error::Limit(
                "backup exceeds 1 GiB or 100,000 entries".into(),
            ));
        }
        inventories.push(entries);
    }
    create_private_root(&output)?;
    let payload = output.join("payload");
    create_private_directory(&payload)?;
    let mut manifest = RecoveryManifest {
        version: 1,
        profile: "agentscm-trusted-local-offline-v1".into(),
        repository,
        policy_epoch,
        roots: Vec::new(),
        entries: Vec::new(),
    };
    let mut total = 0u64;
    for ((source, destination), entries) in roots.iter().zip(inventories) {
        manifest.roots.push(RecoveryRoot {
            original: path_text(source)?,
            destination: destination.clone(),
        });
        for mut entry in entries {
            let from = if entry.path.is_empty() {
                source.clone()
            } else {
                source.join(&entry.path)
            };
            entry.path = if entry.path.is_empty() {
                destination.clone()
            } else {
                format!("{destination}/{}", entry.path)
            };
            total = total
                .checked_add(entry.bytes)
                .ok_or_else(|| Error::Limit("backup size overflow".into()))?;
            if total > MAX_TOTAL || manifest.entries.len() >= MAX_ENTRIES {
                return Err(Error::Limit(
                    "backup exceeds 1 GiB or 100,000 entries".into(),
                ));
            }
            let to = payload.join(&entry.path);
            if entry.directory {
                create_private_directory(&to)?;
            } else {
                if let Some(parent) = to.parent() {
                    create_private_directory(parent)?;
                }
                copy_new(&from, &to, &entry)?;
            }
            manifest.entries.push(entry);
        }
    }
    manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
    let bytes = serde_json::to_vec(&manifest)?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(Error::Limit("backup manifest too large".into()));
    }
    write_new(&output.join(MANIFEST), &bytes)?;
    sync_directory(&output)?;
    drop(databases);
    drop(guards);
    verify_inner(&output, issue_config)
}

pub fn verify(backup: &Path) -> Result<RecoveryManifest> {
    verify_inner(backup, None)
}

pub fn verify_with_issue_config(
    backup: &Path,
    config: &IssueWorkflowRecoveryConfig,
) -> Result<RecoveryManifest> {
    config.validate()?;
    verify_inner(backup, Some(config))
}

fn verify_inner(
    backup: &Path,
    issue_config: Option<&IssueWorkflowRecoveryConfig>,
) -> Result<RecoveryManifest> {
    require_directory(backup)?;
    let manifest: RecoveryManifest =
        serde_json::from_slice(&read_bounded(&backup.join(MANIFEST), MAX_MANIFEST)?)?;
    validate_manifest(&manifest)?;
    verify_inventory(&backup.join("payload"), &manifest, false)?;
    verify_databases(&backup.join("payload"), &manifest, issue_config)?;
    let db = open_readonly(&backup.join("payload/data/state.sqlite"))?;
    verify_source(&backup.join("payload/data"), &db, issue_config)?;
    Ok(manifest)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreInstance {
    version: u32,
    id: String,
}

/// Identity of this particular restore occurrence, independent of its path.
/// It is a relocation freshness binding, not writer fencing or authentication.
pub fn restore_instance(root: &Path) -> Result<String> {
    let instance: RestoreInstance =
        serde_json::from_slice(&read_bounded(&root.join(INSTANCE), 512)?)?;
    if instance.version != 1
        || instance.id.len() != 32
        || !instance
            .id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid("invalid restore occurrence identity".into()));
    }
    Ok(instance.id)
}

/// Restore is quarantined, with the completion record written last. A failed
/// restore leaves its new owned directory for inspection; existing data is untouched.
pub fn restore(backup: &Path, destination: &Path) -> Result<RecoveryManifest> {
    restore_inner(backup, destination, None)
}

pub fn restore_with_issue_config(
    backup: &Path,
    destination: &Path,
    config: &IssueWorkflowRecoveryConfig,
) -> Result<RecoveryManifest> {
    config.validate()?;
    restore_inner(backup, destination, Some(config))
}

fn restore_inner(
    backup: &Path,
    destination: &Path,
    issue_config: Option<&IssueWorkflowRecoveryConfig>,
) -> Result<RecoveryManifest> {
    let manifest = verify_inner(backup, issue_config)?;
    let destination = new_destination(destination)?;
    let backup_root = fs::canonicalize(backup)?;
    if destination.starts_with(&backup_root) || backup_root.starts_with(&destination) {
        return Err(Error::Invalid("restore destination overlaps backup".into()));
    }
    create_private_root(&destination)?;
    create_private_directory(&destination.join(".agentscm-recovery"))?;
    write_new(
        &destination.join(QUARANTINE),
        b"{\"state\":\"reconciliation-required\"}",
    )?;
    #[cfg(all(test, unix))]
    crash_tests::boundary("restore_quarantine", &destination)?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        Error::Invalid(format!("restore instance randomness unavailable: {error}"))
    })?;
    let instance = RestoreInstance {
        version: 1,
        id: random.iter().map(|byte| format!("{byte:02x}")).collect(),
    };
    write_new(&destination.join(INSTANCE), &serde_json::to_vec(&instance)?)?;
    for entry in &manifest.entries {
        let to = destination.join(&entry.path);
        if entry.directory {
            create_private_directory(&to)?;
        } else {
            if let Some(parent) = to.parent() {
                create_private_directory(parent)?;
            }
            copy_new(&backup.join("payload").join(&entry.path), &to, entry)?;
        }
    }
    // Preserve ordinary permission bits only. No ownership, set-ID, links or ACL claims.
    for entry in manifest.entries.iter().rev() {
        set_mode(&destination.join(&entry.path), entry.mode)?;
    }
    verify_inventory(&destination, &manifest, true)?;
    verify_databases(&destination, &manifest, issue_config)?;
    let db = open_readonly(&destination.join("data/state.sqlite"))?;
    verify_source(&destination.join("data"), &db, issue_config)?;
    drop(db);
    #[cfg(all(test, unix))]
    crash_tests::boundary("restore_verified", &destination)?;
    write_new(&destination.join(COMPLETE), &serde_json::to_vec(&manifest)?)?;
    sync_directory(&destination)?;
    #[cfg(all(test, unix))]
    crash_tests::boundary("restore_complete", &destination)?;
    Ok(manifest)
}

/// An explicit trusted-operator acknowledgement, never an automatic failover.
/// Replayed approvals are revoked and contexts must be updated to the new epoch.
pub fn activate(
    destination: &Path,
    old_writer_disabled: bool,
    external_state_reconciled: bool,
) -> Result<u64> {
    activate_inner(
        destination,
        old_writer_disabled,
        external_state_reconciled,
        None,
    )
}

pub fn activate_with_issue_config(
    destination: &Path,
    old_writer_disabled: bool,
    external_state_reconciled: bool,
    config: &IssueWorkflowRecoveryConfig,
) -> Result<u64> {
    config.validate()?;
    activate_inner(
        destination,
        old_writer_disabled,
        external_state_reconciled,
        Some(config),
    )
}

fn activate_inner(
    destination: &Path,
    old_writer_disabled: bool,
    external_state_reconciled: bool,
    issue_config: Option<&IssueWorkflowRecoveryConfig>,
) -> Result<u64> {
    if !old_writer_disabled || !external_state_reconciled {
        return Err(Error::Invalid(
            "activation requires --old-writer-disabled and --external-state-reconciled".into(),
        ));
    }
    require_regular(&destination.join(QUARANTINE))?;
    restore_instance(destination)?;
    let manifest: RecoveryManifest =
        serde_json::from_slice(&read_bounded(&destination.join(COMPLETE), MAX_MANIFEST)?)?;
    validate_manifest(&manifest)?;
    let _guard = AccessGuard::acquire(&destination.join("data"), true, true, false)?;
    // This check also prevents activation of an interrupted or altered restore.
    verify_inventory(destination, &manifest, true)?;
    verify_databases(destination, &manifest, issue_config)?;
    let db = open_readonly(&destination.join("data/state.sqlite"))?;
    verify_source(&destination.join("data"), &db, issue_config)?;
    drop(db);
    let mut connection = Connection::open_with_flags(
        destination.join("data/state.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    validate_authority(&connection)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous: u64 = tx.query_row(
        "SELECT policy_epoch FROM authority_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let expected = manifest
        .policy_epoch
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or_else(|| Error::Limit("policy epoch exhausted".into()))?;
    if previous != manifest.policy_epoch {
        return Err(Error::Conflict(
            "restored policy changed during activation; restore anew and reconcile".into(),
        ));
    }
    tx.execute(
        "UPDATE approvals SET revoked=1 WHERE consumed_event IS NULL",
        [],
    )?;
    tx.execute(
        "UPDATE authority_state SET policy_epoch=?1 WHERE singleton=1",
        [expected],
    )?;
    #[cfg(all(test, unix))]
    crash_tests::boundary("activation_before_commit", destination)?;
    tx.commit()?;
    #[cfg(all(test, unix))]
    crash_tests::boundary("activation_committed", destination)?;
    drop(connection);
    fs::remove_file(destination.join(QUARANTINE))?;
    sync_directory(destination)?;
    #[cfg(all(test, unix))]
    crash_tests::boundary("activation_released", destination)?;
    Ok(expected)
}

fn validate_manifest(manifest: &RecoveryManifest) -> Result<()> {
    if manifest.version != 1
        || manifest.profile != "agentscm-trusted-local-offline-v1"
        || manifest.roots.first().map(|r| r.destination.as_str()) != Some("data")
        || manifest.entries.len() > MAX_ENTRIES
        || manifest.roots.len() > 256
    {
        return Err(Error::Invalid(
            "unsupported recovery manifest/profile".into(),
        ));
    }
    for (index, root) in manifest.roots.iter().enumerate() {
        let original = Path::new(&root.original);
        if !original.is_absolute()
            || original
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err(Error::Invalid("invalid original recovery root".into()));
        }
        if root.destination != "data" {
            let (kind, number) = root
                .destination
                .split_once('/')
                .ok_or_else(|| Error::Invalid("invalid recovery root namespace".into()))?;
            let ordinal = number
                .parse::<usize>()
                .map_err(|_| Error::Invalid("invalid recovery root index".into()))?;
            if !matches!(kind, "workspaces" | "attachments") || ordinal.to_string() != number {
                return Err(Error::Invalid("noncanonical recovery root".into()));
            }
        }
        for previous in &manifest.roots[..index] {
            if Path::new(&root.destination).starts_with(&previous.destination)
                || Path::new(&previous.destination).starts_with(&root.destination)
                || original.starts_with(&previous.original)
                || Path::new(&previous.original).starts_with(original)
            {
                return Err(Error::Invalid(
                    "duplicate or overlapping recovery roots".into(),
                ));
            }
        }
    }
    let mut previous = None;
    let mut total = 0u64;
    for entry in &manifest.entries {
        safe_relative(&entry.path)?;
        if previous.is_some_and(|p: &str| p >= entry.path.as_str())
            || entry.mode > 0o777
            || entry.bytes > MAX_FILE
            || (entry.directory && (entry.bytes != 0 || !entry.sha256.is_empty()))
            || (!entry.directory
                && (entry.sha256.len() != 64
                    || !entry
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())))
        {
            return Err(Error::Invalid(
                "invalid recovery inventory entry/order".into(),
            ));
        }
        if !manifest.roots.iter().any(|root| {
            entry.path == root.destination
                || entry.path.starts_with(&format!("{}/", root.destination))
        }) {
            return Err(Error::Invalid(
                "inventory entry outside selected roots".into(),
            ));
        }
        total = total
            .checked_add(entry.bytes)
            .ok_or_else(|| Error::Limit("inventory overflow".into()))?;
        previous = Some(&entry.path);
    }
    if total > MAX_TOTAL {
        return Err(Error::Limit("inventory exceeds 1 GiB".into()));
    }
    for root in &manifest.roots {
        safe_relative(&root.destination)?;
        if root.destination != "data"
            && !(root.destination.starts_with("workspaces/")
                || root.destination.starts_with("attachments/"))
        {
            return Err(Error::Invalid("unknown restore root".into()));
        }
    }
    Ok(())
}

fn verify_inventory(root: &Path, manifest: &RecoveryManifest, check_modes: bool) -> Result<()> {
    for entry in &manifest.entries {
        let path = root.join(&entry.path);
        reject_ancestors(root, &path)?;
        let metadata = fs::symlink_metadata(&path)?;
        if entry.directory != metadata.is_dir() || (!entry.directory && !metadata.is_file()) {
            return Err(Error::Invalid(format!(
                "inventory type mismatch: {}",
                entry.path
            )));
        }
        if check_modes && mode(&metadata) != entry.mode {
            return Err(Error::Invalid(format!(
                "restored mode mismatch: {}",
                entry.path
            )));
        }
        if !entry.directory
            && (metadata.len() != entry.bytes || hash_file(&path, MAX_FILE)? != entry.sha256)
        {
            return Err(Error::Invalid(format!(
                "backup checksum mismatch: {}",
                entry.path
            )));
        }
    }
    // Every selected tree is exact. Grouping directories outside roots are framing.
    for selected in &manifest.roots {
        let selected_root = root.join(&selected.destination);
        let mut ignored = BTreeSet::new();
        if !selected.destination.starts_with("attachments/")
            && let Some(lease) = lease_root(&selected_root, &selected.destination)?
        {
            ignored.insert(lease.join(LOCK));
        }
        let actual = scan(&selected_root, false, &ignored)?;
        let expected: BTreeSet<_> = manifest
            .entries
            .iter()
            .filter(|e| {
                e.path == selected.destination
                    || e.path.starts_with(&format!("{}/", selected.destination))
            })
            .map(|e| e.path.clone())
            .collect();
        let actual: BTreeSet<_> = actual
            .into_iter()
            .map(|e| {
                if e.path.is_empty() {
                    selected.destination.clone()
                } else {
                    format!("{}/{}", selected.destination, e.path)
                }
            })
            .collect();
        if expected != actual {
            return Err(Error::Invalid(
                "backup contains missing or additional entries".into(),
            ));
        }
    }
    Ok(())
}

fn verify_databases(
    root: &Path,
    manifest: &RecoveryManifest,
    issue_config: Option<&IssueWorkflowRecoveryConfig>,
) -> Result<()> {
    for selected in &manifest.roots {
        for (path, kind) in database_paths(
            &root.join(&selected.destination),
            &selected.destination,
            issue_config,
        )? {
            let db = open_readonly(&path)?;
            validate_database(&db, kind)?;
            let reader = Objects(root.join("data/objects"));
            validate_journal(
                &db,
                kind,
                &manifest.repository,
                &SourceReader::new(&reader),
                issue_config,
            )?;
        }
    }
    let db = open_readonly(&root.join("data/state.sqlite"))?;
    validate_authority(&db)?;
    let (repository, epoch): (String, u64) = db.query_row(
        "SELECT repository_id,policy_epoch FROM authority_state WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if repository != manifest.repository || epoch != manifest.policy_epoch {
        return Err(Error::Invalid(
            "authority and manifest identity mismatch".into(),
        ));
    }
    validate_issue_authority(&repository, issue_config)?;
    Ok(())
}

fn validate_journal(
    db: &Connection,
    kind: DbKind,
    repository: &str,
    source: &SourceReader<'_>,
    issue_config: Option<&IssueWorkflowRecoveryConfig>,
) -> Result<()> {
    let sql = if kind == DbKind::Workspace {
        let bound: String = db.query_row(
            "SELECT repository FROM workspace_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if bound != repository {
            return Err(Error::Invalid(
                "workspace belongs to another repository".into(),
            ));
        }
        Some(
            "SELECT base_revision FROM workspace_state UNION SELECT before_revision FROM operations UNION SELECT after_revision FROM operations WHERE after_revision IS NOT NULL UNION SELECT merge_left FROM operations WHERE merge_left IS NOT NULL UNION SELECT merge_revision FROM operations WHERE merge_revision IS NOT NULL",
        )
    } else if kind == DbKind::SkillWorkflow {
        Some(
            "SELECT json_extract(CAST(definition_json AS TEXT), '$.source.revision') FROM workflow_runs",
        )
    } else if kind == DbKind::Merge {
        crate::merge::validate_recovery(db, repository, source)?;
        None
    } else if kind == DbKind::GitReconcile {
        #[cfg(unix)]
        {
            crate::git_reconcile::validate_recovery(db, repository, source)?;
            None
        }
        #[cfg(not(unix))]
        {
            return Err(Error::Invalid(
                "Git reconciliation recovery is unsupported on non-Unix platforms".into(),
            ));
        }
    } else if kind == DbKind::Issue {
        let config = issue_config.ok_or_else(|| {
            Error::Invalid(
                "issue-workflow recovery is unsupported without explicit configuration".into(),
            )
        })?;
        crate::issue_workflow::recovery::validate_recovery(
            db,
            config.native_repository(),
            config.installation_id(),
        )?;
        None
    } else {
        None
    };
    if let Some(sql) = sql {
        let mut statement = db.prepare(sql)?;
        for revision in statement.query_map([], |row| row.get::<_, String>(0))? {
            source.load_revision(&revision?)?;
        }
    }
    if kind == DbKind::Workspace {
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 5 {
            let mut statement = db.prepare(
                "SELECT source_revision FROM timeline_entries
                 UNION SELECT tested_revision FROM timeline_entries WHERE tested_revision IS NOT NULL
                 UNION SELECT target_revision FROM timeline_entries WHERE target_revision IS NOT NULL",
            )?;
            for revision in statement.query_map([], |row| row.get::<_, String>(0))? {
                source.load_revision(&revision?)?;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DbKind {
    Authority,
    Workspace,
    Merge,
    Workflow,
    SkillWorkflow,
    Export,
    GitReconcile,
    Issue,
}
fn validate_database(db: &Connection, kind: DbKind) -> Result<()> {
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::Invalid(format!("SQLite integrity failed: {kind:?}")));
    }
    if db.prepare("PRAGMA foreign_key_check")?.exists([])? {
        return Err(Error::Invalid(format!(
            "SQLite foreign keys failed: {kind:?}"
        )));
    }
    let app: i64 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let supported = match kind {
        DbKind::Authority => app == 0x4153_434d && matches!(version, 8 | 9),
        DbKind::Workspace => app == 0x4153_574b && matches!(version, 2..=5),
        DbKind::Merge => (app, version) == (0x4147_4d47, 1),
        DbKind::Workflow => (app, version) == (0x4153_4746, 1),
        DbKind::SkillWorkflow => app == 0x4153_5746 && matches!(version, 1..=4),
        DbKind::Export => app == 1095190616 && matches!(version, 1 | 2),
        DbKind::GitReconcile => (app, version) == (0x4153_4752, 1),
        DbKind::Issue => return Ok(()),
    };
    if !supported {
        return Err(Error::Invalid(format!(
            "unsupported SQLite identity/schema: {kind:?}"
        )));
    }
    // Match the export reader's required relations before preserving this as a
    // typed journal. Pragmas and physical integrity alone do not prove that the
    // declared schema can be opened after recovery. Authority and skill-workflow checks
    // are shape-only; canonical task/attempt/registry payloads are validated by runtime readers.
    if kind == DbKind::Authority {
        if version == 8 {
            crate::authority::validate_v8_tables(db)?;
        } else {
            crate::authority::validate_current_tables(db)?;
        }
    }
    if kind == DbKind::SkillWorkflow {
        crate::skill_workflow::runtime::validate_recovery_schema(db)?;
    }
    #[cfg(unix)]
    if kind == DbKind::Workspace && matches!(version, 4 | 5) {
        crate::workspace::validate_recovery_schema(db)?;
    }
    if kind == DbKind::Export {
        db.prepare("SELECT sequence,record FROM exports LIMIT 0")?;
        if version == 2 {
            db.prepare("SELECT result_id,event_sequence,record FROM external_checks LIMIT 0")?;
        }
    }
    Ok(())
}

fn validate_authority(db: &Connection) -> Result<()> {
    validate_database(db, DbKind::Authority)
}

fn quiesce_database(path: &Path) -> Result<Connection> {
    require_regular(path)?;
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(std::time::Duration::ZERO)?;
    let locking: String = db.query_row("PRAGMA locking_mode=EXCLUSIVE", [], |row| row.get(0))?;
    if !locking.eq_ignore_ascii_case("exclusive") {
        return Err(Error::Conflict("SQLite refused exclusive locking".into()));
    }
    // Exclusive locking mode retains the database lock across the checkpoint and
    // copy, until this connection closes. Ordinary BEGIN IMMEDIATE alone would
    // leave a race between checkpoint and transaction acquisition in WAL mode.
    db.execute_batch("BEGIN EXCLUSIVE; COMMIT;")?;
    let (busy, log, checkpointed): (i64, i64, i64) =
        db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy != 0 || log != checkpointed {
        return Err(Error::Conflict(
            "SQLite is active; stop every writer and reader before backup".into(),
        ));
    }
    Ok(db)
}

struct Objects(PathBuf);
impl EncodedObjectReader for Objects {
    fn open_encoded(&self, id: &str) -> Result<Box<dyn Read + '_>> {
        crate::store::validate_id(id)?;
        let path = self.0.join(&id[..2]).join(&id[2..]);
        reject_ancestors(&self.0, &path)?;
        require_regular(&path)?;
        Ok(Box::new(File::open(path)?))
    }
}

fn is_object_shard(path: &str) -> bool {
    path.len() == 2
        && path
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_publication_scratch(shard: &str, name: &str) -> bool {
    if !is_object_shard(shard) {
        return false;
    }
    let Some(rest) = name.strip_prefix(".tmp-") else {
        return false;
    };
    let mut parts = rest.split('-');
    let (Some(pid), Some(sequence), Some(prefix), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    pid.parse::<u32>()
        .is_ok_and(|value| value > 0 && value.to_string() == pid)
        && sequence
            .parse::<u64>()
            .is_ok_and(|value| value.to_string() == sequence)
        && prefix.len() == 12
        && prefix.starts_with(shard)
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn reject_unqualified_issue_workflow(data: &Path) -> Result<()> {
    match fs::symlink_metadata(data.join("issue-workflow")) {
        Ok(_) => Err(Error::Invalid(
            "issue-workflow recovery is unsupported until a typed journal adapter is available"
                .into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_issue_authority(
    repository: &str,
    issue_config: Option<&IssueWorkflowRecoveryConfig>,
) -> Result<()> {
    let Some(config) = issue_config else {
        return Ok(());
    };
    config.validate()?;
    if config.native_repository() != repository {
        return Err(Error::Invalid(
            "configured issue recovery repository does not match authority".into(),
        ));
    }
    Ok(())
}

fn verify_source(
    data: &Path,
    db: &Connection,
    issue_config: Option<&IssueWorkflowRecoveryConfig>,
) -> Result<()> {
    if issue_config.is_none() {
        reject_unqualified_issue_workflow(data)?;
    }
    let objects = data.join("objects");
    require_directory(&objects)?;
    let mut revisions = BTreeSet::new();
    let mut parents = BTreeSet::new();
    for entry in scan(&objects, false, &BTreeSet::new())? {
        if entry.directory {
            if !entry.path.is_empty() && !is_object_shard(&entry.path) {
                return Err(Error::Invalid("noncanonical object-store directory".into()));
            }
            continue;
        }
        let Some((prefix, tail)) = entry.path.split_once('/') else {
            return Err(Error::Invalid("unexpected object-store entry".into()));
        };
        // A killed publisher can leave incomplete bytes before the hard-link
        // publication point, or a second link after it. Preserve this bounded
        // scratch in the ordinary recovery inventory, without treating it as a
        // published object or revision root. Canonical objects still validate.
        if is_publication_scratch(prefix, tail) {
            if entry.bytes > MAX_OBJECT_BYTES {
                return Err(Error::Limit(
                    "publication scratch exceeds object size limit".into(),
                ));
            }
            continue;
        }
        let id = format!("{prefix}{tail}");
        crate::store::validate_id(&id)?;
        if prefix.len() != 2 || tail.len() != 62 {
            return Err(Error::Invalid("noncanonical object path".into()));
        }
        let bytes = read_bounded(&objects.join(&entry.path), MAX_OBJECT_BYTES)?;
        if let EncodedObject::Revision(revision) =
            crate::store::inspect_encoded_object(&id, &bytes)?
        {
            revisions.insert(id);
            parents.extend(revision.parents);
        }
    }
    let reader = Objects(objects);
    let source = SourceReader::new(&reader);
    let heads: Vec<_> = revisions.difference(&parents).cloned().collect();
    if !heads.is_empty() {
        crate::sync::validate_retained_reader(&source, &heads)?;
    }
    let mut statement = db.prepare("SELECT revision FROM targets UNION SELECT revision FROM approvals UNION SELECT after_revision FROM integration_events UNION SELECT before_revision FROM integration_events WHERE before_revision IS NOT NULL")?;
    for revision in statement.query_map([], |row| row.get::<_, String>(0))? {
        source.load_revision(&revision?)?;
    }
    let mut statement = db.prepare("SELECT digest,canonical_json FROM review_contexts")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })? {
        let (digest, bytes) = row?;
        let context: crate::context::ReviewContext = serde_json::from_slice(&bytes)?;
        if context.digest()? != digest {
            return Err(Error::Invalid("retained context digest mismatch".into()));
        }
        source.load_revision(&context.revision)?;
        if let Some(base) = &context.base {
            source.load_revision(&base.revision)?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryMirrorOwnership {
    version: u32,
    profile: String,
    repository: String,
    target: String,
    destination: PathBuf,
    config: Vec<u8>,
    #[serde(default)]
    seed: Option<RecoveryMirrorSeed>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryMirrorSeed {
    object_format: String,
    commit: String,
    tree: String,
}

fn lowercase_sha1(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn owned_mirror(root: &Path) -> Result<bool> {
    if !root.is_dir() {
        return Ok(false);
    }
    let owner = root.join("agentscm-mirror.json");
    if !owner.try_exists()? {
        return Ok(false);
    }
    let owner: RecoveryMirrorOwnership = serde_json::from_slice(&read_bounded(&owner, 64 * 1024)?)?;
    let repository_valid = owner.repository.len() == 32
        && owner
            .repository
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    let seed_valid = owner.seed.as_ref().is_some_and(|seed| {
        seed.object_format == "sha1" && lowercase_sha1(&seed.commit) && lowercase_sha1(&seed.tree)
    });
    let supported_owner = matches!(
        (owner.version, owner.profile.as_str(), owner.seed.is_some()),
        (1, "agentscm-git-snapshot/v1", false)
            | (2, "agentscm-git-snapshot/v1", true)
            | (3, "agentscm-git-snapshot/v2", _)
    );
    if !supported_owner || !repository_valid || (owner.seed.is_some() && !seed_valid) {
        return Err(Error::Invalid("unsupported mirror ownership record".into()));
    }
    // These fields are required by the ownership schema, but the recorded
    // destination intentionally remains the original path when a backup is
    // verified at its relocated payload root.
    let _ = (owner.target, owner.destination, owner.config);
    require_regular(&root.join("agentscm-export.sqlite"))?;
    Ok(true)
}
fn lease_root(root: &Path, destination: &str) -> Result<Option<PathBuf>> {
    if destination.starts_with("workspaces/") {
        Ok(Some(root.join(WORKSPACE_METADATA_DIRECTORY)))
    } else if destination == "data" || (owned_mirror(root)? && root.join(LOCK).try_exists()?) {
        Ok(Some(root.to_path_buf()))
    } else {
        Ok(None)
    }
}
fn database_paths(
    root: &Path,
    destination: &str,
    issue_config: Option<&IssueWorkflowRecoveryConfig>,
) -> Result<Vec<(PathBuf, DbKind)>> {
    if destination == "data"
        && root.join("attachments").try_exists()?
        && retained_attachments(root)?
    {
        return Err(Error::Invalid("backup/restore of nonempty attached stores is not qualified; attachment journals must not be copied as unmanaged files".into()));
    }
    if destination == "data" && issue_config.is_none() {
        reject_unqualified_issue_workflow(root)?;
    }
    let names: &[(&str, DbKind)] = if destination == "data" {
        &[
            ("state.sqlite", DbKind::Authority),
            ("merges/state.sqlite", DbKind::Merge),
            ("workflow.sqlite", DbKind::Workflow),
            ("skill-workflows/state.sqlite", DbKind::SkillWorkflow),
            ("git-reconcile.sqlite", DbKind::GitReconcile),
        ]
    } else if destination.starts_with("workspaces/") {
        &[(".agentscm-workspace/state.sqlite", DbKind::Workspace)]
    } else if owned_mirror(root)? {
        &[("agentscm-export.sqlite", DbKind::Export)]
    } else {
        &[]
    };
    let mut paths = Vec::new();
    for (name, kind) in names {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                reject_ancestors(root, &path)?;
                require_regular(&path)?;
                paths.push((path, *kind))
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && matches!(
                        *kind,
                        DbKind::Merge
                            | DbKind::Workflow
                            | DbKind::SkillWorkflow
                            | DbKind::GitReconcile
                    ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    if destination == "data"
        && let Some(path) = crate::issue_workflow::recovery::database_path(root)?
    {
        paths.push((path, DbKind::Issue));
    }
    Ok(paths)
}

#[cfg(unix)]
fn retained_attachments(root: &Path) -> Result<bool> {
    crate::attached_worktree::journal_has_retained_state(root)
}

#[cfg(not(unix))]
fn retained_attachments(_root: &Path) -> Result<bool> {
    Ok(true)
}
fn ignored_paths(
    root: &Path,
    destination: &str,
    issue_config: Option<&IssueWorkflowRecoveryConfig>,
) -> Result<BTreeSet<PathBuf>> {
    let mut ignored = BTreeSet::new();
    if !destination.starts_with("attachments/")
        && let Some(lease) = lease_root(root, destination)?
    {
        ignored.insert(lease.join(LOCK));
        #[cfg(unix)]
        if destination.starts_with("workspaces/") {
            ignored.insert(lease.join(crate::workspace::TIMELINE_LEASE_FILE));
            ignored.insert(lease.join(crate::workspace::TIMELINE_PROBES_DIRECTORY));
        }
    }
    for (database, _) in database_paths(root, destination, issue_config)? {
        let mut wal = database.as_os_str().to_os_string();
        wal.push("-wal");
        ignored.insert(PathBuf::from(wal));
        let mut shm = database.as_os_str().to_os_string();
        shm.push("-shm");
        ignored.insert(PathBuf::from(shm));
    }
    Ok(ignored)
}

fn scan(root: &Path, hash: bool, ignored: &BTreeSet<PathBuf>) -> Result<Vec<InventoryEntry>> {
    let mut pending = vec![(root.to_path_buf(), String::new(), 0usize)];
    let mut entries = Vec::new();
    let mut total = 0u64;
    while let Some((path, relative, depth)) = pending.pop() {
        if depth > 128 || entries.len() >= MAX_ENTRIES {
            return Err(Error::Limit("backup traversal bound exceeded".into()));
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
            return Err(Error::Invalid(format!(
                "unsupported backup entry: {}",
                path.display()
            )));
        }
        let bytes = if metadata.is_file() {
            metadata.len()
        } else {
            0
        };
        total = total
            .checked_add(bytes)
            .ok_or_else(|| Error::Limit("backup size overflow".into()))?;
        if bytes > MAX_FILE || total > MAX_TOTAL {
            return Err(Error::Limit("backup exceeds file/total size bound".into()));
        }
        entries.push(InventoryEntry {
            path: relative.clone(),
            directory: metadata.is_dir(),
            bytes,
            sha256: if hash && metadata.is_file() {
                hash_file(&path, MAX_FILE)?
            } else {
                String::new()
            },
            mode: mode(&metadata),
        });
        if metadata.is_dir() {
            for child in fs::read_dir(&path)? {
                let child = child?;
                let name = child
                    .file_name()
                    .into_string()
                    .map_err(|_| Error::Invalid("backup path is not UTF-8".into()))?;
                if ignored.contains(&child.path()) {
                    continue;
                }
                let next = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                if next.len() > 4096 || entries.len() + pending.len() >= MAX_ENTRIES {
                    return Err(Error::Limit("backup path/entry bound exceeded".into()));
                }
                pending.push((child.path(), next, depth + 1));
            }
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn safe_relative(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 4096
        || value.contains('\\')
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || path.to_str() != Some(value)
        || value.split('/').any(|c| c.is_empty() || c == ".")
    {
        return Err(Error::Invalid("invalid inventory path".into()));
    }
    Ok(())
}
fn reject_link(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(Error::Invalid("backup path is a symbolic link".into()));
    }
    Ok(())
}
fn reject_ancestors(root: &Path, path: &Path) -> Result<()> {
    require_directory(root)?;
    for ancestor in path.ancestors() {
        reject_link(ancestor)?;
        if ancestor == root {
            return Ok(());
        }
    }
    Err(Error::Invalid("path outside recovery root".into()))
}
fn require_regular(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.file_type().is_symlink() {
        return Err(Error::Invalid(format!(
            "not a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}
fn require_directory(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.file_type().is_symlink() {
        return Err(Error::Invalid(format!(
            "not a directory: {}",
            path.display()
        )));
    }
    Ok(())
}
fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Invalid("non-UTF8 backup root".into()))
}
fn new_destination(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .filter(|n| *n != "." && *n != "..")
        .ok_or_else(|| Error::Invalid("destination needs a new directory name".into()))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent)?;
    let full = parent.join(name);
    path_text(&full)?;
    match fs::symlink_metadata(&full) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(full),
        Err(e) => Err(e.into()),
        Ok(_) => Err(Error::Conflict(
            "recovery destination already exists".into(),
        )),
    }
}
fn new_options() -> OpenOptions {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut o = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    o
}
fn create_private_root(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}
pub(crate) fn create_private_directory(path: &Path) -> Result<()> {
    if path.exists() {
        require_directory(path)?;
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = fs::DirBuilder::new();
        b.mode(0o700).recursive(true).create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}
// Windows cannot open a directory as a file; NTFS journals renames, so there is nothing to sync.
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = new_options().write(true).create_new(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    require_regular(path)?;
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Limit("recovery file exceeds bound".into()));
    }
    Ok(bytes)
}
fn hash_file(path: &Path, limit: u64) -> Result<String> {
    require_regular(path)?;
    let mut input = File::open(path)?.take(limit + 1);
    let mut h = Sha256::new();
    let mut b = [0u8; 65536];
    let mut total = 0;
    loop {
        let n = input.read(&mut b)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > limit {
            return Err(Error::Limit("recovery file exceeds bound".into()));
        }
        h.update(&b[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
fn copy_new(from: &Path, to: &Path, entry: &InventoryEntry) -> Result<()> {
    require_regular(from)?;
    let mut source = File::open(from)?.take(entry.bytes + 1);
    let mut out = new_options().write(true).create_new(true).open(to)?;
    let copied = std::io::copy(&mut source, &mut out)?;
    out.sync_all()?;
    if copied != entry.bytes || hash_file(to, MAX_FILE)? != entry.sha256 {
        return Err(Error::Conflict(
            "source changed or copy checksum failed".into(),
        ));
    }
    if let Some(parent) = to.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}
fn open_readonly(path: &Path) -> Result<Connection> {
    require_regular(path)?;
    let absolute = fs::canonicalize(path)?;
    let mut uri = String::from("file:");
    for byte in absolute.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(*byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push_str("?immutable=1");
    Ok(Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?)
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}
#[cfg(not(unix))]
fn mode(_: &fs::Metadata) -> u32 {
    0
}
#[cfg(unix)]
fn set_mode(path: &Path, value: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(value))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DbKind, validate_database};
    use rusqlite::Connection;

    #[test]
    fn recovery_accepts_only_known_export_journal_versions() {
        for version in [1, 2] {
            let db = Connection::open_in_memory().unwrap();
            db.pragma_update(None, "application_id", 1095190616)
                .unwrap();
            db.pragma_update(None, "user_version", version).unwrap();
            // An identity-only database is not an export journal.
            assert!(validate_database(&db, DbKind::Export).is_err());
            db.execute_batch(
                "CREATE TABLE exports(sequence INTEGER PRIMARY KEY, record BLOB NOT NULL) STRICT;",
            )
            .unwrap();
            if version == 2 {
                assert!(validate_database(&db, DbKind::Export).is_err());
                db.execute_batch(
                    "CREATE TABLE external_checks(result_id TEXT PRIMARY KEY, event_sequence INTEGER NOT NULL, record BLOB NOT NULL) STRICT;",
                )
                .unwrap();
            }
            validate_database(&db, DbKind::Export).unwrap();
            if version == 2 {
                db.execute_batch(
                    "ALTER TABLE external_checks RENAME COLUMN event_sequence TO wrong_event;",
                )
                .unwrap();
                assert!(validate_database(&db, DbKind::Export).is_err());
                db.execute_batch(
                    "ALTER TABLE external_checks RENAME COLUMN wrong_event TO event_sequence;",
                )
                .unwrap();
            }
            db.execute_batch("ALTER TABLE exports RENAME COLUMN record TO wrong_record;")
                .unwrap();
            assert!(validate_database(&db, DbKind::Export).is_err());
        }
        let db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "application_id", 1095190616)
            .unwrap();
        db.pragma_update(None, "user_version", 3).unwrap();
        assert!(validate_database(&db, DbKind::Export).is_err());
        let db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "application_id", 1095190615)
            .unwrap();
        db.pragma_update(None, "user_version", 2).unwrap();
        assert!(validate_database(&db, DbKind::Export).is_err());
    }
}

#[cfg(all(test, not(unix)))]
#[test]
fn git_reconciliation_recovery_requires_unix() {
    let db = Connection::open_in_memory().unwrap();
    let objects = Objects(PathBuf::new());
    assert!(matches!(
        validate_journal(
            &db,
            DbKind::GitReconcile,
            &"a".repeat(32),
            &SourceReader::new(&objects),
            None,
        ),
        Err(Error::Invalid(message)) if message == "Git reconciliation recovery is unsupported on non-Unix platforms"
    ));
}

#[cfg(test)]
mod issue_recovery_configuration_tests {
    use super::IssueWorkflowRecoveryConfig;

    #[test]
    fn configured_issue_identity_is_strict_and_not_inferred() {
        let configured =
            IssueWorkflowRecoveryConfig::new(&"a".repeat(32), "installation-one").unwrap();
        let bytes = serde_json::to_vec(&configured).unwrap();
        assert_eq!(
            IssueWorkflowRecoveryConfig::from_json(&bytes).unwrap(),
            configured
        );
        assert_eq!(configured.native_repository(), "a".repeat(32));
        assert_eq!(configured.installation_id(), "installation-one");
        let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for (key, value) in [
            ("version", serde_json::json!(2)),
            ("native_repository", serde_json::json!("A".repeat(32))),
            ("installation_id", serde_json::json!("")),
            ("installation_id", serde_json::json!("x".repeat(129))),
            ("installation_id", serde_json::json!("path/identifier")),
            ("unexpected", serde_json::json!(true)),
        ] {
            let mut invalid = original.clone();
            invalid[key] = value;
            assert!(
                IssueWorkflowRecoveryConfig::from_json(&serde_json::to_vec(&invalid).unwrap())
                    .is_err(),
                "{key}"
            );
        }
        assert!(IssueWorkflowRecoveryConfig::from_json(&vec![b' '; 16 * 1024 + 1]).is_err());
    }

    #[test]
    fn configured_issue_file_is_bounded_regular_and_explicit() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("issue-recovery.json");
        let configured =
            IssueWorkflowRecoveryConfig::new(&"b".repeat(32), "installation-two").unwrap();
        std::fs::write(&file, serde_json::to_vec(&configured).unwrap()).unwrap();
        assert_eq!(
            IssueWorkflowRecoveryConfig::read(&file).unwrap(),
            configured
        );
        assert!(IssueWorkflowRecoveryConfig::read(temp.path()).is_err());
        #[cfg(unix)]
        {
            let link = temp.path().join("linked-config");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(IssueWorkflowRecoveryConfig::read(&link).is_err());
        }
        std::fs::write(&file, vec![b' '; 16 * 1024 + 1]).unwrap();
        assert!(matches!(
            IssueWorkflowRecoveryConfig::read(&file),
            Err(crate::Error::Limit(_))
        ));
    }
}
