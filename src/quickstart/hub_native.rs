//! A per-user macOS Hub managed by launchd.
use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::SlackBundle;
use super::hub_setup::{self, Runtime, Started};
use super::manifest::{Manifest, sha_hex};
use super::process::{diagnostic, run};
use crate::prompt::Ui;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) label: String,
    pub(crate) plist: PathBuf,
    pub(crate) config_dir: PathBuf,
    pub(crate) data_dir: PathBuf,
    pub(crate) executable: PathBuf,
    pub(crate) executable_sha256: String,
    pub(crate) plist_sha256: String,
    pub(crate) port: u16,
    pub(crate) project: String,
    #[serde(default)]
    pub(crate) environment: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) slack_files: BTreeMap<String, String>,
}

pub(crate) struct Options {
    pub(crate) executable: PathBuf,
    pub(crate) port: u16,
    pub(crate) project: String,
    pub(crate) slack: Option<SlackBundle>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    project: String,
    environment: BTreeMap<String, String>,
    #[serde(default)]
    slack_files: BTreeMap<String, String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum GithubUpdatePhase {
    Rollback,
    Commit,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingGithubUpdate {
    phase: GithubUpdatePhase,
    prior_record: Record,
    next_record: Record,
    prior_plist: String,
    prior_receipt: String,
}

fn service_label(home: &Path, project: &str) -> String {
    let home_id = &sha_hex(home.as_os_str().as_encoded_bytes())[..10];
    format!("com.counterbranch.agentscm.{project}.{home_id}")
}

fn receipt_path(config_dir: &Path) -> PathBuf {
    config_dir.join("retained.json")
}

fn github_update_path(config_dir: &Path) -> PathBuf {
    config_dir.join(".github-update.json")
}

fn github_update_exists(config_dir: &Path) -> Result<bool, String> {
    match github_update_path(config_dir).symlink_metadata() {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "Could not inspect the native GitHub configuration recovery record: {error}"
        )),
    }
}

fn receipt_for(record: &Record) -> Receipt {
    Receipt {
        project: record.project.clone(),
        environment: record.environment.clone(),
        slack_files: record.slack_files.clone(),
    }
}

fn restore_private_file(path: &Path, prior: &Option<Vec<u8>>) -> Result<(), String> {
    match prior {
        Some(bytes) => {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| "The previous private file is not UTF-8.".to_owned())?;
            atomic_private(path, text)
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => super::sync_dir(path.parent().ok_or("private file has no parent")?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        },
    }
}

fn rollback_slack_install(
    swap: &mut Option<SlackSwap>,
    receipt: &Path,
    prior_receipt: &Option<Vec<u8>>,
) -> Result<(), String> {
    if let Some(swap) = swap.as_mut() {
        swap.rollback()
    } else {
        restore_private_file(receipt, prior_receipt)
    }
}

struct NativeRollback<'a> {
    swap: &'a mut Option<SlackSwap>,
    receipt: &'a Path,
    prior_receipt: &'a Option<Vec<u8>>,
    plist: &'a Path,
    prior_plist: &'a Option<Vec<u8>>,
    manifest: &'a mut Manifest,
    prior_record: &'a Option<Record>,
    /// A first install's receipt to keep, so a rerun adopts its data directory as after an
    /// uninstall that keeps data, instead of refusing a directory nothing owns.
    retain: &'a Option<Receipt>,
    restart_previous: bool,
}

fn append_restart_result<F>(errors: &mut Vec<String>, restart: bool, restart_previous: F)
where
    F: FnOnce() -> Result<(), String>,
{
    if errors.is_empty()
        && restart
        && let Err(error) = restart_previous()
    {
        errors.push(format!("restarting the previous Hub failed: {error}"));
    }
}

fn validate_native_restoration(recovery: &NativeRollback<'_>) -> Result<(), String> {
    let receipt = match recovery.prior_receipt {
        Some(expected) => {
            let actual = std::fs::read(recovery.receipt).map_err(|error| error.to_string())?;
            if &actual != expected {
                return Err("The previous native Hub receipt was not restored exactly.".into());
            }
            Some(actual)
        }
        None => {
            if let Some(retained) = recovery.retain {
                let config = recovery.receipt.parent().ok_or("receipt has no parent")?;
                if &load_receipt(config, &retained.project)? != retained {
                    return Err("The first install's receipt was not kept.".into());
                }
            } else if recovery.receipt.symlink_metadata().is_ok() {
                return Err("The new native Hub receipt remains after rollback.".into());
            }
            None
        }
    };
    match recovery.prior_plist {
        Some(expected) => {
            if std::fs::read(recovery.plist)
                .map(|actual| actual != *expected)
                .unwrap_or(true)
            {
                return Err("The previous native Hub LaunchAgent was not restored exactly.".into());
            }
        }
        None if recovery.plist.symlink_metadata().is_ok() => {
            return Err("The new native Hub LaunchAgent remains after rollback.".into());
        }
        None => {}
    }
    if &recovery.manifest.native_hub != recovery.prior_record {
        return Err("The previous native Hub installation record was not restored.".into());
    }
    if let Some(record) = recovery.prior_record {
        validate_slack_state(&record.data_dir, &record.environment, &record.slack_files)?;
        let expected = Receipt {
            project: record.project.clone(),
            environment: record.environment.clone(),
            slack_files: record.slack_files.clone(),
        };
        if load_receipt(&record.config_dir, &record.project)? != expected
            || receipt.is_none()
            || super::manifest::file_hash(recovery.plist).as_deref() != Some(&record.plist_sha256)
        {
            return Err(
                "The previous native Hub generation did not validate after rollback.".into(),
            );
        }
    }
    Ok(())
}

fn restore_native_update<F>(
    recovery: NativeRollback<'_>,
    cause: impl Into<String>,
    restart_previous: F,
) -> String
where
    F: FnOnce() -> Result<(), String>,
{
    restore_native_update_with(recovery, cause, save_manifest_durable, restart_previous)
}

fn restore_native_update_with<S, F>(
    recovery: NativeRollback<'_>,
    cause: impl Into<String>,
    save_manifest: S,
    restart_previous: F,
) -> String
where
    S: FnOnce(&mut Manifest) -> Result<(), String>,
    F: FnOnce() -> Result<(), String>,
{
    let cause = cause.into();
    let mut errors = Vec::new();
    if let Err(error) =
        rollback_slack_install(recovery.swap, recovery.receipt, recovery.prior_receipt)
    {
        errors.push(format!(
            "restoring the Slack bundle and receipt failed: {error}"
        ));
    }
    if let Err(error) = restore_private_file(recovery.plist, recovery.prior_plist) {
        errors.push(format!(
            "restoring the previous LaunchAgent failed: {error}"
        ));
    }
    if let Some(retained) = recovery.retain {
        let kept = serde_json::to_string_pretty(retained)
            .map_err(|_| "cannot encode native Hub ownership receipt".to_owned())
            .and_then(|text| atomic_private(recovery.receipt, &format!("{text}\n")));
        if let Err(error) = kept {
            errors.push(format!(
                "keeping the first install's receipt failed: {error}"
            ));
        }
    }
    recovery.manifest.native_hub = recovery.prior_record.clone();
    if let Err(error) = save_manifest(recovery.manifest) {
        errors.push(format!(
            "restoring the previous installation record failed: {error}"
        ));
    }
    if errors.is_empty()
        && let Err(error) = validate_native_restoration(&recovery)
    {
        errors.push(format!(
            "validating the restored native Hub failed: {error}"
        ));
    }
    append_restart_result(&mut errors, recovery.restart_previous, restart_previous);
    if errors.is_empty() {
        format!("{cause} The previous native Hub configuration was restored.")
    } else {
        format!("{cause}; recovery also failed: {}", errors.join("; "))
    }
}

fn environment_ok(environment: &BTreeMap<String, String>) -> bool {
    const KEYS: [&str; 7] = [
        "AGENTSCM_DATABASE_URL",
        "AGENTSCM_GITHUB_APP_NAME",
        "AGENTSCM_GITHUB_APP_ORG",
        "AGENTSCM_GITHUB_CLIENT_ID",
        "AGENTSCM_GITHUB_CLIENT_SECRET_FILE",
        "AGENTSCM_ADMINS",
        "AGENTSCM_SLACK_FILE",
    ];
    environment.iter().all(|(key, value)| {
        KEYS.contains(&key.as_str()) && !value.is_empty() && !value.chars().any(char::is_control)
    })
}

