//! Trusted-local installation operations. No service, login or release qualification.
//! This binary leaf reuses the native packager; it never invokes a model or shell.
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use agentscm::authority::Authority;
use agentscm::context::{ChangeId, InputIdentity, REVIEW_CONTEXT_VERSION, ReviewContext};
use agentscm::local_recovery::IssueWorkflowRecoveryConfig;
use agentscm::model::FileChange;
use agentscm::store::Engine;
use agentscm::workspace::Workspace;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const RECEIPT: &str = ".agentscm-installation.json";
const NEXT: &str = ".agentscm-installation.next";
const LOCK: &str = ".agentscm-setup.lock";
const MAX_RECEIPT: u64 = 16 * 1024;
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_EXECUTABLE: u64 = 128 * 1024 * 1024;
pub const ISSUE_RECOVERY_CONFIG_FILE: &str = "issue-recovery.json";
const MAX_ISSUE_CONFIG: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    TrustedLocalNative,
    Team,
    LinuxService,
    ModelAssisted,
}

#[derive(Clone, Debug)]
pub struct FreshRequest {
    pub root: PathBuf,
    pub profile: Profile,
}

#[derive(Clone, Debug)]
pub struct OwnedRequest {
    pub root: PathBuf,
    pub installation_id: String,
}

#[derive(Debug)]
pub enum SetupError {
    InvalidPath,
    ExistingPath,
    ForeignInstallation,
    UnsupportedProfile(Profile),
    UnsupportedPlatform,
    Busy,
    Blocked(Blocker),
    Io(std::io::Error),
    Native(agentscm::Error),
    Receipt(serde_json::Error),
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Native/receipt errors may contain local details; keep public diagnostics fixed.
        match self {
            Self::InvalidPath => {
                f.write_str("setup requires a private, absolute, non-overlapping path")
            }
            Self::ExistingPath => f.write_str("fresh installation destination already exists"),
            Self::ForeignInstallation => {
                f.write_str("installation ownership or native identity differs")
            }
            Self::UnsupportedProfile(profile) => {
                write!(f, "setup profile {profile:?} is unsupported")
            }
            Self::UnsupportedPlatform => {
                f.write_str("setup supports only the macOS native foundation")
            }
            Self::Busy => f.write_str("another setup operation holds the installation lock"),
            Self::Blocked(reason) => write!(f, "setup is blocked: {reason:?}"),
            Self::Io(_) => f.write_str(
                "installation filesystem operation failed; retained state was not removed",
            ),
            Self::Native(_) => {
                f.write_str("native operation failed; inspect retained installation state")
            }
            Self::Receipt(_) => f.write_str("installation receipt is invalid"),
        }
    }
}

