//! Descriptor-relative, recoverable updates for project-local Claude settings.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use agentscm::checks::sha256;
use agentscm::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_BYTES: u64 = 256 * 1024;
const MAX_RECOVERIES: usize = 8;
const CONFIGURE_RECOVERIES: usize = MAX_RECOVERIES - 1;
const MAX_DIRECTORY_ENTRIES: usize = 4096;

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

#[derive(Debug)]
pub(super) struct SettingsLocation {
    directory: File,
    target: CString,
    target_text: String,
    display: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecoveryDescriptor {
    version: u8,
    transaction: String,
    target: String,
    recovery: Option<String>,
    prior_existed: bool,
    active_sha256: String,
    recovery_sha256: Option<String>,
    #[serde(default)]
    original_recovery_sha256: Option<String>,
    #[serde(default)]
    late_write_observed: bool,
    #[serde(default)]
    kind: RecoveryKind,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RecoveryKind {
    #[default]
    Displaced,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionJournal {
    version: u8,
    transaction: String,
    target: String,
    temp: String,
    recovery: Option<String>,
    descriptor: String,
    prior_existed: bool,
    expected_sha256: String,
    replacement_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum RecoveryOperationJournal {
    Restore {
        version: u8,
        descriptor: RecoveryDescriptor,
    },
    Prune {
        version: u8,
        descriptor: RecoveryDescriptor,
        descriptor_sha256: String,
        recovery_sha256: Option<String>,
    },
}

struct RecoveryEntry {
    name: String,
    descriptor: RecoveryDescriptor,
}

struct RetainedNames {
    temp: String,
    recovery: String,
    descriptor: String,
    conflict: String,
}

impl SettingsLocation {
    pub(super) fn open(root: &Path, path: &Path) -> Result<Self> {
        if root.canonicalize()? != root {
            return Err(invalid("configured checkout root must be canonical"));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid("Claude settings path is outside the configured checkout"))?;
        let target = relative
            .file_name()
            .ok_or_else(|| invalid("bad Claude settings path"))?;
        let target_text = target
            .to_str()
            .ok_or_else(|| invalid("Claude settings file name is not UTF-8"))?
            .to_owned();
        let target = cstring(target.as_bytes(), "Claude settings file name contains NUL")?;
        let mut directory = open_directory_absolute(root)?;
        let parent = relative
            .parent()
            .ok_or_else(|| invalid("bad Claude settings path"))?;
        for component in parent.components() {
            let Component::Normal(component) = component else {
                return Err(invalid("Claude settings path is not normalized"));
            };
            let name = cstring(component.as_bytes(), "Claude settings path contains NUL")?;
            directory = open_or_create_directory(&directory, &name)?;
        }
        Ok(Self {
            directory,
            target,
            target_text,
            display: path.to_owned(),
        })
    }

    pub(super) fn display(&self) -> &Path {
        &self.display
    }

    pub(super) fn read_target(&self) -> Result<Option<Vec<u8>>> {
        read_optional(&self.directory, &self.target)
    }

    fn journal_name(&self) -> String {
        format!(".{}.agentscm-capture-transaction", self.target_text)
    }

    fn recovery_prefix(&self) -> String {
        format!(".{}.agentscm-capture-recovery-", self.target_text)
    }

    fn recovery_operation_name(&self) -> String {
        format!(".{}.agentscm-capture-recovery-operation", self.target_text)
    }

    fn conflict_prefix(&self) -> String {
        format!(".{}.agentscm-capture-conflict-", self.target_text)
    }

    fn retained_names(&self, transaction: &str) -> Result<RetainedNames> {
        validate_transaction_id(transaction)?;
        Ok(RetainedNames {
            temp: format!(".{}.agentscm-capture-{transaction}.tmp", self.target_text),
            recovery: format!("{}{transaction}.bytes", self.recovery_prefix()),
            descriptor: format!("{}{transaction}.json", self.recovery_prefix()),
            conflict: format!("{}{transaction}.bytes", self.conflict_prefix()),
        })
    }
}

pub(super) fn replace(
    location: &SettingsLocation,
    prior: Option<&[u8]>,
    replacement: &[u8],
    configuring: bool,
) -> Result<()> {
    if replacement.len() as u64 > MAX_BYTES {
        return Err(invalid("Claude settings replacement is too large"));
    }
    refuse_unresolved_recovery(location, configuring)?;
    let journal_name = location.journal_name();
    let journal_c = cstring(journal_name.as_bytes(), "bad transaction name")?;
    if read_optional(&location.directory, &journal_c)?.is_some() {
        return Err(invalid(
            "Claude settings have an unfinished AgentSCM transaction; run change-capture settings-recovery",
        ));
    }

    let transaction = random_id()?;
    let names = location.retained_names(&transaction)?;
    let temp = names.temp.clone();
    let recovery = prior.map(|_| names.recovery.clone());
    let descriptor = names.descriptor.clone();
    let temp_c = cstring(temp.as_bytes(), "bad temporary name")?;
    let recovery_c = recovery
        .as_deref()
        .map(|name| cstring(name.as_bytes(), "bad recovery name"))
        .transpose()?;
    let descriptor_c = cstring(descriptor.as_bytes(), "bad recovery descriptor name")?;

    let journal = TransactionJournal {
        version: 1,
        transaction: transaction.clone(),
        target: location.target_text.clone(),
        temp: temp.clone(),
        recovery: recovery.clone(),
        descriptor: descriptor.clone(),
        prior_existed: prior.is_some(),
        expected_sha256: sha256(prior.unwrap_or_default()),
        replacement_sha256: sha256(replacement),
    };
    write_new_atomic(
        &location.directory,
        &journal_c,
        &serde_json::to_vec(&journal)
            .map_err(|_| invalid("cannot encode Claude settings transaction"))?,
        &transaction,
    )?;
    sync_directory(&location.directory)?;
    if let Err(error) = write_new(&location.directory, &temp_c, replacement) {
        unlink_optional(&location.directory, &journal_c)?;
        sync_directory(&location.directory)?;
        return Err(error);
    }

    if let Some(expected) = prior {
        if let Err(error) = atomic_exchange(&location.directory, &location.target, &temp_c) {
            unlink_optional(&location.directory, &temp_c)?;
            unlink_optional(&location.directory, &journal_c)?;
            sync_directory(&location.directory)?;
            return Err(invalid(format!(
                "safe conditional Claude settings replacement is unavailable: {error}"
            )));
        }
        sync_directory(&location.directory)?;
        make_private(&location.directory, &temp_c)?;
        let displaced = read_required(&location.directory, &temp_c)?;
        if displaced != expected {
            let conflict = names.conflict.clone();
            let conflict_c = cstring(conflict.as_bytes(), "bad conflict name")?;
            write_new(&location.directory, &conflict_c, &displaced)?;
            atomic_exchange(&location.directory, &location.target, &temp_c)?;
            unlink_optional(&location.directory, &temp_c)?;
            let conflict_descriptor = RecoveryDescriptor {
                version: 1,
                transaction: transaction.clone(),
                target: location.target_text.clone(),
                recovery: Some(conflict),
                prior_existed: true,
                active_sha256: sha256(&displaced),
                recovery_sha256: Some(sha256(&displaced)),
                original_recovery_sha256: Some(sha256(&displaced)),
                late_write_observed: false,
                kind: RecoveryKind::Conflict,
            };
            write_new_atomic(
                &location.directory,
                &descriptor_c,
                &serde_json::to_vec(&conflict_descriptor)
                    .map_err(|_| invalid("cannot encode Claude settings conflict descriptor"))?,
                &transaction,
            )?;
            unlink_optional(&location.directory, &journal_c)?;
            sync_directory(&location.directory)?;
            return Err(invalid(format!(
                "Claude settings changed during capture configuration; conflicting bytes are retained under transaction {transaction}"
            )));
        }
        let recovery_c = recovery_c
            .as_ref()
            .ok_or_else(|| invalid("missing recovery name"))?;
        rename_name(&location.directory, &temp_c, recovery_c)?;
    } else if let Err(error) = link_name(&location.directory, &temp_c, &location.target) {
        unlink_optional(&location.directory, &temp_c)?;
        unlink_optional(&location.directory, &journal_c)?;
        sync_directory(&location.directory)?;
        return Err(error);
    } else {
        unlink_optional(&location.directory, &temp_c)?;
    }

    let recovery_descriptor = RecoveryDescriptor {
        version: 1,
        transaction,
        target: location.target_text.clone(),
        recovery,
        prior_existed: prior.is_some(),
        active_sha256: sha256(replacement),
        recovery_sha256: prior.map(sha256),
        original_recovery_sha256: prior.map(sha256),
        late_write_observed: false,
        kind: RecoveryKind::Displaced,
    };
    write_new_atomic(
        &location.directory,
        &descriptor_c,
        &serde_json::to_vec(&recovery_descriptor)
            .map_err(|_| invalid("cannot encode Claude settings recovery descriptor"))?,
        &recovery_descriptor.transaction,
    )?;
    unlink_optional(&location.directory, &journal_c)?;
    sync_directory(&location.directory)
}

pub(super) fn recover(
    location: &SettingsLocation,
    restore: Option<&str>,
    prune_unchanged: bool,
    reconcile_changed: Option<&str>,
) -> Result<Value> {
    cleanup_stages(location)?;
    if let Some(transaction) = reconcile_changed {
        validate_transaction_id(transaction)?;
    }
    let recovery_operation_reconciled = recover_recovery_operation(location, reconcile_changed)?;
    let journal_c = cstring(location.journal_name().as_bytes(), "bad transaction name")?;
    let had_unfinished = read_optional(&location.directory, &journal_c)?.is_some();
    let unfinished = recover_incomplete(location, reconcile_changed)?;
    if unfinished.is_some() && reconcile_changed.is_some() {
        return Err(invalid(
            "the requested Claude settings reconciliation does not match the unfinished transaction",
        ));
    }
    if unfinished.is_some() && (restore.is_some() || prune_unchanged) {
        return Err(invalid(
            "inspect and reconcile the unfinished Claude settings transaction first",
        ));
    }
    if unfinished.is_none() {
        if had_unfinished {
            if let Some(transaction) = reconcile_changed {
                let reconciled = recovery_entries(location)?.into_iter().any(|entry| {
                    entry.descriptor.transaction == transaction
                        && entry.descriptor.late_write_observed
                });
                if !reconciled {
                    return Err(invalid(
                        "Claude settings recovery transaction has no changed retained bytes",
                    ));
                }
            }
        } else if !recovery_operation_reconciled {
            reconcile_retained_entry(location, reconcile_changed)?;
        }
    }
    let mut restored = false;
    if let Some(transaction) = restore {
        validate_transaction_id(transaction)?;
        let mut entries = recovery_entries(location)?;
        let entry = entries
            .iter_mut()
            .find(|entry| entry.descriptor.transaction == transaction)
            .ok_or_else(|| invalid("Claude settings recovery transaction was not found"))?;
        restore_entry(location, entry)?;
        restored = true;
    }
    let mut pruned = 0_usize;
    let mut changed = 0_usize;
    if prune_unchanged {
        for entry in recovery_entries(location)? {
            if entry.descriptor.late_write_observed || recovery_changed(location, &entry)? {
                changed += 1;
                continue;
            }
            remove_entry(location, &entry)?;
            pruned += 1;
        }
        sync_directory(&location.directory)?;
    }
    let entries = recovery_entries(location)?;
    let statuses = entries
        .iter()
        .map(|entry| {
            Ok(json!({
                "transaction": entry.descriptor.transaction,
                "prior_existed": entry.descriptor.prior_existed,
                "late_write": entry.descriptor.late_write_observed || recovery_changed(location, entry)?,
                "kind": entry.descriptor.kind,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "settings": location.display(),
        "restored": restored,
        "pruned": pruned,
        "changed_not_pruned": changed,
        "recoveries": statuses,
        "unfinished_transaction": unfinished,
        "limit": MAX_RECOVERIES,
    }))
}

fn refuse_unresolved_recovery(location: &SettingsLocation, configuring: bool) -> Result<()> {
    if read_optional(
        &location.directory,
        &cstring(
            location.recovery_operation_name().as_bytes(),
            "bad recovery operation name",
        )?,
    )?
    .is_some()
    {
        return Err(invalid(
            "Claude settings have an unfinished recovery operation; run change-capture settings-recovery",
        ));
    }
    if read_optional(
        &location.directory,
        &cstring(location.journal_name().as_bytes(), "bad transaction name")?,
    )?
    .is_some()
    {
        return Err(invalid(
            "Claude settings have an unfinished AgentSCM transaction; run change-capture settings-recovery",
        ));
    }
    let entries = recovery_entries(location)?;
    if entries
        .iter()
        .any(|entry| recovery_changed(location, entry).unwrap_or(true))
    {
        return Err(invalid(
            "a retained Claude settings inode changed after replacement; run change-capture settings-recovery",
        ));
    }
    let limit = if configuring {
        CONFIGURE_RECOVERIES
    } else {
        MAX_RECOVERIES
    };
    if entries.len() >= limit {
        return Err(invalid(
            "Claude settings recovery history is full; inspect and prune it with change-capture settings-recovery",
        ));
    }
    Ok(())
}

fn recover_incomplete(
    location: &SettingsLocation,
    reconcile_changed: Option<&str>,
) -> Result<Option<String>> {
    let journal_name = location.journal_name();
    let journal_c = cstring(journal_name.as_bytes(), "bad transaction name")?;
    let Some(bytes) = read_optional(&location.directory, &journal_c)? else {
        return Ok(None);
    };
    let journal: TransactionJournal = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("Claude settings transaction journal is invalid"))?;
    if journal.version != 1 || journal.target != location.target_text {
        return Err(invalid("Claude settings transaction journal is invalid"));
    }
    validate_transaction_id(&journal.transaction)?;
    let names = location.retained_names(&journal.transaction)?;
    let expected_recovery = journal.prior_existed.then_some(names.recovery.as_str());
    if journal.temp != names.temp
        || journal.descriptor != names.descriptor
        || journal.recovery.as_deref() != expected_recovery
    {
        return Err(invalid(
            "Claude settings transaction journal names are invalid",
        ));
    }
    let temp_c = cstring(names.temp.as_bytes(), "bad journal temporary name")?;
    let descriptor_c = cstring(names.descriptor.as_bytes(), "bad journal descriptor name")?;
    let target = location.read_target()?;
    let temp = read_optional(&location.directory, &temp_c)?;
    let recovery_c = journal
        .recovery
        .as_deref()
        .map(|name| cstring(name.as_bytes(), "bad journal recovery name"))
        .transpose()?;
    let recovery = recovery_c
        .as_ref()
        .map(|name| read_optional(&location.directory, name))
        .transpose()?
        .flatten();
    let target_sha = target.as_deref().map(sha256);
    let temp_sha = temp.as_deref().map(sha256);
    let recovery_sha = recovery.as_deref().map(sha256);
    let conflict_name = names.conflict.clone();
    let conflict_c = cstring(conflict_name.as_bytes(), "bad conflict recovery name")?;
    let conflict = read_optional(&location.directory, &conflict_c)?;
    let conflict_sha = conflict.as_deref().map(sha256);
    let expected_target = if journal.prior_existed {
        Some(journal.expected_sha256.as_str())
    } else {
        None
    };

    if let Some(conflict_sha) = conflict_sha.as_deref() {
        if target_sha.as_deref() == Some(journal.replacement_sha256.as_str())
            && temp_sha.as_deref() == Some(conflict_sha)
        {
            atomic_exchange(&location.directory, &location.target, &temp_c)?;
            unlink_optional(&location.directory, &temp_c)?;
            sync_directory(&location.directory)?;
        } else if target_sha.as_deref() == Some(conflict_sha)
            && (temp.is_none() || temp_sha.as_deref() == Some(journal.replacement_sha256.as_str()))
        {
            unlink_optional(&location.directory, &temp_c)?;
        } else {
            return Err(invalid(
                "Claude settings conflict recovery is ambiguous; retained files were left untouched",
            ));
        }
        let descriptor = RecoveryDescriptor {
            version: 1,
            transaction: journal.transaction,
            target: journal.target,
            recovery: Some(conflict_name),
            prior_existed: true,
            active_sha256: conflict_sha.to_owned(),
            recovery_sha256: Some(conflict_sha.to_owned()),
            original_recovery_sha256: Some(conflict_sha.to_owned()),
            late_write_observed: false,
            kind: RecoveryKind::Conflict,
        };
        write_recovery_descriptor(location, &descriptor_c, &descriptor)?;
        unlink_optional(&location.directory, &journal_c)?;
        sync_directory(&location.directory)?;
        return Ok(None);
    }

    if target_sha.as_deref() == expected_target
        && temp_sha.as_deref() == Some(journal.replacement_sha256.as_str())
        && recovery.is_none()
    {
        unlink_optional(&location.directory, &temp_c)?;
        unlink_optional(&location.directory, &journal_c)?;
        sync_directory(&location.directory)?;
        return Ok(None);
    }
    if target_sha.as_deref() == expected_target && temp.is_none() && recovery.is_none() {
        unlink_optional(&location.directory, &journal_c)?;
        sync_directory(&location.directory)?;
        return Ok(None);
    }
    if target_sha.as_deref() != Some(journal.replacement_sha256.as_str()) {
        return Err(invalid(
            "Claude settings transaction cannot be recovered automatically; retained files were left untouched",
        ));
    }
    if journal.prior_existed {
        if recovery.is_none() && temp_sha.as_deref() == Some(journal.expected_sha256.as_str()) {
            make_private(&location.directory, &temp_c)?;
            rename_name(
                &location.directory,
                &temp_c,
                recovery_c
                    .as_ref()
                    .ok_or_else(|| invalid("transaction recovery name is missing"))?,
            )?;
        } else if recovery_sha.as_deref() != Some(journal.expected_sha256.as_str()) {
            if reconcile_changed != Some(journal.transaction.as_str()) {
                return Ok(Some(journal.transaction));
            }
            let recovery_name = recovery_c
                .as_ref()
                .ok_or_else(|| invalid("transaction recovery name is missing"))?;
            make_private(&location.directory, recovery_name)?;
        }
    } else {
        unlink_optional(&location.directory, &temp_c)?;
    }
    let expected_recovery_sha256 = journal
        .prior_existed
        .then(|| journal.expected_sha256.clone());
    let descriptor = RecoveryDescriptor {
        version: 1,
        transaction: journal.transaction,
        target: journal.target,
        recovery: journal.recovery,
        prior_existed: journal.prior_existed,
        active_sha256: journal.replacement_sha256,
        recovery_sha256: expected_recovery_sha256.clone(),
        original_recovery_sha256: expected_recovery_sha256,
        late_write_observed: false,
        kind: RecoveryKind::Displaced,
    };
    let mut descriptor = descriptor;
    if journal.prior_existed {
        let actual = read_required(
            &location.directory,
            recovery_c
                .as_ref()
                .ok_or_else(|| invalid("transaction recovery name is missing"))?,
        )?;
        let actual_sha = sha256(&actual);
        descriptor.late_write_observed = actual_sha != descriptor.recovery_sha256.clone().unwrap();
        descriptor.recovery_sha256 = Some(actual_sha);
    }
    write_recovery_descriptor(location, &descriptor_c, &descriptor)?;
    cleanup_stages(location)?;
    unlink_optional(&location.directory, &journal_c)?;
    sync_directory(&location.directory)?;
    Ok(None)
}

fn reconcile_retained_entry(location: &SettingsLocation, transaction: Option<&str>) -> Result<()> {
    let Some(transaction) = transaction else {
        return Ok(());
    };
    let mut entries = recovery_entries(location)?;
    let entry = entries
        .iter_mut()
        .find(|entry| entry.descriptor.transaction == transaction)
        .ok_or_else(|| invalid("Claude settings recovery transaction was not found"))?;
    if !recovery_changed(location, entry)? {
        return Err(invalid(
            "Claude settings recovery transaction has no changed retained bytes",
        ));
    }
    let recovery = entry
        .descriptor
        .recovery
        .as_deref()
        .ok_or_else(|| invalid("Claude settings recovery bytes are missing"))?;
    let recovery_c = cstring(recovery.as_bytes(), "bad recovery name")?;
    make_private(&location.directory, &recovery_c)?;
    let actual_sha = sha256(&read_required(&location.directory, &recovery_c)?);
    if entry.descriptor.original_recovery_sha256.is_none() {
        entry.descriptor.original_recovery_sha256 = entry.descriptor.recovery_sha256.clone();
    }
    entry.descriptor.recovery_sha256 = Some(actual_sha);
    entry.descriptor.late_write_observed = true;
    rewrite_descriptor(location, entry)
}

fn write_recovery_descriptor(
    location: &SettingsLocation,
    name: &CString,
    descriptor: &RecoveryDescriptor,
) -> Result<()> {
    let bytes = serde_json::to_vec(descriptor)
        .map_err(|_| invalid("cannot encode Claude settings recovery descriptor"))?;
    if read_optional(&location.directory, name)?.as_deref() != Some(bytes.as_slice()) {
        unlink_optional(&location.directory, name)?;
        write_new_atomic(&location.directory, name, &bytes, &descriptor.transaction)?;
    }
    Ok(())
}

fn restore_entry(location: &SettingsLocation, entry: &mut RecoveryEntry) -> Result<()> {
    if entry.descriptor.kind == RecoveryKind::Conflict {
        return Err(invalid(
            "a conflict record is evidence only and cannot replace current Claude settings",
        ));
    }
    let current = location
        .read_target()?
        .ok_or_else(|| invalid("Claude settings target is missing"))?;
    if sha256(&current) != entry.descriptor.active_sha256 {
        return Err(invalid(
            "Claude settings changed since this recovery was created; restore refused",
        ));
    }
    if entry.descriptor.prior_existed {
        let recovery_name = entry
            .descriptor
            .recovery
            .as_deref()
            .ok_or_else(|| invalid("Claude settings recovery bytes are missing"))?;
        let recovery_c = cstring(recovery_name.as_bytes(), "bad recovery name")?;
        make_private(&location.directory, &recovery_c)?;
        let actual_sha = sha256(&read_required(&location.directory, &recovery_c)?);
        if entry.descriptor.recovery_sha256.as_deref() != Some(actual_sha.as_str()) {
            if entry.descriptor.original_recovery_sha256.is_none() {
                entry.descriptor.original_recovery_sha256 =
                    entry.descriptor.recovery_sha256.clone();
            }
            entry.descriptor.recovery_sha256 = Some(actual_sha);
            entry.descriptor.late_write_observed = true;
            rewrite_descriptor(location, entry)?;
        }
    }
    write_recovery_operation(
        location,
        &RecoveryOperationJournal::Restore {
            version: 1,
            descriptor: entry.descriptor.clone(),
        },
        &entry.descriptor.transaction,
    )?;
    recover_recovery_operation(location, None).map(|_| ())
}

fn recovery_entries(location: &SettingsLocation) -> Result<Vec<RecoveryEntry>> {
    let prefix = location.recovery_prefix();
    let mut entries = Vec::new();
    for name in directory_names(&location.directory)? {
        let Some(transaction) = name
            .strip_prefix(&prefix)
            .and_then(|value| value.strip_suffix(".json"))
        else {
            continue;
        };
        validate_transaction_id(transaction)?;
        let name_c = cstring(name.as_bytes(), "bad recovery descriptor name")?;
        let descriptor: RecoveryDescriptor =
            serde_json::from_slice(&read_required(&location.directory, &name_c)?)
                .map_err(|_| invalid("Claude settings recovery descriptor is invalid"))?;
        let names = location.retained_names(&descriptor.transaction)?;
        let expected_recovery = match (descriptor.kind, descriptor.prior_existed) {
            (RecoveryKind::Displaced, true) => Some(names.recovery.as_str()),
            (RecoveryKind::Displaced, false) => None,
            (RecoveryKind::Conflict, true) => Some(names.conflict.as_str()),
            (RecoveryKind::Conflict, false) => {
                return Err(invalid("Claude settings recovery descriptor is invalid"));
            }
        };
        if descriptor.version != 1
            || descriptor.target != location.target_text
            || descriptor.transaction != transaction
            || name != names.descriptor
            || descriptor.recovery.as_deref() != expected_recovery
        {
            return Err(invalid("Claude settings recovery descriptor is invalid"));
        }
        make_private(&location.directory, &name_c)?;
        if let Some(recovery) = expected_recovery {
            make_private(
                &location.directory,
                &cstring(recovery.as_bytes(), "bad recovery name")?,
            )?;
        }
        entries.push(RecoveryEntry { name, descriptor });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    if entries.len() > MAX_RECOVERIES {
        return Err(invalid(
            "Claude settings recovery history exceeds its bound",
        ));
    }
    Ok(entries)
}

fn recovery_changed(location: &SettingsLocation, entry: &RecoveryEntry) -> Result<bool> {
    match (
        &entry.descriptor.recovery,
        &entry.descriptor.recovery_sha256,
    ) {
        (None, None) if !entry.descriptor.prior_existed => Ok(false),
        (Some(name), Some(expected)) if entry.descriptor.prior_existed => {
            let bytes = read_required(
                &location.directory,
                &cstring(name.as_bytes(), "bad recovery name")?,
            )?;
            Ok(sha256(&bytes) != *expected)
        }
        _ => Err(invalid("Claude settings recovery descriptor is invalid")),
    }
}

fn remove_entry(location: &SettingsLocation, entry: &RecoveryEntry) -> Result<()> {
    let descriptor_bytes = read_required(
        &location.directory,
        &cstring(entry.name.as_bytes(), "bad recovery descriptor name")?,
    )?;
    let recovery_sha256 = entry
        .descriptor
        .recovery
        .as_deref()
        .map(|name| {
            read_required(
                &location.directory,
                &cstring(name.as_bytes(), "bad recovery name")?,
            )
            .map(|bytes| sha256(&bytes))
        })
        .transpose()?;
    write_recovery_operation(
        location,
        &RecoveryOperationJournal::Prune {
            version: 1,
            descriptor: entry.descriptor.clone(),
            descriptor_sha256: sha256(&descriptor_bytes),
            recovery_sha256,
        },
        &entry.descriptor.transaction,
    )?;
    recover_recovery_operation(location, None).map(|_| ())
}

fn write_recovery_operation(
    location: &SettingsLocation,
    operation: &RecoveryOperationJournal,
    transaction: &str,
) -> Result<()> {
    let name = cstring(
        location.recovery_operation_name().as_bytes(),
        "bad recovery operation name",
    )?;
    if read_optional(&location.directory, &name)?.is_some() {
        return Err(invalid(
            "Claude settings have an unfinished recovery operation",
        ));
    }
    write_new_atomic(
        &location.directory,
        &name,
        &serde_json::to_vec(operation)
            .map_err(|_| invalid("cannot encode Claude settings recovery operation"))?,
        transaction,
    )
}

fn recover_recovery_operation(
    location: &SettingsLocation,
    reconcile_changed: Option<&str>,
) -> Result<bool> {
    let journal_c = cstring(
        location.recovery_operation_name().as_bytes(),
        "bad recovery operation name",
    )?;
    let Some(bytes) = read_optional(&location.directory, &journal_c)? else {
        return Ok(false);
    };
    make_private(&location.directory, &journal_c)?;
    let operation: RecoveryOperationJournal = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("Claude settings recovery operation is invalid"))?;
    match operation {
        RecoveryOperationJournal::Restore {
            version,
            mut descriptor,
        } => {
            if version != 1
                || descriptor.version != 1
                || descriptor.kind != RecoveryKind::Displaced
                || descriptor.target != location.target_text
            {
                return Err(invalid("Claude settings recovery operation is invalid"));
            }
            validate_transaction_id(&descriptor.transaction)?;
            let names = location.retained_names(&descriptor.transaction)?;
            if descriptor.recovery.as_deref()
                != descriptor.prior_existed.then_some(names.recovery.as_str())
            {
                return Err(invalid("Claude settings recovery operation is invalid"));
            }
            let descriptor_c = cstring(names.descriptor.as_bytes(), "bad descriptor name")?;
            if descriptor.prior_existed {
                let recovery_c = cstring(names.recovery.as_bytes(), "bad recovery name")?;
                let expected_recovery = descriptor
                    .recovery_sha256
                    .clone()
                    .ok_or_else(|| invalid("Claude settings recovery operation is invalid"))?;
                let target = location.read_target()?;
                let recovery = read_optional(&location.directory, &recovery_c)?;
                let target_sha = target.as_deref().map(sha256);
                let recovery_sha = recovery.as_deref().map(sha256);
                if target_sha.as_deref() == Some(descriptor.active_sha256.as_str()) {
                    if recovery_sha.as_deref() != Some(expected_recovery.as_str()) {
                        if reconcile_changed != Some(descriptor.transaction.as_str()) {
                            return Err(invalid(format!(
                                "Claude settings retained bytes changed during restore; reconcile transaction {} first",
                                descriptor.transaction
                            )));
                        }
                        let actual = recovery_sha.ok_or_else(|| {
                            invalid("Claude settings recovery bytes disappeared during restore")
                        })?;
                        if descriptor.original_recovery_sha256.is_none() {
                            descriptor.original_recovery_sha256 =
                                descriptor.recovery_sha256.clone();
                        }
                        descriptor.recovery_sha256 = Some(actual);
                        descriptor.late_write_observed = true;
                        let entry = RecoveryEntry {
                            name: names.descriptor,
                            descriptor,
                        };
                        rewrite_descriptor(location, &entry)?;
                        unlink_optional(&location.directory, &journal_c)?;
                        sync_directory(&location.directory)?;
                        return Ok(true);
                    }
                    atomic_exchange(&location.directory, &location.target, &recovery_c)?;
                    sync_directory(&location.directory)?;
                } else if target_sha.as_deref() != Some(expected_recovery.as_str()) {
                    return Err(invalid(
                        "Claude settings restore state changed; retained files were left untouched",
                    ));
                }
                let expected_displaced = descriptor.active_sha256.clone();
                let active = location
                    .read_target()?
                    .ok_or_else(|| invalid("Claude settings target is missing after restore"))?;
                if sha256(&active) != expected_recovery {
                    return Err(invalid(
                        "Claude settings restore target differs from retained bytes",
                    ));
                }
                make_private(&location.directory, &recovery_c)?;
                let displaced = read_required(&location.directory, &recovery_c)?;
                let displaced_sha = sha256(&displaced);
                descriptor.active_sha256 = expected_recovery;
                descriptor.recovery_sha256 = Some(displaced_sha.clone());
                descriptor.late_write_observed |= displaced_sha != expected_displaced;
                let entry = RecoveryEntry {
                    name: names.descriptor,
                    descriptor,
                };
                rewrite_descriptor(location, &entry)?;
                make_private(&location.directory, &descriptor_c)?;
            } else {
                match location.read_target()? {
                    Some(active) if sha256(&active) == descriptor.active_sha256 => {
                        unlink_optional(&location.directory, &location.target)?;
                        sync_directory(&location.directory)?;
                    }
                    None => {}
                    Some(_) => {
                        return Err(invalid(
                            "Claude settings changed during restore; retained files were left untouched",
                        ));
                    }
                }
                unlink_optional(&location.directory, &descriptor_c)?;
                sync_directory(&location.directory)?;
            }
        }
        RecoveryOperationJournal::Prune {
            version,
            descriptor,
            descriptor_sha256,
            recovery_sha256,
        } => {
            if version != 1 || descriptor.version != 1 || descriptor.target != location.target_text
            {
                return Err(invalid("Claude settings prune operation is invalid"));
            }
            validate_transaction_id(&descriptor.transaction)?;
            let names = location.retained_names(&descriptor.transaction)?;
            let expected_descriptor = names.descriptor;
            if descriptor.recovery.as_deref()
                != match (descriptor.kind, descriptor.prior_existed) {
                    (RecoveryKind::Displaced, true) => Some(names.recovery.as_str()),
                    (RecoveryKind::Displaced, false) => None,
                    (RecoveryKind::Conflict, true) => Some(names.conflict.as_str()),
                    (RecoveryKind::Conflict, false) => {
                        return Err(invalid("Claude settings prune operation is invalid"));
                    }
                }
            {
                return Err(invalid("Claude settings prune operation is invalid"));
            }
            if let Some(name) = descriptor.recovery.as_deref() {
                let recovery_c = cstring(name.as_bytes(), "bad recovery name")?;
                if let Some(retained) = read_optional(&location.directory, &recovery_c)? {
                    let retained_sha256 = sha256(&retained);
                    if recovery_sha256.as_deref() != Some(retained_sha256.as_str()) {
                        return Err(invalid(
                            "Claude settings retained bytes changed during prune",
                        ));
                    }
                    unlink_optional(&location.directory, &recovery_c)?;
                    sync_directory(&location.directory)?;
                }
            } else if recovery_sha256.is_some() {
                return Err(invalid("Claude settings prune operation is invalid"));
            }
            let descriptor_c = cstring(expected_descriptor.as_bytes(), "bad descriptor name")?;
            if let Some(retained) = read_optional(&location.directory, &descriptor_c)? {
                if sha256(&retained) != descriptor_sha256 {
                    return Err(invalid(
                        "Claude settings recovery descriptor changed during prune",
                    ));
                }
                unlink_optional(&location.directory, &descriptor_c)?;
                sync_directory(&location.directory)?;
            }
        }
    }
    unlink_optional(&location.directory, &journal_c)?;
    sync_directory(&location.directory)?;
    Ok(false)
}

fn rewrite_descriptor(location: &SettingsLocation, entry: &RecoveryEntry) -> Result<()> {
    let temporary = format!("{}.rewrite", entry.name);
    let temporary_c = cstring(temporary.as_bytes(), "bad descriptor temporary name")?;
    let name_c = cstring(entry.name.as_bytes(), "bad descriptor name")?;
    write_new(
        &location.directory,
        &temporary_c,
        &serde_json::to_vec(&entry.descriptor)
            .map_err(|_| invalid("cannot encode Claude settings recovery descriptor"))?,
    )?;
    rename_name(&location.directory, &temporary_c, &name_c)?;
    sync_directory(&location.directory)
}

fn open_directory_absolute(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        return Err(invalid("Claude settings root is not absolute"));
    }
    let slash = cstring(b"/", "bad filesystem root")?;
    // SAFETY: `slash` is a live C string and successful ownership transfers to `File`.
    let fd = unsafe {
        libc::open(
            slash.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    let mut directory = owned_file(fd)?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(component) => {
                let name = cstring(component.as_bytes(), "Claude settings root contains NUL")?;
                directory = open_directory_at(&directory, &name)?;
            }
            _ => return Err(invalid("Claude settings root is not normalized")),
        }
    }
    Ok(directory)
}

fn open_or_create_directory(parent: &File, name: &CString) -> Result<File> {
    match open_directory_at(parent, name) {
        Ok(file) => Ok(file),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            // SAFETY: both descriptor and name remain live for this call.
            let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
            if result == -1 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            } else {
                sync_directory(parent)?;
            }
            open_directory_at(parent, name)
        }
        Err(error) => Err(error),
    }
}

fn open_directory_at(parent: &File, name: &CString) -> Result<File> {
    // SAFETY: both descriptor and name remain live; successful ownership transfers to `File`.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned_file(fd)
}

fn read_optional(parent: &File, name: &CString) -> Result<Option<Vec<u8>>> {
    // SAFETY: both descriptor and name remain live; successful ownership transfers to `File`.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd == -1 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    // SAFETY: `fd` is newly owned by this process.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(invalid(
            "Claude settings retained file is not a bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid("Claude settings retained file is too large"));
    }
    Ok(Some(bytes))
}

fn read_required(parent: &File, name: &CString) -> Result<Vec<u8>> {
    read_optional(parent, name)?.ok_or_else(|| invalid("Claude settings retained file is missing"))
}

fn write_new(parent: &File, name: &CString, bytes: &[u8]) -> Result<()> {
    // SAFETY: both descriptor and name remain live; successful ownership transfers to `File`.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    let mut file = owned_file(fd)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_new_atomic(parent: &File, name: &CString, bytes: &[u8], suffix: &str) -> Result<()> {
    let name_text = name
        .to_str()
        .map_err(|_| invalid("Claude settings retained name is not UTF-8"))?;
    let stage = format!("{name_text}.stage-{suffix}");
    let stage_c = cstring(stage.as_bytes(), "bad retained staging name")?;
    write_new(parent, &stage_c, bytes)?;
    if let Err(error) = link_name(parent, &stage_c, name) {
        unlink_optional(parent, &stage_c)?;
        return Err(error);
    }
    unlink_optional(parent, &stage_c)?;
    sync_directory(parent)
}

fn cleanup_stages(location: &SettingsLocation) -> Result<()> {
    for name in directory_names(&location.directory)? {
        if is_owned_stage(location, &name) {
            unlink_optional(
                &location.directory,
                &cstring(name.as_bytes(), "bad retained staging name")?,
            )?;
        }
    }
    sync_directory(&location.directory)
}

fn is_owned_stage(location: &SettingsLocation, name: &str) -> bool {
    let journal_prefix = format!("{}.stage-", location.journal_name());
    if let Some(transaction) = name.strip_prefix(&journal_prefix) {
        return validate_transaction_id(transaction).is_ok();
    }
    let recovery_operation_prefix = format!("{}.stage-", location.recovery_operation_name());
    if let Some(transaction) = name.strip_prefix(&recovery_operation_prefix) {
        return validate_transaction_id(transaction).is_ok();
    }
    let prefix = location.recovery_prefix();
    let Some(rest) = name.strip_prefix(&prefix) else {
        return false;
    };
    let Some((transaction, stage_transaction)) = rest.split_once(".json.stage-") else {
        return false;
    };
    transaction == stage_transaction && validate_transaction_id(transaction).is_ok()
}

fn make_private(parent: &File, name: &CString) -> Result<()> {
    // SAFETY: the retained parent and single-component name remain live for this call.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    let file = owned_file(fd)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(invalid(
            "Claude settings retained file is not a bounded regular file",
        ));
    }
    // SAFETY: `file` owns a regular-file descriptor.
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o777 != 0o600 {
            return Err(invalid("Claude settings retained file is not private"));
        }
    }
    Ok(())
}