fn private_owned_dir(path: &Path, label: &str) -> Result<(), String> {
    match path.symlink_metadata() {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(format!(
                    "The native Hub {label} is not a regular directory."
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.permissions().mode() & 0o777 != 0o700
                {
                    return Err(format!(
                        "The native Hub {label} is not owned privately by the current user."
                    ));
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            super::private_dir(path).map_err(|error| error.to_string())?;
            #[cfg(unix)]
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| error.to_string())?;
            private_owned_dir(path, label)
        }
        Err(error) => Err(error.to_string()),
    }
}

fn validate_receipt(receipt: &Receipt, config_dir: &Path, project: &str) -> Result<(), String> {
    if receipt.project != project || !environment_ok(&receipt.environment) {
        return Err("The retained native Hub receipt does not match this project.".into());
    }
    let data = config_dir.join("data");
    if receipt.environment.get("AGENTSCM_DATABASE_URL")
        != Some(&format!("sqlite://{}", data.join("hub.sqlite").display()))
    {
        return Err(
            "The retained native Hub database path does not match its data directory.".into(),
        );
    }
    if let Some(secret) = receipt
        .environment
        .get("AGENTSCM_GITHUB_CLIENT_SECRET_FILE")
    {
        let secret = Path::new(secret);
        let name = secret
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if secret.parent() != Some(data.join("secrets").as_path())
            || !name.strip_suffix(".client_secret").is_some_and(|id| {
                id.starts_with("con_")
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
        {
            return Err(
                "The retained native Hub secret path does not match its data directory.".into(),
            );
        }
    }
    validate_slack_state(&data, &receipt.environment, &receipt.slack_files)?;
    Ok(())
}

fn load_receipt(config_dir: &Path, project: &str) -> Result<Receipt, String> {
    let path = receipt_path(config_dir);
    let metadata = path
        .symlink_metadata()
        .map_err(|_| "Retained native Hub data has no ownership receipt; refusing to adopt it.")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("The retained native Hub receipt is not a regular file.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(
                "The retained native Hub receipt is not privately owned by the current user (expected mode 0600).".into(),
            );
        }
    }
    let receipt: Receipt =
        serde_json::from_slice(&std::fs::read(&path).map_err(|error| error.to_string())?)
            .map_err(|_| "The retained native Hub receipt is invalid.".to_owned())?;
    validate_receipt(&receipt, config_dir, project)?;
    Ok(receipt)
}

fn valid_slack_leaf(name: &str) -> bool {
    matches!(name, "config.json" | "signing-secret" | "bot-token")
        || name
            .strip_prefix("agent-")
            .and_then(|rest| rest.strip_suffix(".token"))
            .is_some_and(|index| {
                index
                    .parse::<usize>()
                    .is_ok_and(|number| index == number.to_string())
            })
}

fn validate_slack_state(
    data_dir: &Path,
    environment: &BTreeMap<String, String>,
    files: &BTreeMap<String, String>,
) -> Result<(), String> {
    let expected_dir = data_dir.join("secrets").join("slack");
    let expected_config = expected_dir.join("config.json");
    let configured = environment.get("AGENTSCM_SLACK_FILE");
    if configured.is_none() && files.is_empty() {
        return Ok(());
    }
    if configured != Some(&expected_config.display().to_string()) || files.is_empty() {
        return Err("The retained native Hub Slack receipt is invalid.".into());
    }
    let metadata = expected_dir
        .symlink_metadata()
        .map_err(|_| "The recorded native Hub Slack bundle is missing.".to_owned())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("The recorded native Hub Slack directory is invalid.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("The recorded native Hub Slack directory is not private.".into());
        }
    }
    let report =
        agentscm::hub::slack::setup::check(&expected_config).map_err(|error| error.to_string())?;
    let mut expected: BTreeSet<String> = ["config.json", "signing-secret", "bot-token"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    expected.extend((0..report.binding_count).map(|index| format!("agent-{index}.token")));
    if files.keys().cloned().collect::<BTreeSet<_>>() != expected {
        return Err("The retained native Hub Slack receipt has an invalid file set.".into());
    }
    let actual = std::fs::read_dir(&expected_dir)
        .map_err(|error| error.to_string())?
        .map(|entry| {
            entry
                .map_err(|error| error.to_string())?
                .file_name()
                .into_string()
                .map_err(|_| "A native Hub Slack file name is invalid.".to_owned())
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let journals: BTreeSet<String> = ["install.sqlite", "install.sqlite-wal", "install.sqlite-shm"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    if !actual.is_superset(&expected)
        || !actual
            .difference(&expected)
            .all(|name| journals.contains(name))
    {
        return Err("The native Hub Slack directory contains an unowned file.".into());
    }
    for name in actual.intersection(&journals) {
        let metadata = expected_dir
            .join(name)
            .symlink_metadata()
            .map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("The native Hub Slack journal is invalid.".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err("The native Hub Slack journal is not private.".into());
            }
        }
    }
    for (name, digest) in files {
        let path = expected_dir.join(name);
        let metadata = path
            .symlink_metadata()
            .map_err(|_| "A recorded native Hub Slack file is missing.".to_owned())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("A recorded native Hub Slack file is invalid.".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("A recorded native Hub Slack file is not private.".into());
            }
        }
        if super::manifest::file_hash(&path).as_deref() != Some(digest) {
            return Err("The recorded native Hub Slack bundle has drifted.".into());
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingSlack {
    old_files: BTreeMap<String, String>,
    new_files: BTreeMap<String, String>,
    receipt_present: bool,
    plist: PathBuf,
    plist_present: bool,
    #[serde(default)]
    old_record: Option<Record>,
}

#[derive(Debug)]
struct SlackSwap {
    target: PathBuf,
    next: PathBuf,
    backup: PathBuf,
    old_files: BTreeMap<String, String>,
    new_files: BTreeMap<String, String>,
    receipt: PathBuf,
    prior_receipt: Option<Vec<u8>>,
    active: bool,
}

impl SlackSwap {
    fn retain_for_recovery(&mut self) {
        self.active = false;
    }

    fn rollback(&mut self) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }
        atomic_private(&self.backup.join("rollback"), "")?;
        for (name, digest) in &self.old_files {
            let backup = self.backup.join(name);
            let metadata = backup
                .symlink_metadata()
                .map_err(|_| "A preserved native Hub Slack file is missing.".to_owned())?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || super::manifest::file_hash(&backup).as_deref() != Some(digest)
            {
                return Err("A preserved native Hub Slack file is invalid.".into());
            }
        }
        for name in self.new_files.keys() {
            match std::fs::remove_file(self.target.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        for name in self.old_files.keys() {
            let backup = self.backup.join(name);
            let text = std::fs::read_to_string(&backup).map_err(|error| error.to_string())?;
            atomic_private(&self.target.join(name), &text)?;
        }
        restore_private_file(&self.receipt, &self.prior_receipt)?;
        std::fs::remove_dir_all(&self.next).ok();
        if self.old_files.is_empty() {
            // A first install's rollback also drops the journal its Hub wrote, so a retry
            // finds no Slack directory without an ownership record.
            for journal in ["install.sqlite", "install.sqlite-wal", "install.sqlite-shm"] {
                match std::fs::remove_file(self.target.join(journal)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
            match self.target.read_dir() {
                Ok(entries) => {
                    if entries.count() == 0 {
                        std::fs::remove_dir(&self.target).map_err(|error| error.to_string())?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        super::sync_dir(
            self.target
                .parent()
                .ok_or("native Slack target has no parent")?,
        )?;
        if self.target.exists() {
            super::sync_dir(&self.target)?;
        }
        self.active = false;
        Ok(())
    }

    fn commit(&mut self) -> Result<(), String> {
        // The published files are already validated and recorded before commit. From here
        // recovery is forward-only: a cleanup failure leaves the durable pending record for the
        // next quickstart instead of rolling an active service back from Drop.
        self.active = false;
        std::fs::remove_dir_all(&self.next).ok();
        std::fs::remove_dir_all(&self.backup).map_err(|error| error.to_string())?;
        super::sync_dir(
            self.backup
                .parent()
                .ok_or("native Slack backup has no parent")?,
        )?;
        Ok(())
    }
}

impl Drop for SlackSwap {
    fn drop(&mut self) {
        let _ = self.rollback();
    }
}

fn recover_pending_slack(
    data_dir: &Path,
    receipt_file: &Path,
    expected_plist: &Path,
    manifest: &mut Manifest,
) -> Result<bool, String> {
    recover_pending_slack_with(
        data_dir,
        receipt_file,
        expected_plist,
        manifest,
        save_manifest_durable,
    )
}

fn recover_pending_slack_with<F>(
    data_dir: &Path,
    receipt_file: &Path,
    expected_plist: &Path,
    manifest: &mut Manifest,
    save_manifest: F,
) -> Result<bool, String>
where
    F: FnOnce(&mut Manifest) -> Result<(), String>,
{
    let target = data_dir.join("secrets/slack");
    let parent = data_dir.join("secrets");
    let next = parent.join(".slack-next");
    let backup = parent.join(".slack-rollback");
    if parent.symlink_metadata().is_ok() {
        private_owned_dir(&parent, "secrets directory")?;
    }
    if !backup.exists() {
        if next.exists() {
            private_owned_dir(&next, "pending Slack staging directory")?;
            std::fs::remove_dir_all(next).map_err(|error| error.to_string())?;
        }
        return Ok(false);
    }
    private_owned_dir(&backup, "pending Slack rollback directory")?;
    if !backup.join("pending.json").exists() {
        std::fs::remove_dir_all(&backup).map_err(|error| error.to_string())?;
        if next.exists() {
            std::fs::remove_dir_all(next).map_err(|error| error.to_string())?;
        }
        return Ok(false);
    }
    let pending: PendingSlack = serde_json::from_slice(
        &std::fs::read(backup.join("pending.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|_| "The pending native Hub Slack update is invalid.".to_owned())?;
    if pending
        .old_files
        .keys()
        .chain(pending.new_files.keys())
        .any(|name| !valid_slack_leaf(name))
    {
        return Err("The pending native Hub Slack update has invalid file names.".into());
    }
    if pending.plist != expected_plist
        || pending
            .old_record
            .as_ref()
            .is_some_and(|record| record.plist != pending.plist)
    {
        return Err("The pending native Hub Slack update has an invalid LaunchAgent path.".into());
    }
    if !backup.join("ready").exists() {
        std::fs::remove_dir_all(&backup).map_err(|error| error.to_string())?;
        if next.exists() {
            std::fs::remove_dir_all(next).map_err(|error| error.to_string())?;
        }
        return Ok(false);
    }
    let ready = backup
        .join("ready")
        .symlink_metadata()
        .map_err(|error| error.to_string())?;
    if !ready.is_file() || ready.file_type().is_symlink() {
        return Err("The pending native Hub Slack update marker is invalid.".into());
    }
    if pending.receipt_present && !backup.join("retained.json").exists() {
        return Err("The pending native Hub Slack receipt backup is missing.".into());
    }
    let prior_plist = if pending.plist_present {
        let path = backup.join("previous.plist");
        let metadata = path
            .symlink_metadata()
            .map_err(|_| "The pending native Hub plist backup is missing.".to_owned())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("The pending native Hub plist backup is invalid.".into());
        }
        Some(std::fs::read(path).map_err(|error| error.to_string())?)
    } else {
        None
    };
    let rollback_requested = backup.join("rollback").exists();
    let commit = !rollback_requested
        && manifest
            .native_hub
            .as_ref()
            .is_some_and(|record| record.slack_files == pending.new_files);
    if commit {
        let record = manifest.native_hub.as_ref().expect("checked above");
        if record.plist != expected_plist
            || super::manifest::file_hash(expected_plist).as_deref() != Some(&record.plist_sha256)
        {
            return Err("The pending native Hub LaunchAgent has not been committed safely.".into());
        }
        validate_slack_state(&record.data_dir, &record.environment, &record.slack_files)?;
        let receipt = Receipt {
            project: record.project.clone(),
            environment: record.environment.clone(),
            slack_files: record.slack_files.clone(),
        };
        atomic_private(
            receipt_file,
            &format!(
                "{}\n",
                serde_json::to_string_pretty(&receipt)
                    .map_err(|_| "cannot encode native Hub ownership receipt")?
            ),
        )?;
        std::fs::remove_dir_all(&backup).map_err(|error| error.to_string())?;
        if next.exists() {
            std::fs::remove_dir_all(next).map_err(|error| error.to_string())?;
        }
        super::sync_dir(&parent)?;
        return Ok(true);
    }
    let prior_receipt = if pending.receipt_present {
        Some(std::fs::read(backup.join("retained.json")).map_err(|error| error.to_string())?)
    } else {
        None
    };
    let old_record = pending.old_record.clone();
    let mut swap = SlackSwap {
        target: target.clone(),
        next,
        backup,
        old_files: pending.old_files,
        new_files: pending.new_files,
        receipt: receipt_file.to_owned(),
        prior_receipt: prior_receipt.clone(),
        active: true,
    };
    swap.rollback()?;
    restore_private_file(expected_plist, &prior_plist)?;
    if let Some(record) = old_record.as_ref()
        && super::manifest::file_hash(expected_plist).as_deref() != Some(&record.plist_sha256)
    {
        return Err("The previous native Hub LaunchAgent could not be restored.".into());
    }
    manifest.native_hub = old_record.clone();
    save_manifest(manifest)?;
    let backup = swap.backup.clone();
    let mut recovered_swap = Some(swap);
    let recovery = NativeRollback {
        swap: &mut recovered_swap,
        receipt: receipt_file,
        prior_receipt: &prior_receipt,
        plist: expected_plist,
        prior_plist: &prior_plist,
        manifest,
        prior_record: &old_record,
        retain: &None,
        restart_previous: false,
    };
    validate_native_restoration(&recovery)?;
    std::fs::remove_dir_all(&backup).map_err(|error| error.to_string())?;
    super::sync_dir(&parent)?;
    Ok(false)
}

struct SlackInstallPrior<'a> {
    files: &'a BTreeMap<String, String>,
    receipt: Option<&'a [u8]>,
    record: Option<&'a Record>,
    plist_path: &'a Path,
    plist: Option<&'a [u8]>,
}

#[derive(Debug)]
struct SlackInstallError {
    message: String,
    swap: Option<Box<SlackSwap>>,
}

impl From<String> for SlackInstallError {
    fn from(message: String) -> Self {
        Self {
            message,
            swap: None,
        }
    }
}

impl From<&str> for SlackInstallError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

fn install_slack_bundle(
    data_dir: &Path,
    receipt_file: &Path,
    bundle: &SlackBundle,
    prior: SlackInstallPrior<'_>,
) -> Result<(BTreeMap<String, String>, Option<SlackSwap>), SlackInstallError> {
    install_slack_bundle_with(data_dir, receipt_file, bundle, prior, validate_slack_state)
}

fn install_slack_bundle_with<F>(
    data_dir: &Path,
    receipt_file: &Path,
    bundle: &SlackBundle,
    prior: SlackInstallPrior<'_>,
    validate: F,
) -> Result<(BTreeMap<String, String>, Option<SlackSwap>), SlackInstallError>
where
    F: FnOnce(&Path, &BTreeMap<String, String>, &BTreeMap<String, String>) -> Result<(), String>,
{
    let dir = data_dir.join("secrets").join("slack");
    if bundle.config != dir.join("config.json")
        || bundle.files.is_empty()
        || !bundle.files.iter().all(|(name, _)| valid_slack_leaf(name))
    {
        return Err("The prepared Slack bundle has an invalid target.".into());
    }
    let files: BTreeMap<_, _> = bundle
        .files
        .iter()
        .map(|(name, bytes)| (name.clone(), sha_hex(bytes)))
        .collect();
    if !prior.files.is_empty() && files == *prior.files {
        return Ok((files, None));
    }
    let parent = dir
        .parent()
        .ok_or_else(|| "The native Hub Slack target has no parent.".to_owned())?;
    private_owned_dir(parent, "Slack secrets directory")?;
    private_owned_dir(&dir, "Slack bundle directory")?;
    let next = parent.join(".slack-next");
    let backup = parent.join(".slack-rollback");
    if next.exists() || backup.exists() {
        return Err("A pending native Hub Slack update must be recovered first.".into());
    }
    super::private_dir(&next).map_err(|error| error.to_string())?;
    let staged = (|| {
        for (name, bytes) in &bundle.files {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| "A prepared Slack bundle file is not UTF-8.".to_owned())?;
            atomic_private(&next.join(name), text)?;
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = staged {
        let _ = std::fs::remove_dir_all(&next);
        return Err(error.into());
    }
    super::private_dir(&backup).map_err(|error| error.to_string())?;
    let pending = PendingSlack {
        old_files: prior.files.clone(),
        new_files: files.clone(),
        receipt_present: prior.receipt.is_some(),
        plist: prior.plist_path.to_owned(),
        plist_present: prior.plist.is_some(),
        old_record: prior.record.cloned(),
    };
    atomic_private(
        &backup.join("pending.json"),
        &serde_json::to_string_pretty(&pending)
            .map_err(|_| "cannot encode pending native Slack update")?,
    )?;
    if let Some(receipt) = prior.receipt {
        atomic_private(
            &backup.join("retained.json"),
            std::str::from_utf8(receipt)
                .map_err(|_| "The native Hub ownership receipt is not UTF-8.".to_owned())?,
        )?;
    }
    if let Some(plist) = prior.plist {
        atomic_private(
            &backup.join("previous.plist"),
            std::str::from_utf8(plist)
                .map_err(|_| "The previous native Hub LaunchAgent is not UTF-8.".to_owned())?,
        )?;
    }
    for (name, digest) in prior.files {
        let source = dir.join(name);
        let text = std::fs::read_to_string(&source).map_err(|error| error.to_string())?;
        atomic_private(&backup.join(name), &text)?;
        if super::manifest::file_hash(&backup.join(name)).as_deref() != Some(digest) {
            let _ = std::fs::remove_dir_all(&next);
            let _ = std::fs::remove_dir_all(&backup);
            return Err("Could not preserve the previous native Hub Slack bundle.".into());
        }
    }
    super::sync_dir(parent)?;
    atomic_private(&backup.join("ready"), "")?;
    let swap = SlackSwap {
        target: dir.clone(),
        next,
        backup,
        old_files: prior.files.clone(),
        new_files: files.clone(),
        receipt: receipt_file.to_owned(),
        prior_receipt: prior.receipt.map(<[u8]>::to_vec),
        active: true,
    };
    let replaced = (|| {
        for name in files.keys() {
            std::fs::rename(swap.next.join(name), dir.join(name))
                .map_err(|error| error.to_string())?;
        }
        for name in prior.files.keys().filter(|name| !files.contains_key(*name)) {
            std::fs::remove_file(dir.join(name)).map_err(|error| error.to_string())?;
        }
        super::sync_dir(&dir)?;
        Ok::<(), String>(())
    })();
    if let Err(error) = replaced {
        return Err(SlackInstallError {
            message: error,
            swap: Some(Box::new(swap)),
        });
    }
    if let Err(error) = validate(
        data_dir,
        &BTreeMap::from([(
            "AGENTSCM_SLACK_FILE".to_owned(),
            bundle.config.display().to_string(),
        )]),
        &files,
    ) {
        return Err(SlackInstallError {
            message: error,
            swap: Some(Box::new(swap)),
        });
    }
    Ok((files, Some(swap)))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn plist_text(record: &Record, environment: &BTreeMap<String, String>) -> String {
    let url = format!("http://127.0.0.1:{}", record.port);
    let arguments = [
        record.executable.display().to_string(),
        "hub".into(),
        "serve".into(),
        "--bind".into(),
        format!("127.0.0.1:{}", record.port),
        "--objects".into(),
        record.data_dir.join("objects").display().to_string(),
        "--secrets".into(),
        record.data_dir.join("secrets").display().to_string(),
        "--public-url".into(),
        url,
    ];
    let mut text = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{}</string>\n  <key>ProgramArguments</key><array>\n",
        xml(&record.label)
    );
    for argument in arguments {
        text.push_str(&format!("    <string>{}</string>\n", xml(&argument)));
    }
    text.push_str("  </array>\n  <key>EnvironmentVariables</key><dict>\n");
    for (key, value) in environment {
        text.push_str(&format!(
            "    <key>{}</key><string>{}</string>\n",
            xml(key),
            xml(value)
        ));
    }
    // launchd has no size-bounded file sink. Keep persistent logs bounded at zero; operators can
    // inspect launchctl state, and foreground diagnostics remain available from `hub serve`.
    text.push_str(
        "  </dict>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n  <key>ThrottleInterval</key><integer>10</integer>\n  <key>ProcessType</key><string>Background</string>\n  <key>StandardOutPath</key><string>/dev/null</string>\n  <key>StandardErrorPath</key><string>/dev/null</string>\n</dict>\n</plist>\n",
    );
    text
}

fn base_environment(record: &Record) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::from([(
        "AGENTSCM_DATABASE_URL".into(),
        format!("sqlite://{}", record.data_dir.join("hub.sqlite").display()),
    )]);
    for key in ["AGENTSCM_GITHUB_APP_NAME", "AGENTSCM_GITHUB_APP_ORG"] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
            && !value.chars().any(char::is_control)
        {
            environment.insert(key.into(), value);
        }
    }
    environment
}

#[cfg(unix)]
fn user_domain() -> Result<String, String> {
    // SAFETY: getuid has no preconditions and only reads the current process user id.
    Ok(format!("gui/{}", unsafe { libc::getuid() }))
}

#[cfg(not(unix))]
fn user_domain() -> Result<String, String> {
    Err("native Hub service control is available only on macOS".into())
}

fn launchctl(arguments: &[&str]) -> Result<(), String> {
    let output = run(Command::new("launchctl").args(arguments), CONTROL_TIMEOUT)?;
    if output.success {
        Ok(())
    } else {
        Err(format!(
            "launchctl {} failed: {}",
            arguments[0],
            diagnostic(&output)
        ))
    }
}

fn native_service_loaded(target: &str, label: &str) -> Result<bool, String> {
    let status = run(
        Command::new("launchctl").args(["print", target]),
        CONTROL_TIMEOUT,
    )?;
    if status.success {
        Ok(true)
    } else if service_absent(&status, label) {
        Ok(false)
    } else {
        Err(format!(
            "Could not inspect the native Hub service; no recovery data was changed: {}",
            diagnostic(&status)
        ))
    }
}

fn stop_native_service(target: &str) -> Result<(), String> {
    let label = target
        .rsplit_once('/')
        .map(|(_, label)| label)
        .ok_or_else(|| "The native Hub service target is invalid.".to_owned())?;
    let bootout = launchctl(&["bootout", target]);
    let status = run(
        Command::new("launchctl").args(["print", target]),
        CONTROL_TIMEOUT,
    )?;
    if service_absent(&status, label) {
        return Ok(());
    }
    let bootout = bootout
        .err()
        .map(|error| format!("{error}; "))
        .unwrap_or_default();
    if status.success {
        Err(format!(
            "{bootout}launchctl still reports the native Hub loaded"
        ))
    } else {
        Err(format!(
            "{bootout}launchctl could not confirm that the native Hub stopped: {}",
            diagnostic(&status)
        ))
    }
}

fn atomic_private(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path.parent().ok_or("service path has no parent")?;
    super::private_dir(parent).map_err(|error| error.to_string())?;
    let temp = parent.join(format!(".agentscm-service-{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    crate::hub_cli::write_private(&temp, contents).map_err(|error| error.to_string())?;
    super::sync_file(&temp)?;
    std::fs::rename(&temp, path).map_err(|error| error.to_string())?;
    super::sync_dir(parent)
}

fn github_environment_transition(prior: &Record, next: &Record) -> bool {
    const KEYS: [&str; 3] = [
        "AGENTSCM_GITHUB_CLIENT_ID",
        "AGENTSCM_GITHUB_CLIENT_SECRET_FILE",
        "AGENTSCM_ADMINS",
    ];
    if prior.label != next.label
        || prior.plist != next.plist
        || prior.config_dir != next.config_dir
        || prior.data_dir != next.data_dir
        || prior.executable != next.executable
        || prior.executable_sha256 != next.executable_sha256
        || prior.port != next.port
        || prior.project != next.project
        || prior.slack_files != next.slack_files
    {
        return false;
    }
    let mut expected = prior.environment.clone();
    for key in KEYS {
        if expected.contains_key(key) {
            return false;
        }
        let Some(value) = next.environment.get(key) else {
            return false;
        };
        expected.insert(key.into(), value.clone());
    }
    expected == next.environment
}

fn read_github_update(template: &Record) -> Result<Option<PendingGithubUpdate>, String> {
    let path = github_update_path(&template.config_dir);
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("The pending native GitHub configuration record is invalid.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(
                "The pending native GitHub configuration record is not privately owned.".into(),
            );
        }
    }
    let pending: PendingGithubUpdate =
        serde_json::from_slice(&std::fs::read(&path).map_err(|error| error.to_string())?)
            .map_err(|_| "The pending native GitHub configuration record is invalid.".to_owned())?;
    let identity_matches = |record: &Record| {
        record.label == template.label
            && record.plist == template.plist
            && record.config_dir == template.config_dir
            && record.data_dir == template.data_dir
            && record.project == template.project
    };
    if !identity_matches(&pending.prior_record)
        || !identity_matches(&pending.next_record)
        || !github_environment_transition(&pending.prior_record, &pending.next_record)
        || sha_hex(pending.prior_plist.as_bytes()) != pending.prior_record.plist_sha256
        || sha_hex(plist_text(&pending.next_record, &pending.next_record.environment).as_bytes())
            != pending.next_record.plist_sha256
    {
        return Err("The pending native GitHub configuration record has drifted.".into());
    }
    let receipt: Receipt = serde_json::from_str(&pending.prior_receipt)
        .map_err(|_| "The pending native GitHub receipt backup is invalid.".to_owned())?;
    if receipt != receipt_for(&pending.prior_record) {
        return Err("The pending native GitHub receipt backup has drifted.".into());
    }
    validate_receipt(
        &receipt,
        &pending.prior_record.config_dir,
        &pending.prior_record.project,
    )?;
    validate_receipt(
        &receipt_for(&pending.next_record),
        &pending.next_record.config_dir,
        &pending.next_record.project,
    )?;
    Ok(Some(pending))
}

fn begin_github_update(
    prior: &Record,
    next: &Record,
) -> Result<(PendingGithubUpdate, String), String> {
    let path = github_update_path(&prior.config_dir);
    if github_update_exists(&prior.config_dir)? {
        return Err("A pending native GitHub configuration update requires recovery.".into());
    }
    if super::manifest::file_hash(&prior.plist).as_deref() != Some(&prior.plist_sha256) {
        return Err("The native Hub LaunchAgent changed before GitHub configuration.".into());
    }
    let receipt = load_receipt(&prior.config_dir, &prior.project)?;
    if receipt != receipt_for(prior) {
        return Err("The native Hub ownership receipt drifted before GitHub configuration.".into());
    }
    let prior_receipt = std::fs::read_to_string(receipt_path(&prior.config_dir))
        .map_err(|error| error.to_string())?;
    let pending = PendingGithubUpdate {
        phase: GithubUpdatePhase::Rollback,
        prior_record: prior.clone(),
        next_record: next.clone(),
        prior_plist: std::fs::read_to_string(&prior.plist).map_err(|error| error.to_string())?,
        prior_receipt,
    };
    if !github_environment_transition(prior, next) {
        return Err("The native GitHub configuration update is invalid.".into());
    }
    validate_receipt(&receipt_for(next), &next.config_dir, &next.project)?;
    let text = serde_json::to_string_pretty(&pending)
        .map_err(|_| "cannot encode native GitHub configuration recovery record")?;
    let mut commit = pending.clone();
    commit.phase = GithubUpdatePhase::Commit;
    let committed = serde_json::to_string_pretty(&commit)
        .map_err(|_| "cannot encode native GitHub configuration recovery record")?;
    atomic_private(&path, &format!("{text}\n"))?;
    Ok((pending, committed))
}

fn validate_github_transaction_state(
    pending: &PendingGithubUpdate,
    manifest: &Manifest,
) -> Result<(), String> {
    let manifest_matches = manifest.native_hub.as_ref() == Some(&pending.prior_record)
        || manifest.native_hub.as_ref() == Some(&pending.next_record);
    let plist_hash = super::manifest::file_hash(&pending.prior_record.plist);
    let plist_matches = plist_hash.as_deref() == Some(&pending.prior_record.plist_sha256)
        || plist_hash.as_deref() == Some(&pending.next_record.plist_sha256);
    let receipt = load_receipt(
        &pending.prior_record.config_dir,
        &pending.prior_record.project,
    )?;
    let receipt_matches = receipt == receipt_for(&pending.prior_record)
        || receipt == receipt_for(&pending.next_record);
    if manifest_matches && plist_matches && receipt_matches {
        Ok(())
    } else {
        Err(
            "The pending native GitHub configuration no longer matches either recorded generation; no service or file was changed."
                .into(),
        )
    }
}

fn clear_github_update(config_dir: &Path) -> Result<(), String> {
    let path = github_update_path(config_dir);
    std::fs::remove_file(&path).map_err(|error| error.to_string())?;
    super::sync_dir(
        path.parent()
            .ok_or("native GitHub recovery record has no parent")?,
    )
}

fn validate_github_generation(manifest: &Manifest, record: &Record) -> Result<(), String> {
    if manifest.native_hub.as_ref() != Some(record)
        || super::manifest::file_hash(&record.plist).as_deref() != Some(&record.plist_sha256)
        || load_receipt(&record.config_dir, &record.project)? != receipt_for(record)
    {
        return Err("The native GitHub configuration generation did not validate.".into());
    }
    Ok(())
}

fn settle_github_update_for_uninstall(
    template: &Record,
    manifest: &mut Manifest,
) -> Result<Record, String> {
    if !github_update_exists(&template.config_dir)? {
        return Ok(template.clone());
    }
    let pending = read_github_update(template)?.ok_or_else(|| {
        "The native GitHub configuration recovery record changed while it was being inspected."
            .to_owned()
    })?;
    validate_github_transaction_state(&pending, manifest)?;
    if pending.phase == GithubUpdatePhase::Commit {
        validate_github_generation(manifest, &pending.next_record).map_err(|_| {
            "The committed native GitHub configuration is not the exact recorded new generation; no service or file was changed."
                .to_owned()
        })?;
    }
    let domain = user_domain()?;
    let target = format!("{domain}/{}", template.label);
    stop_native_service(&target)?;
    let chosen = match pending.phase {
        GithubUpdatePhase::Rollback => {
            atomic_private(&pending.prior_record.plist, &pending.prior_plist)?;
            atomic_private(
                &receipt_path(&pending.prior_record.config_dir),
                &pending.prior_receipt,
            )?;
            pending.prior_record
        }
        GithubUpdatePhase::Commit => pending.next_record,
    };
    manifest.native_hub = Some(chosen.clone());
    save_manifest_durable(manifest)?;
    validate_github_generation(manifest, &chosen).map_err(|error| {
        format!("{error} The Hub remains stopped and the private recovery record was retained.")
    })?;
    clear_github_update(&chosen.config_dir).map_err(|error| {
        format!(
            "The native GitHub configuration was settled with the Hub stopped, but its recovery record could not be removed: {error}"
        )
    })?;
    Ok(chosen)
}

async fn recover_github_update(
    template: &Record,
    manifest: &mut Manifest,
    domain: &str,
) -> Result<bool, String> {
    let Some(pending) = read_github_update(template)? else {
        return Ok(false);
    };
    validate_github_transaction_state(&pending, manifest)?;
    let target = format!("{domain}/{}", template.label);
    let service_loaded = native_service_loaded(&target, &template.label)?;
    let current = match pending.phase {
        GithubUpdatePhase::Rollback => &pending.prior_record,
        GithubUpdatePhase::Commit => &pending.next_record,
    };
    let exact_current = manifest.native_hub.as_ref() == Some(current)
        && super::manifest::file_hash(&current.plist).as_deref() == Some(&current.plist_sha256)
        && load_receipt(&current.config_dir, &current.project)? == receipt_for(current);
    if exact_current && service_loaded {
        let url = format!("http://127.0.0.1:{}", current.port);
        if super::wait_healthy(&url, Duration::from_secs(60)).await {
            clear_github_update(&template.config_dir)?;
            return Ok(true);
        }
    }
    if service_loaded {
        stop_native_service(&target)?;
    }
    let desired = match pending.phase {
        GithubUpdatePhase::Rollback => {
            atomic_private(&pending.prior_record.plist, &pending.prior_plist)?;
            atomic_private(
                &receipt_path(&pending.prior_record.config_dir),
                &pending.prior_receipt,
            )?;
            manifest.native_hub = Some(pending.prior_record.clone());
            save_manifest_durable(manifest)?;
            &pending.prior_record
        }
        GithubUpdatePhase::Commit => {
            if manifest.native_hub.as_ref() != Some(&pending.next_record)
                || super::manifest::file_hash(&pending.next_record.plist).as_deref()
                    != Some(&pending.next_record.plist_sha256)
                || load_receipt(
                    &pending.next_record.config_dir,
                    &pending.next_record.project,
                )? != receipt_for(&pending.next_record)
            {
                return Err(
                    "The committed native GitHub configuration could not be validated; recovery evidence was retained."
                        .into(),
                );
            }
            &pending.next_record
        }
    };
    if validate_github_generation(manifest, desired).is_err() {
        return Err(
            "The recovered native GitHub configuration did not validate; recovery evidence was retained."
                .into(),
        );
    }
    launchctl(&["bootstrap", domain, &desired.plist.display().to_string()])?;
    let url = format!("http://127.0.0.1:{}", desired.port);
    if !super::wait_healthy(&url, Duration::from_secs(60)).await {
        let stop = stop_native_service(&target)
            .err()
            .map(|error| format!(" Stopping it also failed: {error}"))
            .unwrap_or_default();
        return Err(format!(
            "The recovered native Hub did not become healthy; recovery evidence was retained.{stop}"
        ));
    }
    clear_github_update(&template.config_dir)?;
    Ok(true)
}

async fn reconcile_github_error(
    template: &Record,
    manifest: &mut Manifest,
    domain: &str,
    cause: impl Into<String>,
) -> String {
    let cause = cause.into();
    let phase = read_github_update(template)
        .ok()
        .flatten()
        .map(|pending| pending.phase);
    match recover_github_update(template, manifest, domain).await {
        Ok(true) if phase == Some(GithubUpdatePhase::Rollback) => {
            format!("{cause} The previous Hub configuration was restored.")
        }
        Ok(true) => {
            format!("{cause} The new native GitHub configuration was validated and retained.")
        }
        Ok(false) => format!(
            "{cause}; the native GitHub configuration recovery record disappeared before reconciliation."
        ),
        Err(error) => format!(
            "{cause}; recovery also failed: {error} Recovery is incomplete; the Hub service state is unconfirmed and the private recovery record was retained."
        ),
    }
}

fn save_manifest_durable(manifest: &mut Manifest) -> Result<(), String> {
    manifest.save().map_err(|error| error.to_string())?;
    let path = super::manifest::path().map_err(|error| error.to_string())?;
    super::sync_file(&path)?;
    super::sync_dir(path.parent().ok_or("quickstart manifest has no parent")?)
}

fn reject_symlink(path: &Path, description: &str) -> Result<(), String> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        Err(format!(
            "The native Hub {description} is a symbolic link; refusing it."
        ))
    } else {
        Ok(())
    }
}

fn executable_digest(path: &Path) -> Result<String, String> {
    std::fs::read(path)
        .map(|bytes| sha_hex(&bytes))
        .map_err(|error| format!("cannot read {}: {error}", path.display()))
}

pub(crate) fn preflight_port(port: u16) -> Result<(), String> {
    if port == 0 {
        return Err("--hub-port must be between 1 and 65535.".into());
    }
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
        .map(drop)
        .map_err(|_| format!("127.0.0.1:{port} is already in use; choose another --hub-port."))
}

struct NativeRuntime<'a> {
    record: Record,
    environment: BTreeMap<String, String>,
    domain: String,
    freshly_started: bool,
    url: String,
    manifest: &'a mut Manifest,
}

impl NativeRuntime<'_> {
    fn kickstart(&self) -> Result<(), String> {
        launchctl(&[
            "kickstart",
            "-k",
            &format!("{}/{}", self.domain, self.record.label),
        ])
    }
}

impl Runtime for NativeRuntime<'_> {
    fn url(&self) -> &str {
        &self.url
    }

    fn connection_ids(&self) -> Result<Vec<String>, String> {
        let dir = self.record.data_dir.join("secrets");
        let mut listing = String::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            listing.push_str(&entry.path().display().to_string());
            listing.push('\n');
        }
        Ok(hub_setup::connection_ids(&listing))
    }

    fn read_secret(&self, name: &str) -> Result<String, String> {
        if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            return Err("invalid Hub secret name".into());
        }
        std::fs::read_to_string(self.record.data_dir.join("secrets").join(name))
            .map(|text| text.trim().to_owned())
            .map_err(|error| error.to_string())
    }

    fn restart_fresh(&mut self) -> Result<(), String> {
        if std::mem::replace(&mut self.freshly_started, false) {
            Ok(()) // Bootstrap already started a new process with a fresh first-run link.
        } else {
            self.kickstart()
        }
    }

    async fn configure_github(
        &mut self,
        connection: &str,
        client_id: &str,
        login: &str,
    ) -> Result<(), String> {
        if self
            .environment
            .get("AGENTSCM_ADMINS")
            .is_some_and(|value| !value.is_empty())
        {
            return Ok(());
        }
        let previous_record = self.record.clone();
        let mut environment = self.environment.clone();
        environment.extend([
            ("AGENTSCM_GITHUB_CLIENT_ID".into(), client_id.into()),
            (
                "AGENTSCM_GITHUB_CLIENT_SECRET_FILE".into(),
                self.record
                    .data_dir
                    .join("secrets")
                    .join(format!("{connection}.client_secret"))
                    .display()
                    .to_string(),
            ),
            ("AGENTSCM_ADMINS".into(), format!("github:{login}")),
        ]);
        let mut next_record = self.record.clone();
        next_record.environment = environment.clone();
        let text = plist_text(&next_record, &environment);
        next_record.plist_sha256 = sha_hex(text.as_bytes());
        let receipt = serde_json::to_string_pretty(&receipt_for(&next_record))
            .map_err(|_| "cannot encode native Hub ownership receipt".to_owned())?;
        let (_, committed) = begin_github_update(&previous_record, &next_record)?;
        let target = format!("{}/{}", self.domain, self.record.label);
        if let Err(error) = stop_native_service(&target) {
            return Err(reconcile_github_error(
                &previous_record,
                self.manifest,
                &self.domain,
                error,
            )
            .await);
        }
        if let Err(error) = atomic_private(&next_record.plist, &text) {
            return Err(reconcile_github_error(
                &previous_record,
                self.manifest,
                &self.domain,
                error,
            )
            .await);
        }
        self.manifest.native_hub = Some(next_record.clone());
        if let Err(error) = save_manifest_durable(self.manifest) {
            return Err(reconcile_github_error(
                &previous_record,
                self.manifest,
                &self.domain,
                error,
            )
            .await);
        }
        let replacement = launchctl(&[
            "bootstrap",
            &self.domain,
            &self.record.plist.display().to_string(),
        ]);
        if let Err(error) = replacement {
            return Err(reconcile_github_error(
                &previous_record,
                self.manifest,
                &self.domain,
                error,
            )
            .await);
        }
        if !super::wait_healthy(&self.url, Duration::from_secs(60)).await {
            return Err(reconcile_github_error(
                &previous_record,
                self.manifest,
                &self.domain,
                "The configured native Hub did not become healthy.",
            )
            .await);
        }
        if let Err(error) = atomic_private(
            &receipt_path(&next_record.config_dir),
            &format!("{receipt}\n"),
        ) {
            return Err(reconcile_github_error(
                &previous_record,
                self.manifest,
                &self.domain,
                error,
            )
            .await);
        }
        let journal = github_update_path(&previous_record.config_dir);
        if let Err(error) = atomic_private(&journal, &format!("{committed}\n")) {
            return Err(reconcile_github_error(
                &previous_record,
                self.manifest,
                &self.domain,
                error,
            )
            .await);
        }
        if let Err(error) = clear_github_update(&previous_record.config_dir) {
            match github_update_exists(&previous_record.config_dir) {
                Ok(true) => {
                    if let Err(retry) = clear_github_update(&previous_record.config_dir) {
                        return Err(format!(
                            "The native GitHub configuration is committed and healthy, but its private recovery record could not be removed: {error}; retry failed: {retry}"
                        ));
                    }
                }
                Ok(false) => {
                    return Err(format!(
                        "The native GitHub configuration is committed and healthy and its recovery record was removed, but the cleanup directory could not be synced: {error}"
                    ));
                }
                Err(inspect) => {
                    return Err(format!(
                        "The native GitHub configuration is committed and healthy, but recovery-record cleanup could not be confirmed after {error}: {inspect}"
                    ));
                }
            }
        }
        self.record = next_record;
        self.environment = environment;
        Ok(())
    }
}

pub(crate) async fn start(
    ui: &Ui,
    login: Option<&str>,
    tty: bool,
    home: &Path,
    repo: Option<&str>,
    manifest: &mut Manifest,
    options: &Options,
) -> Result<Started, String> {
    if std::env::consts::OS != "macos" || std::env::consts::ARCH != "aarch64" {
        return Err("The native local Hub is qualified on Apple Silicon macOS only; choose the Docker runtime on this machine.".into());
    }
    if !crate::quickstart::valid_hub_project(&options.project) {
        return Err("--hub-project must start with a lowercase letter and contain lowercase letters, digits or hyphens (63 characters maximum).".into());
    }
    let mut existing = manifest.native_hub.clone();
    if existing.is_none() {
        preflight_port(options.port)?;
    }
    let executable = options
        .executable
        .canonicalize()
        .map_err(|error| format!("cannot resolve installed AgentSCM executable: {error}"))?;
    let config_dir = home
        .join(".agentscm")
        .join("hub-native")
        .join(&options.project);
    let data_dir = config_dir.join("data");
    reject_symlink(&home.join(".agentscm"), "AgentSCM directory")?;
    reject_symlink(&home.join(".agentscm/hub-native"), "native Hub root")?;
    reject_symlink(&home.join("Library"), "Library directory")?;
    reject_symlink(&home.join("Library/LaunchAgents"), "LaunchAgents directory")?;
    reject_symlink(&config_dir, "configuration directory")?;
    reject_symlink(&data_dir, "data directory")?;
    let mut record = Record {
        label: service_label(home, &options.project),
        plist: home.join("Library").join("LaunchAgents").join(format!(
            "com.counterbranch.agentscm.{}.plist",
            options.project
        )),
        config_dir,
        data_dir,
        executable_sha256: executable_digest(&executable)?,
        executable,
        plist_sha256: String::new(),
        port: options.port,
        project: options.project.clone(),
        environment: BTreeMap::new(),
        slack_files: BTreeMap::new(),
    };
    let domain = user_domain()?;
    let target = format!("{domain}/{}", record.label);
    let mut service_was_loaded = native_service_loaded(&target, &record.label)?;
    let mut service_loaded = service_was_loaded;
    if github_update_exists(&record.config_dir)? {
        recover_github_update(&record, manifest, &domain).await?;
        service_was_loaded = true;
        service_loaded = true;
    }
    let pending_slack = record
        .data_dir
        .join("secrets/.slack-rollback")
        .symlink_metadata()
        .is_ok();
    if pending_slack && service_loaded {
        stop_native_service(&target)?;
        service_loaded = false;
    }
    let _recovered_slack_restart = recover_pending_slack(
        &record.data_dir,
        &receipt_path(&record.config_dir),
        &record.plist,
        manifest,
    )?;
    existing = manifest.native_hub.clone();
    if let Some(existing) = existing.as_ref() {
        if existing.project != options.project
            || existing.port != options.port
            || existing.label != record.label
            || existing.config_dir != record.config_dir
            || existing.data_dir != record.data_dir
            || existing.plist != record.plist
            || std::fs::read(&existing.plist)
                .ok()
                .is_none_or(|bytes| sha_hex(&bytes) != existing.plist_sha256)
        {
            return Err("The recorded native Hub identity or LaunchAgent has drifted; refusing to replace it.".into());
        }
        record.plist = existing.plist.clone();
        record.environment = existing.environment.clone();
        record.slack_files = existing.slack_files.clone();
        validate_slack_state(&record.data_dir, &record.environment, &record.slack_files)?;
        // Older releases never copied GitHub settings into the receipt, so its environment may
        // lag the record; it is rewritten from the record below. The Slack files must match.
        let retained = load_receipt(&record.config_dir, &options.project)?;
        if retained.slack_files != existing.slack_files {
            return Err("The native Hub ownership receipt has drifted.".into());
        }
    } else if record.plist.exists() {
        return Err("A native Hub LaunchAgent already exists without a quickstart ownership record; refusing to replace it.".into());
    } else if record.config_dir.exists() {
        let receipt = load_receipt(&record.config_dir, &options.project)?;
        record.environment = receipt.environment;
        record.slack_files = receipt.slack_files;
    }
    let preset = ui
        .yes
        .then(|| hub_setup::github_login(login, ui))
        .transpose()?;
    println!(
        "Will install a per-user launchd Hub at http://127.0.0.1:{} using {}.",
        options.port,
        record.executable.display()
    );
    if options.slack.is_some() {
        println!(
            "Will copy the prepared Slack configuration and credentials into the native Hub's private data directory before the Hub starts."
        );
    }
    private_owned_dir(&record.data_dir, "data directory")?;
    private_owned_dir(&record.data_dir.join("secrets"), "secrets directory")?;
    let mut environment = if record.environment.is_empty() {
        base_environment(&record)
    } else {
        record.environment.clone()
    };
    let receipt_file = receipt_path(&record.config_dir);
    let prior_receipt = match std::fs::read(&receipt_file) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let prior_plist = match std::fs::read(&record.plist) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let retain = (existing.is_none() && prior_receipt.is_none()).then(|| Receipt {
        project: record.project.clone(),
        environment: base_environment(&record),
        slack_files: BTreeMap::new(),
    });
    let slack_will_change = options.slack.as_ref().is_some_and(|bundle| {
        let files: BTreeMap<_, _> = bundle
            .files
            .iter()
            .map(|(name, bytes)| (name.clone(), sha_hex(bytes)))
            .collect();
        files != record.slack_files
    });
    if slack_will_change && service_loaded {
        launchctl(&["bootout", &target])?;
        service_loaded = false;
    }
    let mut slack_swap = None;
    if let Some(bundle) = options.slack.as_ref() {
        if !record.slack_files.is_empty() {
            validate_slack_state(&record.data_dir, &environment, &record.slack_files)?;
        } else if record.data_dir.join("secrets/slack").exists() {
            return Err("A native Hub Slack directory exists without an ownership record; refusing to replace it.".into());
        }
        let installed = install_slack_bundle(
            &record.data_dir,
            &receipt_file,
            bundle,
            SlackInstallPrior {
                files: &record.slack_files,
                receipt: prior_receipt.as_deref(),
                record: existing.as_ref(),
                plist_path: &record.plist,
                plist: prior_plist.as_deref(),
            },
        );
        let (files, swap) = match installed {
            Ok(installed) => installed,
            Err(mut error) => {
                slack_swap = error.swap.take().map(|swap| *swap);
                return Err(restore_native_update(
                    NativeRollback {
                        swap: &mut slack_swap,
                        receipt: &receipt_file,
                        prior_receipt: &prior_receipt,
                        plist: &record.plist,
                        prior_plist: &prior_plist,
                        manifest,
                        prior_record: &existing,
                        retain: &retain,
                        restart_previous: service_was_loaded && !service_loaded,
                    },
                    error.message,
                    || launchctl(&["bootstrap", &domain, &record.plist.display().to_string()]),
                ));
            }
        };
        record.slack_files = files;
        slack_swap = swap;
        environment.insert(
            "AGENTSCM_SLACK_FILE".into(),
            bundle.config.display().to_string(),
        );
    } else {
        validate_slack_state(&record.data_dir, &environment, &record.slack_files)?;
    }
    record.environment = environment.clone();
    let receipt = Receipt {
        project: record.project.clone(),
        environment: environment.clone(),
        slack_files: record.slack_files.clone(),
    };
    let receipt = match serde_json::to_string_pretty(&receipt) {
        Ok(receipt) => receipt,
        Err(_) => {
            let error = "cannot encode native Hub ownership receipt".to_owned();
            return Err(restore_native_update(
                NativeRollback {
                    swap: &mut slack_swap,
                    receipt: &receipt_file,
                    prior_receipt: &prior_receipt,
                    plist: &record.plist,
                    prior_plist: &prior_plist,
                    manifest,
                    prior_record: &existing,
                    retain: &retain,
                    restart_previous: service_was_loaded && !service_loaded,
                },
                error,
                || launchctl(&["bootstrap", &domain, &record.plist.display().to_string()]),
            ));
        }
    };
    if let Err(error) = atomic_private(&receipt_file, &format!("{receipt}\n")) {
        return Err(restore_native_update(
            NativeRollback {
                swap: &mut slack_swap,
                receipt: &receipt_file,
                prior_receipt: &prior_receipt,
                plist: &record.plist,
                prior_plist: &prior_plist,
                manifest,
                prior_record: &existing,
                retain: &retain,
                restart_previous: service_was_loaded && !service_loaded,
            },
            error,
            || launchctl(&["bootstrap", &domain, &record.plist.display().to_string()]),
        ));
    }
    let text = plist_text(&record, &environment);
    record.plist_sha256 = sha_hex(text.as_bytes());
    let unchanged = existing.as_ref().is_some_and(|previous| {
        previous.executable == record.executable
            && previous.executable_sha256 == record.executable_sha256
            && previous.plist_sha256 == record.plist_sha256
            && previous.environment == record.environment
            && previous.slack_files == record.slack_files
    });
    let install = if unchanged && service_loaded {
        Ok(())
    } else if unchanged {
        launchctl(&["bootstrap", &domain, &record.plist.display().to_string()])
    } else {
        if let Err(error) = atomic_private(&record.plist, &text) {
            return Err(restore_native_update(
                NativeRollback {
                    swap: &mut slack_swap,
                    receipt: &receipt_file,
                    prior_receipt: &prior_receipt,
                    plist: &record.plist,
                    prior_plist: &prior_plist,
                    manifest,
                    prior_record: &existing,
                    retain: &retain,
                    restart_previous: service_was_loaded && !service_loaded,
                },
                error,
                || launchctl(&["bootstrap", &domain, &record.plist.display().to_string()]),
            ));
        }
        manifest.native_hub = Some(record.clone());
        if let Err(error) = save_manifest_durable(manifest) {
            return Err(restore_native_update(
                NativeRollback {
                    swap: &mut slack_swap,
                    receipt: &receipt_file,
                    prior_receipt: &prior_receipt,
                    plist: &record.plist,
                    prior_plist: &prior_plist,
                    manifest,
                    prior_record: &existing,
                    retain: &retain,
                    restart_previous: service_was_loaded && !service_loaded,
                },
                error,
                || launchctl(&["bootstrap", &domain, &record.plist.display().to_string()]),
            ));
        }
        if service_loaded {
            let _ = launchctl(&["bootout", &target]);
        }
        launchctl(&["bootstrap", &domain, &record.plist.display().to_string()])
    };
    if let Err(error) = install {
        return Err(restore_native_update(
            NativeRollback {
                swap: &mut slack_swap,
                receipt: &receipt_file,
                prior_receipt: &prior_receipt,
                plist: &record.plist,
                prior_plist: &prior_plist,
                manifest,
                prior_record: &existing,
                retain: &retain,
                restart_previous: service_was_loaded,
            },
            error,
            || launchctl(&["bootstrap", &domain, &record.plist.display().to_string()]),
        ));
    }
    let mut runtime = NativeRuntime {
        url: format!("http://127.0.0.1:{}", options.port),
        record,
        environment,
        domain,
        freshly_started: !(unchanged && service_loaded),
        manifest,
    };
    if !super::wait_healthy(&runtime.url, Duration::from_secs(60)).await {
        let status = run(
            Command::new("launchctl").args([
                "print",
                &format!("{}/{}", runtime.domain, runtime.record.label),
            ]),
            CONTROL_TIMEOUT,
        )
        .map(|output| diagnostic(&output))
        .unwrap_or_default();
        let target = format!("{}/{}", runtime.domain, runtime.record.label);
        let cause = format!(
            "The hub at {} didn't become healthy within 60 seconds. launchd status: {}. Persistent service output is disabled to prevent unbounded logs; run the recorded executable with the plist arguments in a terminal for foreground diagnostics.",
            runtime.url,
            if status.is_empty() {
                "unavailable"
            } else {
                &status
            }
        );
        if let Err(error) = stop_native_service(&target) {
            if let Some(swap) = slack_swap.as_mut() {
                swap.retain_for_recovery();
            }
            return Err(format!(
                "{cause} Stopping the unhealthy Hub before credential recovery failed: {error}. The pending recovery evidence was retained."
            ));
        }
        return Err(restore_native_update(
            NativeRollback {
                swap: &mut slack_swap,
                receipt: &receipt_file,
                prior_receipt: &prior_receipt,
                plist: &runtime.record.plist,
                prior_plist: &prior_plist,
                manifest: runtime.manifest,
                prior_record: &existing,
                retain: &retain,
                restart_previous: service_was_loaded,
            },
            cause,
            || {
                launchctl(&[
                    "bootstrap",
                    &runtime.domain,
                    &runtime.record.plist.display().to_string(),
                ])
            },
        ));
    }
    if let Some(swap) = slack_swap.as_mut() {
        swap.commit()?;
    }
    let started = hub_setup::finish(&mut runtime, ui, login, preset, tty, repo).await?;
    runtime.manifest.native_hub = Some(runtime.record);
    save_manifest_durable(runtime.manifest)?;
    Ok(started)
}

fn service_absent(output: &super::process::Output, label: &str) -> bool {
    !output.timed_out
        && output.code == Some(113)
        && output
            .stderr
            .contains(&format!("Could not find service \"{label}\""))
}

pub(crate) fn uninstall(manifest: &mut Manifest, home: &Path, wipe: bool) -> Result<(), String> {
    let recorded = manifest
        .native_hub
        .clone()
        .ok_or_else(|| "The native Hub has no installation record.".to_owned())?;
    let native_root = home.join(".agentscm").join("hub-native");
    if !crate::quickstart::valid_hub_project(&recorded.project)
        || recorded.label != service_label(home, &recorded.project)
        || recorded.config_dir != native_root.join(&recorded.project)
        || recorded.data_dir != recorded.config_dir.join("data")
        || recorded.plist
            != home.join("Library").join("LaunchAgents").join(format!(
                "com.counterbranch.agentscm.{}.plist",
                recorded.project
            ))
    {
        return Err(
            "The recorded native Hub identity has drifted; refusing to stop or delete it.".into(),
        );
    }
    reject_symlink(&home.join(".agentscm"), "AgentSCM directory")?;
    reject_symlink(&native_root, "native Hub root")?;
    reject_symlink(&home.join("Library"), "Library directory")?;
    reject_symlink(&home.join("Library/LaunchAgents"), "LaunchAgents directory")?;
    reject_symlink(&recorded.config_dir, "configuration directory")?;
    reject_symlink(&recorded.data_dir, "data directory")?;
    let record = settle_github_update_for_uninstall(&recorded, manifest)?;
    let plist = match std::fs::read(&record.plist) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    if plist
        .as_ref()
        .is_some_and(|bytes| sha_hex(bytes) != record.plist_sha256)
    {
        return Err(
            "The native Hub LaunchAgent changed since quickstart wrote it; refusing to remove it."
                .into(),
        );
    }
    if !wipe {
        let receipt = Receipt {
            project: record.project.clone(),
            environment: record.environment.clone(),
            slack_files: record.slack_files.clone(),
        };
        if !environment_ok(&receipt.environment) {
            return Err("The recorded native Hub environment is not safe to retain.".into());
        }
        let text = serde_json::to_string_pretty(&receipt)
            .map_err(|_| "cannot encode retained native Hub receipt")?;
        atomic_private(&receipt_path(&record.config_dir), &format!("{text}\n"))?;
    }
    let domain = user_domain()?;
    let target = format!("{domain}/{}", record.label);
    let printed = run(
        Command::new("launchctl").args(["print", &target]),
        CONTROL_TIMEOUT,
    )?;
    if printed.success {
        if plist.is_none()
            && !printed
                .stdout
                .lines()
                .any(|line| line.trim() == format!("program = {}", record.executable.display()))
        {
            return Err("The LaunchAgent plist is missing and the loaded program identity could not be verified; its record was retained for manual cleanup.".into());
        }
        launchctl(&["bootout", &target])?;
    } else if !service_absent(&printed, &record.label) {
        return Err(format!(
            "Could not inspect the recorded LaunchAgent; no service configuration was removed: {}",
            diagnostic(&printed)
        ));
    }
    match std::fs::remove_file(&record.plist) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    if wipe {
        std::fs::remove_dir_all(&record.config_dir).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_absence_requires_the_exact_launchctl_result() {
        let output = |code, stderr: &str, timed_out| super::super::process::Output {
            success: false,
            code,
            stdout: String::new(),
            stderr: stderr.into(),
            timed_out,
        };
        let label = "com.counterbranch.agentscm.test";
        assert!(service_absent(
            &output(
                Some(113),
                &format!("Could not find service \"{label}\" in domain for user gui"),
                false,
            ),
            label,
        ));
        assert!(!service_absent(
            &output(Some(5), "GUI domain unavailable", false),
            label,
        ));
        assert!(!service_absent(
            &output(
                Some(113),
                &format!("Could not find service \"{label}\""),
                true,
            ),
            label,
        ));
    }

    fn fixture(root: &Path) -> Record {
        Record {
            label: "com.counterbranch.agentscm.test".into(),
            plist: root.join("test.plist"),
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            executable: root.join("agentscm"),
            executable_sha256: "a".repeat(64),
            plist_sha256: String::new(),
            port: 7444,
            project: "test".into(),
            environment: BTreeMap::new(),
            slack_files: BTreeMap::new(),
        }
    }

    #[test]
    fn plist_has_exact_program_and_no_inherited_environment() {
        let temp = tempfile::tempdir().unwrap();
        let record = fixture(temp.path());
        let text = plist_text(&record, &base_environment(&record));
        assert!(text.contains("<string>hub</string>"));
        assert!(text.contains("<string>serve</string>"));
        assert!(text.contains("127.0.0.1:7444"));
        assert!(text.contains("AGENTSCM_DATABASE_URL"));
        for forbidden in ["AWS_", "HOME</key>", "PATH</key>", "CLIENT_SECRET</key>"] {
            assert!(!text.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn escapes_paths_and_rejects_unsafe_project_names() {
        let temp = tempfile::tempdir().unwrap();
        let mut record = fixture(temp.path());
        record.executable = PathBuf::from("/tmp/a&b<tool>");
        let text = plist_text(&record, &base_environment(&record));
        assert!(text.contains("a&amp;b&lt;tool&gt;"));
        assert!(
            crate::quickstart::valid_hub_project("work-1")
                && !crate::quickstart::valid_hub_project("../work")
                && !crate::quickstart::valid_hub_project("a/b")
                && !crate::quickstart::valid_hub_project(".")
                && !crate::quickstart::valid_hub_project("..")
        );
        assert_ne!(
            service_label(Path::new("/Users/one"), "work"),
            service_label(Path::new("/Users/two"), "work")
        );
    }

    #[test]
    fn restart_failure_is_reported_after_successful_restore() {
        let mut errors = Vec::new();
        append_restart_result(&mut errors, true, || {
            Err("synthetic bootstrap failure".into())
        });
        assert_eq!(
            errors,
            ["restarting the previous Hub failed: synthetic bootstrap failure"]
        );
    }

    #[test]
    fn retained_receipt_recovers_only_whitelisted_environment() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("hub-native").join("work");
        super::super::private_dir(&config).unwrap();
        let environment = BTreeMap::from([
            (
                "AGENTSCM_DATABASE_URL".into(),
                format!(
                    "sqlite://{}",
                    config.join("data").join("hub.sqlite").display()
                ),
            ),
            ("AGENTSCM_ADMINS".into(), "github:octocat".into()),
        ]);
        let receipt = Receipt {
            project: "work".into(),
            environment: environment.clone(),
            slack_files: BTreeMap::new(),
        };
        let text = serde_json::to_string(&receipt).unwrap();
        atomic_private(&receipt_path(&config), &text).unwrap();
        assert_eq!(
            load_receipt(&config, "work").unwrap().environment,
            environment
        );
        assert!(load_receipt(&config, "other").is_err());

        let unsafe_receipt = Receipt {
            project: "work".into(),
            environment: BTreeMap::from([("AWS_SECRET_ACCESS_KEY".into(), "secret".into())]),
            slack_files: BTreeMap::new(),
        };
        atomic_private(
            &receipt_path(&config),
            &serde_json::to_string(&unsafe_receipt).unwrap(),
        )
        .unwrap();
        assert!(load_receipt(&config, "work").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_slack_bundle_is_private_and_drift_checked() {
        let temp = tempfile::tempdir().unwrap();
        let native = temp.path().join("hub-native/work");
        let data = native.join("data");
        let target = data.join("secrets/slack");
        let config = serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "app_id": "A123",
            "bot_id": "B123",
            "team_id": "T123",
            "hub_url": "https://hub.example.test/",
            "signing_secret_file": target.join("signing-secret"),
            "bot_token_file": target.join("bot-token"),
            "database": target.join("install.sqlite"),
            "bindings": [{
                "slack_user": "U123",
                "provider": "github",
                "host": "github.com",
                "forge_user_id": 42,
                "repository": "acme/widgets",
                "repository_id": 101,
                "agent_token_file": target.join("agent-0.token")
            }]
        }))
        .unwrap();
        let bundle = SlackBundle {
            config: target.join("config.json"),
            files: vec![
                ("config.json".into(), config),
                ("signing-secret".into(), b"synthetic-signing".to_vec()),
                ("bot-token".into(), b"xoxb-synthetic".to_vec()),
                ("agent-0.token".into(), vec![b'a'; 64]),
            ],
        };
        let mut expanded_value: serde_json::Value =
            serde_json::from_slice(&bundle.files[0].1).unwrap();
        let mut second = expanded_value["bindings"][0].clone();
        second["slack_user"] = "U456".into();
        second["agent_token_file"] = target.join("agent-1.token").display().to_string().into();
        expanded_value["bindings"]
            .as_array_mut()
            .unwrap()
            .push(second);
        let mut expanded_bundle = SlackBundle {
            config: bundle.config.clone(),
            files: bundle.files.clone(),
        };
        expanded_bundle.files[0].1 = serde_json::to_vec_pretty(&expanded_value).unwrap();
        expanded_bundle
            .files
            .push(("agent-1.token".into(), vec![b'b'; 64]));
        let (_expanded_files, mut first_swap) = install_slack_bundle(
            &data,
            &receipt_path(&native),
            &expanded_bundle,
            SlackInstallPrior {
                files: &BTreeMap::new(),
                receipt: None,
                record: None,
                plist_path: &native.join("service.plist"),
                plist: None,
            },
        )
        .unwrap();
        // The Hub wrote its journal before the first install was rolled back.
        atomic_private(&target.join("install.sqlite"), "synthetic-journal").unwrap();
        atomic_private(&target.join("install.sqlite-wal"), "synthetic-journal").unwrap();
        first_swap.as_mut().unwrap().rollback().unwrap();
        assert!(!target.exists());
        std::fs::remove_dir_all(data.join("secrets/.slack-rollback")).unwrap();
        let (expanded_files, mut first_swap) = install_slack_bundle(
            &data,
            &receipt_path(&native),
            &expanded_bundle,
            SlackInstallPrior {
                files: &BTreeMap::new(),
                receipt: None,
                record: None,
                plist_path: &native.join("service.plist"),
                plist: None,
            },
        )
        .unwrap();
        first_swap.as_mut().unwrap().commit().unwrap();
        let plist = native.join("service.plist");
        let old_plist = b"old synthetic plist".to_vec();
        atomic_private(&plist, std::str::from_utf8(&old_plist).unwrap()).unwrap();
        let environment = BTreeMap::from([
            (
                "AGENTSCM_DATABASE_URL".into(),
                format!("sqlite://{}", data.join("hub.sqlite").display()),
            ),
            (
                "AGENTSCM_SLACK_FILE".into(),
                expanded_bundle.config.display().to_string(),
            ),
        ]);
        let mut old_record = fixture(temp.path());
        old_record.plist = plist.clone();
        old_record.config_dir = native.clone();
        old_record.data_dir = data.clone();
        old_record.environment = environment.clone();
        old_record.slack_files = expanded_files.clone();
        old_record.plist_sha256 = sha_hex(&old_plist);
        let prior_receipt = serde_json::to_vec_pretty(&Receipt {
            project: old_record.project.clone(),
            environment,
            slack_files: expanded_files.clone(),
        })
        .unwrap();
        atomic_private(
            &receipt_path(&native),
            std::str::from_utf8(&prior_receipt).unwrap(),
        )
        .unwrap();
        let (_files, mut interrupted) = install_slack_bundle(
            &data,
            &receipt_path(&native),
            &bundle,
            SlackInstallPrior {
                files: &expanded_files,
                receipt: Some(&prior_receipt),
                record: Some(&old_record),
                plist_path: &plist,
                plist: Some(&old_plist),
            },
        )
        .unwrap();
        atomic_private(&plist, "new synthetic plist").unwrap();
        interrupted.as_mut().unwrap().active = false;
        drop(interrupted);
        let mut recovered_manifest = Manifest {
            native_hub: Some(old_record.clone()),
            ..Manifest::default()
        };
        assert!(
            !recover_pending_slack_with(
                &data,
                &receipt_path(&native),
                &plist,
                &mut recovered_manifest,
                |_| Ok(())
            )
            .unwrap()
        );
        assert_eq!(std::fs::read(&plist).unwrap(), old_plist);
        assert_eq!(
            recovered_manifest.native_hub.as_ref().unwrap().plist_sha256,
            old_record.plist_sha256
        );
        for (name, digest) in &expanded_files {
            assert_eq!(
                super::super::manifest::file_hash(&target.join(name)),
                Some(digest.clone())
            );
        }
        atomic_private(&target.join("install.sqlite"), "synthetic-journal").unwrap();
        for untouched_old_files in 0..=expanded_files.len() {
            let (_files, mut shrink_swap) = install_slack_bundle(
                &data,
                &receipt_path(&native),
                &bundle,
                SlackInstallPrior {
                    files: &expanded_files,
                    receipt: None,
                    record: None,
                    plist_path: &native.join("service.plist"),
                    plist: None,
                },
            )
            .unwrap();
            let swap = shrink_swap.as_mut().unwrap();
            for name in expanded_files.keys().take(untouched_old_files) {
                std::fs::copy(swap.backup.join(name), target.join(name)).unwrap();
            }
            swap.rollback().unwrap();
            for (name, digest) in &expanded_files {
                assert_eq!(
                    super::super::manifest::file_hash(&target.join(name)),
                    Some(digest.clone())
                );
            }
            std::fs::remove_dir_all(data.join("secrets/.slack-rollback")).unwrap();
        }
        let (files, mut shrink_swap) = install_slack_bundle(
            &data,
            &receipt_path(&native),
            &bundle,
            SlackInstallPrior {
                files: &expanded_files,
                receipt: None,
                record: None,
                plist_path: &native.join("service.plist"),
                plist: None,
            },
        )
        .unwrap();
        shrink_swap.as_mut().unwrap().commit().unwrap();
        assert!(!target.join("agent-1.token").exists());
        assert_eq!(
            std::fs::read_to_string(target.join("install.sqlite")).unwrap(),
            "synthetic-journal"
        );
        let environment = BTreeMap::from([
            (
                "AGENTSCM_DATABASE_URL".into(),
                format!("sqlite://{}", data.join("hub.sqlite").display()),
            ),
            (
                "AGENTSCM_SLACK_FILE".into(),
                bundle.config.display().to_string(),
            ),
        ]);
        validate_slack_state(&data, &environment, &files).unwrap();
        assert_eq!(
            target.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in files.keys() {
            assert_eq!(
                target.join(name).metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let receipt = Receipt {
            project: "work".into(),
            environment: environment.clone(),
            slack_files: files.clone(),
        };
        atomic_private(
            &receipt_path(&native),
            &serde_json::to_string(&receipt).unwrap(),
        )
        .unwrap();
        let retained = load_receipt(&native, "work").unwrap();
        assert_eq!(retained.environment, environment);
        assert_eq!(retained.slack_files, files);

        let prior_receipt = std::fs::read(receipt_path(&native)).unwrap();
        let mut current_record = old_record.clone();
        current_record.environment = environment.clone();
        current_record.slack_files = files.clone();
        let mut failure = install_slack_bundle_with(
            &data,
            &receipt_path(&native),
            &expanded_bundle,
            SlackInstallPrior {
                files: &files,
                receipt: Some(&prior_receipt),
                record: Some(&current_record),
                plist_path: &plist,
                plist: Some(&old_plist),
            },
            |_, _, _| Err("synthetic post-install verification failure".into()),
        )
        .unwrap_err();
        let mut slack_swap = failure.swap.take().map(|swap| *swap);
        let backup = slack_swap.as_ref().unwrap().backup.clone();
        atomic_private(&backup.join("config.json"), "corrupt backup").unwrap();
        let restarted = std::cell::Cell::new(false);
        let mut rollback_manifest = Manifest {
            native_hub: Some(current_record.clone()),
            ..Manifest::default()
        };
        let restored = restore_native_update_with(
            NativeRollback {
                swap: &mut slack_swap,
                receipt: &receipt_path(&native),
                prior_receipt: &Some(prior_receipt),
                plist: &plist,
                prior_plist: &Some(old_plist),
                manifest: &mut rollback_manifest,
                prior_record: &Some(current_record),
                retain: &None,
                restart_previous: true,
            },
            failure.message,
            |_| Ok(()),
            || {
                restarted.set(true);
                Ok(())
            },
        );
        assert!(restored.contains("synthetic post-install verification failure"));
        assert!(restored.contains("recovery also failed"));
        assert!(!restarted.get());
        assert!(backup.exists(), "rollback evidence must remain retryable");

        std::fs::write(target.join("bot-token"), "changed").unwrap();
        assert!(load_receipt(&native, "work").is_err());
        assert!(validate_slack_state(&data, &environment, &files).is_err());
    }
}