impl std::error::Error for SetupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Native(error) => Some(error),
            Self::Receipt(error) => Some(error),
            _ => None,
        }
    }
}
impl From<std::io::Error> for SetupError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<agentscm::Error> for SetupError {
    fn from(value: agentscm::Error) -> Self {
        Self::Native(value)
    }
}
impl From<serde_json::Error> for SetupError {
    fn from(value: serde_json::Error) -> Self {
        Self::Receipt(value)
    }
}
pub type Result<T> = std::result::Result<T, SetupError>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Blocker {
    InterruptedReceiptWrite,
    InterruptedNativeInitialization,
    InterruptedPackaging,
    InterruptedSelfCheck,
    SelfCheckAlreadyRecorded,
    NativeNotInitialized,
    BundleNotPackaged,
    IssueRecoveryConfigNotWritten,
    InterruptedIssueRecoveryConfig,
    IssueRecoveryConfigMissing,
    IssueRecoveryConfigInvalid,
    IssueRecoveryConfigMismatch,
    UnexpectedIssueRecoveryConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct NativeIdentity {
    directory: DirectoryIdentity,
    database: DirectoryIdentity,
    repository: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
enum Progress {
    Claimed,
    InitializingNative,
    NativeInitialized {
        native: NativeIdentity,
    },
    WritingIssueRecoveryConfig {
        native: NativeIdentity,
    },
    IssueRecoveryConfigWritten {
        native: NativeIdentity,
    },
    PackagingNative {
        native: NativeIdentity,
    },
    Installed {
        native: NativeIdentity,
        manifest_sha256: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum SelfCheck {
    NotRun,
    Pending,
    Recorded {
        repository: String,
        revision: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    format: u32,
    installation_id: String,
    root: PathBuf,
    directory: DirectoryIdentity,
    package_version: String,
    executable_sha256: String,
    operating_system: String,
    architecture: String,
    progress: Progress,
    self_check: SelfCheck,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Check {
    Observed { detail: String },
    Recorded { detail: String },
    Blocked { reason: Blocker },
    NotRun,
    Unsupported,
    NotConfigured,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub root: PathBuf,
    pub installation_id: String,
    pub package_version: String,
    pub native: Check,
    pub bundle_inventory: Check,
    pub self_check: Check,
    pub service_health: Check,
    pub operator_authentication: Check,
    pub team: Check,
    pub forge: Check,
    pub model: Check,
    pub platform_services: Check,
    pub host_resources: Check,
    pub issue_recovery_config_path: PathBuf,
    pub issue_recovery_config: Check,
}

/// Reserve a new private root and persist ownership before native initialization.
/// An interruption before the first receipt leaves an unclaimed directory, refused on retry.
pub fn begin(request: &FreshRequest) -> Result<OwnedRequest> {
    require_profile(request.profile)?;
    let root = checked_root(&request.root)?;
    if exists(&root)? {
        return Err(SetupError::ExistingPath);
    }
    let executable_sha256 = executable_identity()?;
    create_private_directory(&root)?;
    let _lock = installation_lock(&root, true)?;
    let mut entropy = [0u8; 16];
    getrandom::fill(&mut entropy).map_err(|_| SetupError::ForeignInstallation)?;
    let installation_id = entropy
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let receipt = Receipt {
        format: 1,
        installation_id: installation_id.clone(),
        root: root.clone(),
        directory: directory_identity(&root)?,
        package_version: env!("CARGO_PKG_VERSION").into(),
        executable_sha256,
        operating_system: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        progress: Progress::Claimed,
        self_check: SelfCheck::NotRun,
    };
    write_receipt(&root, &receipt)?;
    Ok(OwnedRequest {
        root,
        installation_id,
    })
}

/// Continue only with the typed ownership identity already returned by `begin`.
pub fn resume(request: &OwnedRequest) -> Result<DoctorReport> {
    let (root, _lock, mut receipt) = owned(request)?;
    loop {
        match receipt.progress.clone() {
            Progress::Claimed => {
                if exists(&root.join("data"))? {
                    return Err(SetupError::ForeignInstallation);
                }
                receipt.progress = Progress::InitializingNative;
                write_receipt(&root, &receipt)?;
            }
            Progress::InitializingNative => {
                // No durable expected repository identity exists yet. Never adopt partial DATA.
                if exists(&root.join("data"))? {
                    return Err(SetupError::Blocked(
                        Blocker::InterruptedNativeInitialization,
                    ));
                }
                let data = root.join("data");
                let _engine = Engine::open(&data)?;
                let authority = Authority::open(&data)?;
                let native = NativeIdentity {
                    directory: directory_identity(&data)?,
                    database: file_identity(&data.join("state.sqlite"))?,
                    repository: authority.repository_id()?,
                };
                drop(authority);
                receipt.progress = Progress::NativeInitialized { native };
                write_receipt(&root, &receipt)?;
            }
            Progress::NativeInitialized { native } => {
                validate_native(&root, &native)?;
                if exists(&root.join(ISSUE_RECOVERY_CONFIG_FILE))? {
                    return Err(SetupError::Blocked(Blocker::UnexpectedIssueRecoveryConfig));
                }
                receipt.progress = Progress::WritingIssueRecoveryConfig { native };
                write_receipt(&root, &receipt)?;
            }
            Progress::WritingIssueRecoveryConfig { native } => {
                validate_native(&root, &native)?;
                if exists(&root.join(ISSUE_RECOVERY_CONFIG_FILE))? {
                    return Err(SetupError::Blocked(Blocker::InterruptedIssueRecoveryConfig));
                }
                write_issue_config(&root, &native, &receipt.installation_id)?;
                receipt.progress = Progress::IssueRecoveryConfigWritten { native };
                write_receipt(&root, &receipt)?;
            }
            Progress::IssueRecoveryConfigWritten { native } => {
                validate_native(&root, &native)?;
                require_issue_config(&root, &native, &receipt.installation_id)?;
                if exists(&root.join("bundle"))? {
                    return Err(SetupError::ForeignInstallation);
                }
                receipt.progress = Progress::PackagingNative { native };
                write_receipt(&root, &receipt)?;
            }
            Progress::PackagingNative { native } => {
                validate_native(&root, &native)?;
                require_issue_config(&root, &native, &receipt.installation_id)?;
                if exists(&root.join("bundle"))? {
                    return Err(SetupError::Blocked(Blocker::InterruptedPackaging));
                }
                // NativeBundle is an inventory declaration, not an authenticated release.
                crate::distribution::package_native(&root.join("bundle"))?;
                let manifest_sha256 =
                    file_digest(&root.join("bundle/manifest.json"), MAX_MANIFEST)?;
                // The packager reopens current_exe independently. Bind the actual copy
                // to our earlier pin before publishing Installed, even if that path changed.
                validate_bundle(&root, &manifest_sha256, &receipt.executable_sha256)?;
                require_issue_config(&root, &native, &receipt.installation_id)?;
                receipt.progress = Progress::Installed {
                    native,
                    manifest_sha256,
                };
                write_receipt(&root, &receipt)?;
            }
            Progress::Installed { native, .. } => {
                require_issue_config(&root, &native, &receipt.installation_id)?;
                return inspect(&root, &receipt);
            }
        }
    }
}

/// Inspect only this exact owned installation. No server, port, credential or provider probe.
pub fn doctor(request: &OwnedRequest) -> Result<DoctorReport> {
    let (root, _lock, receipt) = owned(request)?;
    inspect(&root, &receipt)
}

/// Optional synthetic native read/workspace exercise in a separate owned repository.
/// No checkpoint, Change or approval is created in installation DATA or a user repository.
pub fn self_check(request: &OwnedRequest) -> Result<DoctorReport> {
    let (root, _lock, mut receipt) = owned(request)?;
    inspect(&root, &receipt)?;
    if !matches!(receipt.progress, Progress::Installed { .. }) {
        return Err(SetupError::Blocked(Blocker::NativeNotInitialized));
    }
    if let Progress::Installed { native, .. } = &receipt.progress {
        require_issue_config(&root, native, &receipt.installation_id)?;
    }
    match &receipt.self_check {
        SelfCheck::NotRun => (),
        SelfCheck::Pending => return Err(SetupError::Blocked(Blocker::InterruptedSelfCheck)),
        SelfCheck::Recorded { .. } => {
            return Err(SetupError::Blocked(Blocker::SelfCheckAlreadyRecorded));
        }
    }
    let sample = root.join("self-check");
    if exists(&sample)? {
        return Err(SetupError::ForeignInstallation);
    }
    receipt.self_check = SelfCheck::Pending;
    write_receipt(&root, &receipt)?;
    create_private_directory(&sample)?;
    let engine = Engine::open(sample.join("data"))?;
    let mut authority = Authority::open(sample.join("data"))?;
    let repository = authority.repository_id()?;
    let content = b"AgentSCM isolated setup self-check.\n";
    let revision = engine.checkpoint(
        None,
        &[FileChange {
            path: "sample.txt".into(),
            content: Some(content.to_vec()),
        }],
    )?;
    let change = ChangeId {
        repository: repository.clone(),
        id: "setup-self-check".into(),
    };
    let policy_epoch = authority.policy_epoch()?;
    authority.start_change(&ReviewContext {
        version: REVIEW_CONTEXT_VERSION,
        change: change.clone(),
        change_version: 1,
        revision: revision.clone(),
        base: None,
        target: "self-check".into(),
        policy_epoch,
        intent: "Isolated setup storage/workspace exercise".into(),
        acceptance_criteria: vec!["Read the sample bytes from its own workspace".into()],
        scope: vec!["sample.txt".into()],
        exclusions: vec![],
        omissions: vec![
            "No approval, integration, service, provider or model qualification".into(),
        ],
        dependencies: vec![],
        recipe: InputIdentity::Unknown {},
        toolchain: InputIdentity::Unknown {},
        environment: InputIdentity::Unknown {},
        required_evidence: vec![],
        required_capabilities: vec![],
    })?;
    let workspace =
        Workspace::create(&engine, sample.join("workspace"), change.clone(), &revision)?;
    let state = workspace.state()?;
    let status = workspace.status(&engine, 10)?;
    if !state.ready
        || state.change != change
        || state.base_revision != revision
        || state.pending.is_some()
        || !status.complete
        || !status.changes.is_empty()
        || engine.read_file(&revision, "sample.txt")?.as_slice() != content
    {
        return Err(SetupError::ForeignInstallation);
    }
    receipt.self_check = SelfCheck::Recorded {
        repository,
        revision,
    };
    write_receipt(&root, &receipt)?;
    inspect(&root, &receipt)
}

fn inspect(root: &Path, receipt: &Receipt) -> Result<DoctorReport> {
    let (native, bundle_inventory) = match &receipt.progress {
        Progress::Claimed => (
            Check::Blocked {
                reason: Blocker::NativeNotInitialized,
            },
            Check::Blocked {
                reason: Blocker::BundleNotPackaged,
            },
        ),
        Progress::InitializingNative => (
            Check::Blocked {
                reason: Blocker::InterruptedNativeInitialization,
            },
            Check::Blocked {
                reason: Blocker::BundleNotPackaged,
            },
        ),
        Progress::NativeInitialized { native }
        | Progress::WritingIssueRecoveryConfig { native }
        | Progress::IssueRecoveryConfigWritten { native } => {
            validate_native(root, native)?;
            (
                Check::Observed {
                    detail: native.repository.clone(),
                },
                Check::Blocked {
                    reason: Blocker::BundleNotPackaged,
                },
            )
        }
        Progress::PackagingNative { native } => {
            validate_native(root, native)?;
            (
                Check::Observed {
                    detail: native.repository.clone(),
                },
                Check::Blocked {
                    reason: Blocker::InterruptedPackaging,
                },
            )
        }
        Progress::Installed {
            native,
            manifest_sha256,
        } => {
            validate_native(root, native)?;
            validate_bundle(root, manifest_sha256, &receipt.executable_sha256)?;
            (
                Check::Observed {
                    detail: native.repository.clone(),
                },
                Check::Observed {
                    detail:
                        "local native bundle inventory; publisher authentication not established"
                            .into(),
                },
            )
        }
    };
    let self_check = match &receipt.self_check {
        SelfCheck::NotRun => Check::NotRun,
        SelfCheck::Pending => Check::Blocked {
            reason: Blocker::InterruptedSelfCheck,
        },
        SelfCheck::Recorded { .. } => Check::Recorded {
            detail: "recorded isolated exercise; current sample workspace not re-probed".into(),
        },
    };
    let issue_recovery_config = match &receipt.progress {
        Progress::Claimed | Progress::InitializingNative => Check::Blocked {
            reason: Blocker::IssueRecoveryConfigNotWritten,
        },
        Progress::NativeInitialized { .. } => Check::Blocked {
            reason: if exists(&root.join(ISSUE_RECOVERY_CONFIG_FILE))? {
                Blocker::UnexpectedIssueRecoveryConfig
            } else { Blocker::IssueRecoveryConfigNotWritten },
        },
        Progress::WritingIssueRecoveryConfig { .. } => Check::Blocked {
            reason: Blocker::InterruptedIssueRecoveryConfig,
        },
        Progress::IssueRecoveryConfigWritten { native }
        | Progress::PackagingNative { native }
        | Progress::Installed { native, .. } => match check_issue_config(root, native, &receipt.installation_id) {
            Ok(()) => Check::Observed {
                detail: "canonical external installation configuration; recovery admission not qualified".into(),
            },
            Err(reason) => Check::Blocked { reason },
        },
    };
    Ok(DoctorReport {
        root: root.into(),
        installation_id: receipt.installation_id.clone(),
        package_version: receipt.package_version.clone(),
        native,
        bundle_inventory,
        self_check,
        service_health: Check::NotRun,
        operator_authentication: Check::NotConfigured,
        team: Check::Unsupported,
        forge: Check::NotConfigured,
        model: Check::NotConfigured,
        platform_services: Check::Unsupported,
        host_resources: Check::NotRun,
        issue_recovery_config_path: root.join(ISSUE_RECOVERY_CONFIG_FILE),
        issue_recovery_config,
    })
}

fn issue_config_bytes(native: &NativeIdentity, installation_id: &str) -> Result<Vec<u8>> {
    // Both inputs come from setup's owned receipt/current native identity, never a journal.
    let config = IssueWorkflowRecoveryConfig::new(&native.repository, installation_id)?;
    let bytes = serde_json::to_vec(&config)?;
    if bytes.len() as u64 > MAX_ISSUE_CONFIG {
        return Err(SetupError::Blocked(Blocker::IssueRecoveryConfigInvalid));
    }
    Ok(bytes)
}

fn write_issue_config(root: &Path, native: &NativeIdentity, installation_id: &str) -> Result<()> {
    let bytes = issue_config_bytes(native, installation_id)?;
    let mut file = options()
        .write(true)
        .create_new(true)
        .open(root.join(ISSUE_RECOVERY_CONFIG_FILE))
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                SetupError::Blocked(Blocker::InterruptedIssueRecoveryConfig)
            } else {
                SetupError::Io(error)
            }
        })?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    File::open(root)?.sync_all()?;
    require_issue_config(root, native, installation_id)
}

fn check_issue_config(
    root: &Path,
    native: &NativeIdentity,
    installation_id: &str,
) -> std::result::Result<(), Blocker> {
    let path = root.join(ISSUE_RECOVERY_CONFIG_FILE);
    match exists(&path) {
        Ok(false) => return Err(Blocker::IssueRecoveryConfigMissing),
        Err(_) => return Err(Blocker::IssueRecoveryConfigInvalid),
        Ok(true) => (),
    }
    private_file(&path).map_err(|_| Blocker::IssueRecoveryConfigInvalid)?;
    let bytes =
        read_small(&path, MAX_ISSUE_CONFIG).map_err(|_| Blocker::IssueRecoveryConfigInvalid)?;
    let actual = IssueWorkflowRecoveryConfig::from_json(&bytes)
        .map_err(|_| Blocker::IssueRecoveryConfigInvalid)?;
    let expected = issue_config_bytes(native, installation_id)
        .map_err(|_| Blocker::IssueRecoveryConfigInvalid)?;
    if actual.native_repository() != native.repository.as_str()
        || actual.installation_id() != installation_id
        || bytes != expected
    {
        return Err(Blocker::IssueRecoveryConfigMismatch);
    }
    Ok(())
}

fn require_issue_config(root: &Path, native: &NativeIdentity, installation_id: &str) -> Result<()> {
    check_issue_config(root, native, installation_id).map_err(SetupError::Blocked)
}

fn owned(request: &OwnedRequest) -> Result<(PathBuf, agentscm::lease::Lease, Receipt)> {
    require_profile(Profile::TrustedLocalNative)?;
    let root = checked_root(&request.root)?;
    private_directory(&root)?;
    let lock = installation_lock(&root, false)?;
    if exists(&root.join(NEXT))? {
        return Err(SetupError::Blocked(Blocker::InterruptedReceiptWrite));
    }
    private_file(&root.join(RECEIPT))?;
    let receipt: Receipt = serde_json::from_slice(&read_small(&root.join(RECEIPT), MAX_RECEIPT)?)?;
    if receipt.format != 1
        || receipt.root != root
        || receipt.installation_id.len() != 32
        || !receipt
            .installation_id
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
        || receipt.installation_id != request.installation_id
        || receipt.directory != directory_identity(&root)?
        || receipt.package_version != env!("CARGO_PKG_VERSION")
        || receipt.executable_sha256 != executable_identity()?
        || receipt.operating_system != std::env::consts::OS
        || receipt.architecture != std::env::consts::ARCH
    {
        return Err(SetupError::ForeignInstallation);
    }
    Ok((root, lock, receipt))
}

fn validate_native(root: &Path, expected: &NativeIdentity) -> Result<()> {
    let data = root.join("data");
    if directory_identity(&data)? != expected.directory
        || file_identity(&data.join("state.sqlite"))? != expected.database
    {
        return Err(SetupError::ForeignInstallation);
    }
    directory_identity(&data.join("objects"))?;
    for suffix in [
        "state.sqlite-wal",
        "state.sqlite-shm",
        ".agentscm-local-access.lock",
    ] {
        let file = data.join(suffix);
        if exists(&file)? {
            file_identity(&file)?;
        }
    }
    let authority = Authority::open_read_only(&data)?;
    if authority.repository_id()? != expected.repository {
        return Err(SetupError::ForeignInstallation);
    }
    authority.policy_epoch()?;
    Ok(())
}

fn validate_bundle(root: &Path, expected: &str, executable_sha256: &str) -> Result<()> {
    let bundle = root.join("bundle");
    directory_identity(&bundle)?;
    let names = [
        "agentscm",
        "LICENSE",
        "THIRD_PARTY_NOTICES.md",
        "Cargo.lock",
    ];
    let mut seen = std::collections::BTreeSet::new();
    for entry in fs::read_dir(&bundle)? {
        let entry = entry?;
        let name = entry.file_name();
        if !names.iter().any(|allowed| name == *allowed) && name != "manifest.json" {
            return Err(SetupError::ForeignInstallation);
        }
        if !seen.insert(name) {
            return Err(SetupError::ForeignInstallation);
        }
        // Five fixed immediate regular files only; never recurse into arbitrary output.
        let file = open_regular(&entry.path())?;
        if entry.file_name() == "manifest.json" {
            require_execution(&file, false)?;
        }
    }
    if seen.len() != 5 {
        return Err(SetupError::ForeignInstallation);
    }
    let bytes = read_small(&root.join("bundle/manifest.json"), MAX_MANIFEST)?;
    if format!("{:x}", Sha256::digest(&bytes)) != expected {
        return Err(SetupError::ForeignInstallation);
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value["format"] != "agentscm.native-bundle/v1" {
        return Err(SetupError::ForeignInstallation);
    }
    let files = value["files"]
        .as_array()
        .ok_or(SetupError::ForeignInstallation)?;
    if files.len() != names.len() {
        return Err(SetupError::ForeignInstallation);
    }
    for name in names {
        let matches = files
            .iter()
            .filter(|entry| entry["path"] == name)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(SetupError::ForeignInstallation);
        }
        let entry = matches[0];
        let path = root.join("bundle").join(name);
        let executable = name == "agentscm";
        if entry["executable"].as_bool() != Some(executable) {
            return Err(SetupError::ForeignInstallation);
        }
        let file = open_regular(&path)?;
        require_execution(&file, executable)?;
        let length = file.metadata()?.len();
        let limit = if executable {
            MAX_EXECUTABLE
        } else {
            MAX_MANIFEST * 32
        };
        let digest = digest_file(file, limit)?;
        if entry["bytes"].as_u64() != Some(length)
            || entry["sha256"].as_str() != Some(digest.as_str())
            || (executable && digest != executable_sha256)
        {
            return Err(SetupError::ForeignInstallation);
        }
    }
    Ok(())
}

fn require_profile(profile: Profile) -> Result<()> {
    if profile != Profile::TrustedLocalNative {
        return Err(SetupError::UnsupportedProfile(profile));
    }
    if !cfg!(target_os = "macos") {
        return Err(SetupError::UnsupportedPlatform);
    }
    Ok(())
}

fn checked_root(path: &Path) -> Result<PathBuf> {
    if path.to_str().is_none()
        || !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err(SetupError::InvalidPath);
    }
    let parent = path.parent().ok_or(SetupError::InvalidPath)?;
    let mut prefix = PathBuf::new();
    for component in parent.components() {
        prefix.push(component);
        if !fs::symlink_metadata(&prefix)?.is_dir()
            || fs::symlink_metadata(&prefix)?.file_type().is_symlink()
        {
            return Err(SetupError::InvalidPath);
        }
        for marker in [RECEIPT, ".git", ".agentscm-workspace", "state.sqlite"] {
            if exists(&prefix.join(marker))? {
                return Err(SetupError::InvalidPath);
            }
        }
        // Bare Git repositories have no .git entry. Match the existing export
        // boundary with three bounded filesystem checks, without Git discovery.
        if prefix.join("HEAD").is_file()
            && prefix.join("objects").is_dir()
            && prefix.join("config").is_file()
        {
            return Err(SetupError::InvalidPath);
        }
    }
    private_parent(parent)?;
    let leaf = path.file_name().ok_or(SetupError::InvalidPath)?;
    if exists(path)? && fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(SetupError::InvalidPath);
    }
    let canonical = fs::canonicalize(parent)?.join(leaf);
    if canonical.to_str().is_none() {
        return Err(SetupError::InvalidPath);
    }
    Ok(canonical)
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn private_parent(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != current_uid()
        || meta.mode() & 0o022 != 0
    {
        return Err(SetupError::InvalidPath);
    }
    Ok(())
}
#[cfg(not(unix))]
fn private_parent(_path: &Path) -> Result<()> {
    Err(SetupError::UnsupportedPlatform)
}

fn private_directory(path: &Path) -> Result<()> {
    private_parent(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fs::symlink_metadata(path)?.mode() & 0o077 != 0 {
            return Err(SetupError::InvalidPath);
        }
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<()> {
    file_identity(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fs::symlink_metadata(path)?.mode() & 0o077 != 0 {
            return Err(SetupError::ForeignInstallation);
        }
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            SetupError::ExistingPath
        } else {
            SetupError::Io(e)
        }
    })?;
    File::open(path.parent().ok_or(SetupError::InvalidPath)?)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn identity(path: &Path, directory: bool) -> Result<DirectoryIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink()
        || (directory && !meta.is_dir())
        || (!directory && !meta.is_file())
        || meta.uid() != current_uid()
        || meta.mode() & 0o022 != 0
        || (!directory && meta.nlink() != 1)
    {
        return Err(SetupError::ForeignInstallation);
    }
    Ok(DirectoryIdentity {
        device: meta.dev(),
        inode: meta.ino(),
    })
}
#[cfg(not(unix))]
fn identity(_path: &Path, _directory: bool) -> Result<DirectoryIdentity> {
    Err(SetupError::UnsupportedPlatform)
}
fn directory_identity(path: &Path) -> Result<DirectoryIdentity> {
    identity(path, true)
}
fn file_identity(path: &Path) -> Result<DirectoryIdentity> {
    identity(path, false)
}

fn require_execution(file: &File, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = file.metadata()?.mode();
        let expected = if executable { 0o111 } else { 0 };
        if mode & 0o111 != expected || mode & 0o7000 != 0 {
            return Err(SetupError::ForeignInstallation);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, executable);
        return Err(SetupError::UnsupportedPlatform);
    }
    Ok(())
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory access preconditions.
    unsafe { libc::geteuid() }
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    options
}

fn open_regular(path: &Path) -> Result<File> {
    let before = file_identity(path)?;
    let file = options().read(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        if !meta.is_file()
            || before
                != (DirectoryIdentity {
                    device: meta.dev(),
                    inode: meta.ino(),
                })
        {
            return Err(SetupError::ForeignInstallation);
        }
    }
    Ok(file)
}

fn installation_lock(root: &Path, create: bool) -> Result<agentscm::lease::Lease> {
    let path = root.join(LOCK);
    if !create {
        private_file(&path)?;
    }
    let file = if create {
        options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?
    } else {
        open_regular(&path)?
    };
    match agentscm::lease::Lease::try_exclusive(file) {
        Ok(lease) => Ok(lease),
        Err(std::fs::TryLockError::WouldBlock) => Err(SetupError::Busy),
        Err(std::fs::TryLockError::Error(error)) => Err(SetupError::Io(error)),
    }
}

fn read_small(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = open_regular(path)?;
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(SetupError::ForeignInstallation);
    }
    Ok(bytes)
}

fn file_digest(path: &Path, maximum: u64) -> Result<String> {
    digest_file(open_regular(path)?, maximum)
}

fn digest_file(mut file: File, maximum: u64) -> Result<String> {
    let mut hash = Sha256::new();
    let mut consumed = 0u64;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        consumed += count as u64;
        if consumed > maximum {
            return Err(SetupError::ForeignInstallation);
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn executable_identity() -> Result<String> {
    // The running installed CLI may be owned by root; setup does not modify it.
    let file = options().read(true).open(std::env::current_exe()?)?;
    if !file.metadata()?.is_file() {
        return Err(SetupError::ForeignInstallation);
    }
    digest_file(file, MAX_EXECUTABLE)
}

fn write_receipt(root: &Path, receipt: &Receipt) -> Result<()> {
    let bytes = serde_json::to_vec(receipt)?;
    if bytes.len() as u64 > MAX_RECEIPT {
        return Err(SetupError::ForeignInstallation);
    }
    let mut file = options()
        .write(true)
        .create_new(true)
        .open(root.join(NEXT))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(root.join(NEXT), root.join(RECEIPT))?;
    File::open(root)?.sync_all()?;
    Ok(())
}