fn link_name(parent: &File, source: &CString, target: &CString) -> Result<()> {
    // SAFETY: descriptor and names remain live for the call.
    let result = unsafe {
        libc::linkat(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            target.as_ptr(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Err(invalid(
                "Claude settings appeared during capture configuration",
            ))
        } else {
            Err(error.into())
        }
    }
}

fn rename_name(parent: &File, source: &CString, target: &CString) -> Result<()> {
    // SAFETY: descriptor and names remain live for the call.
    let result = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            target.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn unlink_optional(parent: &File, name: &CString) -> Result<()> {
    // SAFETY: descriptor and name remain live for the call.
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error.into())
        }
    }
}

fn sync_directory(directory: &File) -> Result<()> {
    directory.sync_all().map_err(Into::into)
}

fn directory_names(directory: &File) -> Result<Vec<String>> {
    let current = c".";
    // SAFETY: `directory` and `current` remain live. Opening `.` creates a new file description
    // with its own directory offset; duplicating the retained fd would share a consumed offset.
    let scan = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            current.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if scan == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: scan is an owned directory descriptor.
    let stream = unsafe { libc::fdopendir(scan) };
    if stream.is_null() {
        // SAFETY: fdopendir did not consume the descriptor on failure.
        unsafe { libc::close(scan) };
        return Err(std::io::Error::last_os_error().into());
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: stream is valid until closed below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: d_name is NUL-terminated for the live entry.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_str()
            .map_err(|_| invalid("Claude settings directory contains a non-UTF-8 entry"))?;
        if name != "." && name != ".." {
            names.push(name.to_owned());
            if names.len() > MAX_DIRECTORY_ENTRIES {
                // SAFETY: stream remains valid.
                unsafe { libc::closedir(stream) };
                return Err(invalid("Claude settings directory has too many entries"));
            }
        }
    }
    // SAFETY: stream is valid and closed exactly once.
    if unsafe { libc::closedir(stream) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(names)
}

fn owned_file(fd: libc::c_int) -> Result<File> {
    if fd == -1 {
        Err(std::io::Error::last_os_error().into())
    } else {
        // SAFETY: the successful syscall returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn cstring(bytes: &[u8], message: &str) -> Result<CString> {
    CString::new(bytes).map_err(|_| invalid(message))
}

fn random_id() -> Result<String> {
    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).map_err(|_| invalid("random source unavailable"))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn validate_transaction_id(value: &str) -> Result<()> {
    if value.len() == 24
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(invalid("Claude settings recovery transaction is invalid"))
    }
}

#[cfg(target_os = "macos")]
fn atomic_exchange(parent: &File, left: &CString, right: &CString) -> Result<()> {
    // SAFETY: descriptor and names remain live for the call.
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            left.as_ptr(),
            parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(target_os = "linux")]
fn atomic_exchange(parent: &File, left: &CString, right: &CString) -> Result<()> {
    // SAFETY: descriptor and names remain live for the syscall.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            left.as_ptr(),
            parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn atomic_exchange(_parent: &File, _left: &CString, _right: &CString) -> Result<()> {
    Err(invalid(
        "safe conditional Claude settings replacement is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        RecoveryDescriptor, RecoveryKind, RecoveryOperationJournal, SettingsLocation,
        TransactionJournal, atomic_exchange, recover, recovery_entries, replace, sha256,
        sync_directory, write_recovery_operation,
    };
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn retained_inode_detects_late_editor_write_and_explicit_restore_preserves_it() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let settings = root_path.as_path().join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"before").unwrap();
        let mut editor = std::fs::OpenOptions::new()
            .write(true)
            .open(&settings)
            .unwrap();
        let location = SettingsLocation::open(root_path.as_path(), &settings).unwrap();
        replace(&location, Some(b"before"), b"installed", true).unwrap();
        editor.seek(SeekFrom::Start(0)).unwrap();
        editor.write_all(b"late!!").unwrap();
        editor.sync_all().unwrap();
        assert!(replace(&location, Some(b"installed"), b"again", true).is_err());
        let listed = recover(&location, None, false, None).unwrap();
        let id = listed["recoveries"][0]["transaction"]
            .as_str()
            .unwrap()
            .to_owned();
        recover(&location, Some(&id), false, None).unwrap();
        assert_eq!(std::fs::read(&settings).unwrap(), b"late!!");
    }

    #[test]
    fn retained_directory_descriptor_prevents_ancestor_symlink_redirection() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let settings = root_path.as_path().join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"before").unwrap();
        let location = SettingsLocation::open(root_path.as_path(), &settings).unwrap();
        std::fs::rename(
            root_path.as_path().join(".claude"),
            root_path.as_path().join("moved"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), root_path.as_path().join(".claude")).unwrap();
        replace(&location, Some(b"before"), b"installed", true).unwrap();
        assert_eq!(
            std::fs::read(root_path.as_path().join("moved/settings.local.json")).unwrap(),
            b"installed"
        );
        assert!(!outside.path().join("settings.local.json").exists());
    }

    #[test]
    fn opening_settings_rejects_a_symlinked_checkout_ancestor() {
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("checkout/.claude")).unwrap();
        let alias = parent.path().join("alias");
        std::os::unix::fs::symlink(outside.path(), &alias).unwrap();
        let root = alias.join("checkout");
        let settings = root.join(".claude/settings.local.json");

        assert!(SettingsLocation::open(&root, &settings).is_err());
        assert!(
            !outside
                .path()
                .join("checkout/.claude/settings.local.json")
                .exists()
        );
    }

    #[test]
    fn bounded_history_reserves_the_last_slot_for_hook_removal() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let settings = root_path.as_path().join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"version-0").unwrap();
        let location = SettingsLocation::open(root_path.as_path(), &settings).unwrap();
        for version in 1..=7 {
            let prior = format!("version-{}", version - 1);
            let next = format!("version-{version}");
            replace(&location, Some(prior.as_bytes()), next.as_bytes(), true).unwrap();
        }
        assert!(replace(&location, Some(b"version-7"), b"configure-blocked", true).is_err());
        replace(&location, Some(b"version-7"), b"hooks-removed", false).unwrap();
        assert_eq!(std::fs::read(&settings).unwrap(), b"hooks-removed");
        assert!(replace(&location, Some(b"hooks-removed"), b"ninth", false).is_err());
        let report = recover(&location, None, true, None).unwrap();
        assert_eq!(report["pruned"], 8);
        replace(&location, Some(b"hooks-removed"), b"reconfigured", true).unwrap();
    }

    #[test]
    fn concurrent_target_change_is_retained_as_bounded_explicit_conflict() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let settings = root_path.as_path().join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"before").unwrap();
        let location = SettingsLocation::open(root_path.as_path(), &settings).unwrap();
        std::fs::write(&settings, b"editor").unwrap();

        assert!(replace(&location, Some(b"before"), b"installed", true).is_err());
        assert_eq!(std::fs::read(&settings).unwrap(), b"editor");
        let listed = recover(&location, None, false, None).unwrap();
        assert_eq!(listed["recoveries"].as_array().unwrap().len(), 1);
        assert_eq!(listed["recoveries"][0]["kind"], "conflict");
        assert_eq!(listed["recoveries"][0]["late_write"], false);

        let pruned = recover(&location, None, true, None).unwrap();
        assert_eq!(pruned["pruned"], 1);
        assert!(pruned["recoveries"].as_array().unwrap().is_empty());
    }

    #[test]
    fn retained_names_reject_path_injection_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let settings = root_path.as_path().join(".claude/settings.local.json");
        let outside = root_path.as_path().join("outside");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"installed").unwrap();
        std::fs::write(&outside, b"preserve").unwrap();
        let location = SettingsLocation::open(root_path.as_path(), &settings).unwrap();
        let transaction = "0123456789abcdef01234567";
        let names = location.retained_names(transaction).unwrap();
        let descriptor = RecoveryDescriptor {
            version: 1,
            transaction: transaction.into(),
            target: "settings.local.json".into(),
            recovery: Some("../outside".into()),
            prior_existed: true,
            active_sha256: sha256(b"installed"),
            recovery_sha256: Some(sha256(b"preserve")),
            original_recovery_sha256: Some(sha256(b"preserve")),
            late_write_observed: false,
            kind: RecoveryKind::Displaced,
        };
        std::fs::write(
            settings.parent().unwrap().join(&names.descriptor),
            serde_json::to_vec(&descriptor).unwrap(),
        )
        .unwrap();
        assert!(recover(&location, None, true, None).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"preserve");
        std::fs::remove_file(settings.parent().unwrap().join(&names.descriptor)).unwrap();

        let journal = TransactionJournal {
            version: 1,
            transaction: transaction.into(),
            target: "settings.local.json".into(),
            temp: "../outside".into(),
            recovery: Some(names.recovery),
            descriptor: names.descriptor,
            prior_existed: true,
            expected_sha256: sha256(b"before"),
            replacement_sha256: sha256(b"installed"),
        };
        std::fs::write(
            settings.parent().unwrap().join(location.journal_name()),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        assert!(recover(&location, None, false, None).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"preserve");
    }

    #[test]
    fn crash_late_write_requires_reconcile_and_preserves_both_versions() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let settings = root_path.as_path().join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"installed").unwrap();
        let location = SettingsLocation::open(root_path.as_path(), &settings).unwrap();
        let transaction = "89abcdef0123456789abcdef";
        let names = location.retained_names(transaction).unwrap();
        std::fs::write(
            settings.parent().unwrap().join(&names.recovery),
            b"late-write",
        )
        .unwrap();
        std::fs::set_permissions(
            settings.parent().unwrap().join(&names.recovery),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let journal = TransactionJournal {
            version: 1,
            transaction: transaction.into(),
            target: "settings.local.json".into(),
            temp: names.temp,
            recovery: Some(names.recovery.clone()),
            descriptor: names.descriptor,
            prior_existed: true,
            expected_sha256: sha256(b"before"),
            replacement_sha256: sha256(b"installed"),
        };
        std::fs::write(
            settings.parent().unwrap().join(location.journal_name()),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();

        let inspected = recover(&location, None, false, None).unwrap();
        assert_eq!(inspected["unfinished_transaction"], transaction);
        assert!(replace(&location, Some(b"installed"), b"again", false).is_err());
        let reconciled = recover(&location, None, false, Some(transaction)).unwrap();
        assert!(reconciled["unfinished_transaction"].is_null());
        assert_eq!(reconciled["recoveries"][0]["late_write"], true);
        assert_eq!(
            std::fs::metadata(settings.parent().unwrap().join(&names.recovery))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::write(
            settings.parent().unwrap().join(&names.recovery),
            b"later-write",
        )
        .unwrap();
        assert!(replace(&location, Some(b"installed"), b"hooks-removed", false).is_err());
        recover(&location, None, false, Some(transaction)).unwrap();
        replace(&location, Some(b"installed"), b"hooks-removed", false).unwrap();
        assert_eq!(std::fs::read(&settings).unwrap(), b"hooks-removed");
        assert_eq!(
            std::fs::read(settings.parent().unwrap().join(&names.recovery)).unwrap(),
            b"later-write"
        );
    }

    #[test]
    fn displaced_settings_inode_is_made_private() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let settings = root_path.as_path().join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"before").unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o644)).unwrap();
        let location = SettingsLocation::open(root_path.as_path(), &settings).unwrap();
        replace(&location, Some(b"before"), b"installed", true).unwrap();
        let listed = recover(&location, None, false, None).unwrap();
        let transaction = listed["recoveries"][0]["transaction"].as_str().unwrap();
        let names = location.retained_names(transaction).unwrap();
        let recovery = names.recovery;
        assert_eq!(
            std::fs::metadata(settings.parent().unwrap().join(&recovery))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::set_permissions(
            settings.parent().unwrap().join(&recovery),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::set_permissions(
            settings.parent().unwrap().join(&names.descriptor),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        recover(&location, None, false, None).unwrap();
        assert_eq!(
            std::fs::metadata(settings.parent().unwrap().join(&recovery))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(settings.parent().unwrap().join(names.descriptor))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn restore_crash_after_exchange_recovers_both_orientations_and_late_bytes() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let settings = root_path.as_path().join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"before").unwrap();
        let location = SettingsLocation::open(root_path.as_path(), &settings).unwrap();
        replace(&location, Some(b"before"), b"installed", true).unwrap();
        let entry = recovery_entries(&location).unwrap().remove(0);
        write_recovery_operation(
            &location,
            &RecoveryOperationJournal::Restore {
                version: 1,
                descriptor: entry.descriptor.clone(),
            },
            &entry.descriptor.transaction,
        )
        .unwrap();
        let recovery = location
            .retained_names(&entry.descriptor.transaction)
            .unwrap()
            .recovery;
        let recovery_c = super::cstring(recovery.as_bytes(), "bad recovery").unwrap();
        atomic_exchange(&location.directory, &location.target, &recovery_c).unwrap();
        sync_directory(&location.directory).unwrap();
        std::fs::write(
            settings.parent().unwrap().join(&recovery),
            b"late-installed",
        )
        .unwrap();

        let report = recover(&location, None, false, None).unwrap();
        assert_eq!(std::fs::read(&settings).unwrap(), b"before");
        assert_eq!(report["recoveries"][0]["late_write"], true);
        assert_eq!(
            std::fs::read(settings.parent().unwrap().join(recovery)).unwrap(),
            b"late-installed"
        );
    }

    #[test]
    fn restore_journal_with_pre_exchange_late_write_requires_exact_reconcile() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let settings = root_path.as_path().join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"before").unwrap();
        let location = SettingsLocation::open(root_path.as_path(), &settings).unwrap();
        replace(&location, Some(b"before"), b"installed", true).unwrap();
        let entry = recovery_entries(&location).unwrap().remove(0);
        write_recovery_operation(
            &location,
            &RecoveryOperationJournal::Restore {
                version: 1,
                descriptor: entry.descriptor.clone(),
            },
            &entry.descriptor.transaction,
        )
        .unwrap();
        let recovery = location
            .retained_names(&entry.descriptor.transaction)
            .unwrap()
            .recovery;
        std::fs::write(
            settings.parent().unwrap().join(recovery),
            b"late-before-exchange",
        )
        .unwrap();

        assert!(recover(&location, None, false, None).is_err());
        recover(&location, None, false, Some(&entry.descriptor.transaction)).unwrap();
        recover(&location, Some(&entry.descriptor.transaction), false, None).unwrap();
        assert_eq!(std::fs::read(&settings).unwrap(), b"late-before-exchange");
    }

    #[test]
    fn prune_journal_completes_after_recovery_bytes_were_unlinked() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let settings = root_path.as_path().join(".claude/settings.local.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, b"before").unwrap();
        let location = SettingsLocation::open(root_path.as_path(), &settings).unwrap();
        replace(&location, Some(b"before"), b"installed", true).unwrap();
        let entry = recovery_entries(&location).unwrap().remove(0);
        let names = location
            .retained_names(&entry.descriptor.transaction)
            .unwrap();
        let descriptor_bytes =
            std::fs::read(settings.parent().unwrap().join(&names.descriptor)).unwrap();
        let recovery_bytes =
            std::fs::read(settings.parent().unwrap().join(&names.recovery)).unwrap();
        write_recovery_operation(
            &location,
            &RecoveryOperationJournal::Prune {
                version: 1,
                descriptor: entry.descriptor.clone(),
                descriptor_sha256: sha256(&descriptor_bytes),
                recovery_sha256: Some(sha256(&recovery_bytes)),
            },
            &entry.descriptor.transaction,
        )
        .unwrap();
        std::fs::remove_file(settings.parent().unwrap().join(&names.recovery)).unwrap();
        sync_directory(&location.directory).unwrap();

        let report = recover(&location, None, false, None).unwrap();
        assert!(report["recoveries"].as_array().unwrap().is_empty());
        assert!(!settings.parent().unwrap().join(&names.descriptor).exists());
        assert!(
            !settings
                .parent()
                .unwrap()
                .join(location.recovery_operation_name())
                .exists()
        );
    }
}
