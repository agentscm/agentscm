//! Caller-asserted local capture and observational action receipts.
//!
//! Conversation bodies, command arguments and process output are retained only in a
//! [`LocalSpool`]. The shareable envelopes contain the bounded facts from
//! [`crate::change_record`] and opaque content identities; they grant no approval or execution
//! authority.

pub mod claude_code;
pub mod codex;
pub mod redact;
pub mod spool;

use std::ffi::{CString, OsStr};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::change_record::{
    ActionFinishV1, ActionKindV1, ActionOutcomeV1, ActionStartV1, ArtifactMediaV1, ArtifactRefV1,
    AssuranceBasisV1, CaptureEnvelopeV1, CaptureOmissionV1, CheckResultV1, CoverageV1, DriftV1,
    RecipeProvenanceV1, RetentionClassV1, SCHEMA_VERSION_V1, SourceBindingV1,
};
use crate::checks::sha256;
#[cfg(unix)]
use crate::execution::Cancellation;
#[cfg(unix)]
use crate::execution::process::{Stop, capture_with_input, capture_with_input_tee_after_spawn};
use spool::{RecipeAttemptReservationV1, RecipeAttemptV1};

pub use spool::{
    AppendOutcome, BoundedCountV1, CompactionReport, DeferredHookV1, DeferredReasonV1,
    FileIdentityV1, LocalSpool, MIN_ACTION_CAPTURE_BYTES, MIN_ACTION_CAPTURE_RECORDS,
    NamespaceBindOutcome, QuarantineEntryV1, QuarantinePruneReport, SpoolAttemptInspectionV1,
    SpoolClassificationV1, SpoolDigestV1, SpoolEntryInspectionV1, SpoolExportManifestV1,
    SpoolIdentityV1, SpoolInspectionV1, SpoolLimits, SpoolNamespaceV1, SpoolProbeStateV1,
    export_spool_archive, inspect_spool,
};

#[derive(Debug)]
pub enum CaptureError {
    Invalid(String),
    Conflict(String),
    Limit(String),
    NotFound(String),
    Io(std::io::Error),
    Json(serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CaptureError>;

const MAX_GIT_OUTPUT: u64 = 8 * 1024 * 1024;
const MAX_COMMAND_OUTPUT: u64 = 16 * 1024 * 1024;
const MAX_COMMAND_ARGS: usize = 128;
const MAX_COMMAND_ARG_BYTES: usize = 32 * 1024;
const MAX_SNAPSHOT_FILES: usize = 10_000;
const MAX_SNAPSHOT_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SNAPSHOT_TOTAL_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct GitSourceRequest {
    pub repository: String,
    pub worktree_id: String,
    pub repository_path: PathBuf,
    pub base_ref: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredRecipeV1 {
    pub id: String,
    pub argv: Vec<String>,
    pub coverage: CoverageV1,
    pub timeout_seconds: u64,
    pub max_output_bytes: u64,
}

impl ConfiguredRecipeV1 {
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > 128
            || !self.id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'@')
            })
        {
            return Err(CaptureError::Invalid(
                "configured recipe id is invalid".into(),
            ));
        }
        if self.argv.is_empty() || self.argv.len() > MAX_COMMAND_ARGS {
            return Err(CaptureError::Limit(format!(
                "configured recipe must contain 1..{MAX_COMMAND_ARGS} arguments"
            )));
        }
        if !Path::new(&self.argv[0]).is_absolute() {
            return Err(CaptureError::Invalid(
                "configured recipe executable must be an absolute path".into(),
            ));
        }
        let bytes = self.argv.iter().try_fold(0usize, |total, value| {
            if value.contains('\0') {
                return Err(CaptureError::Invalid(
                    "configured recipe argument contains NUL".into(),
                ));
            }
            total.checked_add(value.len()).ok_or_else(|| {
                CaptureError::Limit("configured recipe argument bytes overflow".into())
            })
        })?;
        if bytes > MAX_COMMAND_ARG_BYTES {
            return Err(CaptureError::Limit(format!(
                "configured recipe arguments exceed {MAX_COMMAND_ARG_BYTES} bytes"
            )));
        }
        if !(1..=300).contains(&self.timeout_seconds) {
            return Err(CaptureError::Invalid(
                "configured recipe timeout must be 1..300 seconds".into(),
            ));
        }
        if !(256..=MAX_COMMAND_OUTPUT).contains(&self.max_output_bytes) {
            return Err(CaptureError::Invalid(format!(
                "configured recipe output limit must be 256..{MAX_COMMAND_OUTPUT} bytes"
            )));
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        #[derive(Serialize)]
        struct Identity<'a> {
            schema_version: u16,
            contract: &'static str,
            id: &'a str,
            argv: &'a [String],
            coverage: CoverageV1,
            timeout_seconds: u64,
            max_output_bytes: u64,
            cwd: &'static str,
            stdin: &'static str,
            environment: &'static str,
        }
        Ok(sha256(&serde_json::to_vec(&Identity {
            schema_version: SCHEMA_VERSION_V1,
            contract: "agentscm.configured_recipe.v1",
            id: &self.id,
            argv: &self.argv,
            coverage: self.coverage,
            timeout_seconds: self.timeout_seconds,
            max_output_bytes: self.max_output_bytes,
            cwd: "repository_root",
            stdin: "null",
            environment: "allowlisted_inherited",
        })?))
    }
}

#[derive(Clone, Debug)]
pub struct ObservedCommandRequest {
    pub start_operation_id: String,
    pub finish_operation_id: String,
    pub work_id: String,
    pub generation: i64,
    pub session_id: String,
    pub action_id: String,
    pub source: GitSourceRequest,
    pub argv: Vec<String>,
    pub setup_digest: String,
    pub coverage: CoverageV1,
    pub timeout: Duration,
    pub max_output_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct ConfiguredRecipeRunRequest {
    pub work_id: String,
    pub generation: i64,
    pub observed_session_id: String,
    pub session_id: String,
    pub source: GitSourceRequest,
    pub setup_digest: String,
    pub recipe: ConfiguredRecipeV1,
    pub managed_invocation: String,
}

#[derive(Clone, Debug)]
pub struct ConfiguredRecipeReconcileRequest {
    pub work_id: String,
    pub source: GitSourceRequest,
    pub recipe_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservedStop {
    Exited,
    Signalled,
    TimedOut,
    Cancelled,
    OutputLimit,
    Io,
    SpawnFailed,
}

#[derive(Clone, Debug)]
pub struct ObservedCommand {
    pub start: ActionStartV1,
    pub finish: ActionFinishV1,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stop: ObservedStop,
    pub storage_warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum RecipeReconcileOutcome {
    None,
    Busy { fence_id: String },
    ClearedPreSpawn { action_id: String },
    Unresolved { action_id: String, phase: String },
    Reconciled(ActionFinishV1),
}

#[derive(Serialize)]
struct RecipeIdentity<'a> {
    schema_version: u16,
    argv: &'a [String],
    timeout_ms: u128,
    max_output_bytes: u64,
}

#[derive(Serialize)]
struct LocalCommandRecord<'a> {
    schema_version: u16,
    action_id: &'a str,
    argv: &'a [String],
    exit_code: Option<i32>,
    signal: Option<i32>,
    stop: &'a str,
    provenance: &'static str,
    assurance_limit: &'static str,
}

pub(crate) fn local_host_identity() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut uuid = [0_u8; 16];
        let timeout = libc::timespec {
            tv_sec: 5,
            tv_nsec: 0,
        };
        // SAFETY: both pointers reference initialized fixed-size values for the duration of the
        // call. gethostuuid writes exactly one 16-byte host UUID on success.
        if unsafe { libc::gethostuuid(uuid.as_mut_ptr(), &timeout) } == 0 {
            return Some(sha256(&[b"macos-host:".as_slice(), &uuid].concat()));
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        read_local_identity_file(Path::new("/etc/machine-id"), "linux-host").or_else(|| {
            read_local_identity_file(Path::new("/var/lib/dbus/machine-id"), "linux-host")
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

pub(crate) fn local_boot_identity() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let name = CString::new("kern.boottime").ok()?;
        // SAFETY: the zeroed timeval is valid output storage for sysctlbyname.
        let mut boot: libc::timeval = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::timeval>();
        // SAFETY: name is NUL-terminated, output points to a timeval-sized allocation, and no
        // input value is supplied.
        if unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut boot as *mut libc::timeval).cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } == 0
            && length == std::mem::size_of::<libc::timeval>()
        {
            return Some(sha256(
                format!("macos-boot:{}:{}", boot.tv_sec, boot.tv_usec).as_bytes(),
            ));
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        read_local_identity_file(Path::new("/proc/sys/kernel/random/boot_id"), "linux-boot")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn read_local_identity_file(path: &Path, label: &str) -> Option<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file() || metadata.len() > 256 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(257).read_to_end(&mut bytes).ok()?;
    let value = std::str::from_utf8(&bytes).ok()?.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return None;
    }
    Some(sha256(format!("{label}:{value}").as_bytes()))
}

#[derive(Serialize)]
struct WorkingSnapshotManifest {
    schema_version: u16,
    head: String,
    entries: Vec<WorkingSnapshotEntry>,
    unsupported: Vec<&'static str>,
}

#[derive(Serialize)]
struct WorkingSnapshotEntry {
    path: String,
    kind: &'static str,
    executable: bool,
    artifact: Option<ArtifactRefV1>,
}

struct OpenedGitRoot {
    directory: File,
    device: u64,
    inode: u64,
}

struct PreparedRootCwd(File);

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_CWD_CLONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

enum OpenedWorkingEntry {
    Deleted,
    Directory,
    Unsupported,
    Content {
        kind: &'static str,
        executable: bool,
        bytes: Vec<u8>,
    },
}

impl OpenedGitRoot {
    fn open(path: &Path) -> Result<Self> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = directory.metadata()?;
        if !metadata.file_type().is_dir() {
            return Err(CaptureError::Invalid(
                "working source root is not a directory".into(),
            ));
        }
        let opened = Self {
            directory,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        opened.verify_path(path)?;
        Ok(opened)
    }

    fn verify_path(&self, path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir()
            || metadata.file_type().is_symlink()
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
        {
            return Err(CaptureError::Invalid(
                "working source root identity changed during capture".into(),
            ));
        }
        Ok(())
    }

    fn verify_descriptor(&self) -> Result<()> {
        let metadata = self.directory.metadata()?;
        if !metadata.file_type().is_dir()
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
        {
            return Err(CaptureError::Invalid(
                "working source root descriptor identity changed".into(),
            ));
        }
        Ok(())
    }

    fn configure_cwd(&self, command: &mut Command) -> Result<()> {
        self.prepare_cwd()?.configure(command);
        Ok(())
    }

    fn prepare_cwd(&self) -> Result<PreparedRootCwd> {
        #[cfg(test)]
        if FAIL_NEXT_CWD_CLONE.with(|fail| fail.replace(false)) {
            return Err(std::io::Error::from_raw_os_error(libc::EMFILE).into());
        }
        Ok(PreparedRootCwd(self.directory.try_clone()?))
    }

    fn read_entry(&self, path: &str) -> Result<OpenedWorkingEntry> {
        let relative = Path::new(path);
        let leaf = relative
            .file_name()
            .ok_or_else(|| CaptureError::Invalid("working source path has no leaf".into()))?;
        let mut parent = self.directory.try_clone()?;
        if let Some(ancestors) = relative.parent() {
            for component in ancestors.components() {
                let Component::Normal(component) = component else {
                    return Err(CaptureError::Invalid(
                        "working source path escaped its worktree".into(),
                    ));
                };
                match openat_component(&parent, component, libc::O_RDONLY | libc::O_DIRECTORY) {
                    Ok(next) => parent = next,
                    Err(CaptureError::Io(error))
                        if error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        return Ok(OpenedWorkingEntry::Deleted);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        readat_leaf(&parent, leaf)
    }
}

impl PreparedRootCwd {
    fn configure(self, command: &mut Command) {
        let directory = self.0;
        let descriptor = directory.as_raw_fd();
        // SAFETY: only the async-signal-safe fchdir syscall is used after fork. The cloned file is
        // retained by the closure through spawn, so its descriptor remains valid in the child.
        unsafe {
            command.pre_exec(move || {
                let _retain = &directory;
                if libc::fchdir(descriptor) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

fn component_name(component: &OsStr) -> Result<CString> {
    CString::new(component.as_bytes())
        .map_err(|_| CaptureError::Invalid("working source path contains a NUL byte".into()))
}

fn openat_component(parent: &File, component: &OsStr, flags: i32) -> Result<File> {
    let component = component_name(component)?;
    // SAFETY: the parent descriptor and NUL-terminated component remain valid for the call. The
    // flags reject symlinks, and a successful descriptor is returned with unique ownership.
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            component.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn readat_leaf(parent: &File, leaf: &OsStr) -> Result<OpenedWorkingEntry> {
    let leaf = component_name(leaf)?;
    // SAFETY: stat is initialized and both descriptor and NUL-terminated leaf remain valid.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstatat writes one stat value and does not retain either pointer.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(OpenedWorkingEntry::Deleted)
        } else {
            Err(error.into())
        };
    }
    let file_type = stat.st_mode & libc::S_IFMT;
    if file_type == libc::S_IFLNK {
        return readlinkat_bounded(parent, &leaf).map(|bytes| OpenedWorkingEntry::Content {
            kind: "symlink",
            executable: false,
            bytes,
        });
    }
    if file_type == libc::S_IFDIR {
        return Ok(OpenedWorkingEntry::Directory);
    }
    if file_type != libc::S_IFREG {
        return Ok(OpenedWorkingEntry::Unsupported);
    }
    let file = openat_component(parent, OsStr::from_bytes(leaf.as_bytes()), libc::O_RDONLY)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(CaptureError::Invalid(
            "working source changed type while it was captured".into(),
        ));
    }
    if metadata.len() > MAX_SNAPSHOT_FILE_BYTES {
        return Err(CaptureError::Limit(format!(
            "working source file exceeds {MAX_SNAPSHOT_FILE_BYTES} bytes"
        )));
    }
    let executable = metadata.permissions().mode() & 0o111 != 0;
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SNAPSHOT_FILE_BYTES {
        return Err(CaptureError::Limit(format!(
            "working source file exceeds {MAX_SNAPSHOT_FILE_BYTES} bytes"
        )));
    }
    Ok(OpenedWorkingEntry::Content {
        kind: "file",
        executable,
        bytes,
    })
}

fn readlinkat_bounded(parent: &File, leaf: &CString) -> Result<Vec<u8>> {
    let mut capacity = 256usize;
    loop {
        let mut bytes = vec![0_u8; capacity];
        // SAFETY: readlinkat writes at most `capacity` bytes into the valid vector allocation and
        // does not retain any pointer.
        let length = unsafe {
            libc::readlinkat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                bytes.as_mut_ptr().cast(),
                capacity,
            )
        };
        if length < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let length = usize::try_from(length)
            .map_err(|_| CaptureError::Limit("symlink target length is out of range".into()))?;
        if length < capacity {
            bytes.truncate(length);
            return Ok(bytes);
        }
        if capacity as u64 >= MAX_SNAPSHOT_FILE_BYTES {
            return Err(CaptureError::Limit(format!(
                "working source symlink target exceeds {MAX_SNAPSHOT_FILE_BYTES} bytes"
            )));
        }
        capacity = capacity
            .saturating_mul(2)
            .min(MAX_SNAPSHOT_FILE_BYTES as usize);
    }
}

/// Captures a read-only Git identity and explicit mutable-state omissions. It does not touch the
/// index, refs, hooks or working files.
#[cfg(unix)]
pub fn observe_git(request: &GitSourceRequest) -> Result<SourceBindingV1> {
    if !request.repository_path.is_absolute() {
        return Err(CaptureError::Invalid(
            "observed repository path must be absolute".into(),
        ));
    }
    if request.base_ref.is_empty()
        || request.base_ref.starts_with('-')
        || request.base_ref.contains('\0')
    {
        return Err(CaptureError::Invalid("Git base ref is invalid".into()));
    }
    let canonical = request.repository_path.canonicalize()?;
    let opened_root = OpenedGitRoot::open(&canonical)?;
    observe_git_opened(request, &opened_root, true)
}

#[cfg(unix)]
fn observe_git_opened(
    request: &GitSourceRequest,
    opened_root: &OpenedGitRoot,
    require_configured_path: bool,
) -> Result<SourceBindingV1> {
    opened_root.verify_descriptor()?;
    if require_configured_path {
        let canonical = request.repository_path.canonicalize()?;
        opened_root.verify_path(&canonical)?;
    }
    let root = git_line_opened(opened_root, &["rev-parse", "--show-toplevel"])?;
    if require_configured_path
        && request.repository_path.canonicalize()? != PathBuf::from(root).canonicalize()?
    {
        return Err(CaptureError::Invalid(
            "observed repository path must be the canonical worktree root".into(),
        ));
    }
    let head = git_line_opened(opened_root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let tree = git_line_opened(opened_root, &["rev-parse", "--verify", "HEAD^{tree}"])?;
    let base_tip = git_line_opened(
        opened_root,
        &[
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", request.base_ref),
        ],
    )?;
    let base = git_line_opened(opened_root, &["merge-base", &head, &base_tip])?;
    let status = git_bytes_opened(
        opened_root,
        &["status", "--porcelain=v2", "--untracked-files=all"],
        MAX_GIT_OUTPUT,
    )?;
    let mut untracked_count = 0u32;
    let mut working = false;
    let mut index = false;
    let mut submodule = false;
    for line in status
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        if line.starts_with(b"? ") {
            untracked_count = untracked_count.saturating_add(1);
            continue;
        }
        if matches!(line.first(), Some(b'1' | b'2')) && line.len() >= 4 {
            index |= line[2] != b'.';
            working |= line[3] != b'.';
            submodule |= line.windows(2).any(|window| window == b"S.")
                || line.windows(2).any(|window| window == b".S");
        } else {
            working = true;
        }
    }
    let modes = git_bytes_opened(opened_root, &["ls-files", "-s"], MAX_GIT_OUTPUT)?;
    let symlink = modes
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"120000 "));
    submodule |= modes
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"160000 "));
    let mut omissions = Vec::new();
    if working {
        omissions.push(CaptureOmissionV1::WorkingTreeContent);
    }
    if index {
        omissions.push(CaptureOmissionV1::IndexState);
    }
    if untracked_count > 0 {
        omissions.push(CaptureOmissionV1::UntrackedContent);
    }
    if submodule {
        omissions.push(CaptureOmissionV1::SubmoduleContent);
    }
    if symlink {
        omissions.push(CaptureOmissionV1::SymlinkTarget);
    }
    opened_root.verify_descriptor()?;
    Ok(SourceBindingV1::Git {
        repository: request.repository.clone(),
        worktree_id: request.worktree_id.clone(),
        head,
        tree,
        base,
        dirty: !status.is_empty(),
        untracked_count,
        omissions,
        captured_snapshot: None,
        snapshot_coverage: if status.is_empty() {
            CoverageV1::Complete
        } else {
            CoverageV1::Partial
        },
    })
}

/// Retains the exact bytes of tracked and nonignored untracked working files in the private spool.
/// The manifest is local content-addressed evidence; only its opaque artifact reference enters the
/// source binding. Ignored files and submodule contents are deliberately omitted.
#[cfg(unix)]
pub fn capture_git_source(
    request: &GitSourceRequest,
    spool: &LocalSpool,
) -> Result<SourceBindingV1> {
    let root = request.repository_path.canonicalize()?;
    let opened_root = OpenedGitRoot::open(&root)?;
    capture_git_source_opened(request, spool, &opened_root, true)
}

/// Retains the materialized manifest even for clean source, for exact before/after comparisons.
/// This preserves the normal capture bounds, path handling and omission metadata.
#[cfg(unix)]
pub fn capture_git_source_materialized(
    request: &GitSourceRequest,
    spool: &LocalSpool,
) -> Result<SourceBindingV1> {
    let root = request.repository_path.canonicalize()?;
    let opened_root = OpenedGitRoot::open(&root)?;
    capture_git_source_snapshot(request, spool, &opened_root, true, true)
}

#[cfg(unix)]
fn capture_git_source_opened(
    request: &GitSourceRequest,
    spool: &LocalSpool,
    opened_root: &OpenedGitRoot,
    require_configured_path: bool,
) -> Result<SourceBindingV1> {
    capture_git_source_snapshot(request, spool, opened_root, require_configured_path, false)
}

#[cfg(unix)]
fn capture_git_source_snapshot(
    request: &GitSourceRequest,
    spool: &LocalSpool,
    opened_root: &OpenedGitRoot,
    require_configured_path: bool,
    force_materialized: bool,
) -> Result<SourceBindingV1> {
    let mut source = observe_git_opened(request, opened_root, require_configured_path)?;
    let SourceBindingV1::Git {
        head,
        dirty,
        omissions,
        captured_snapshot,
        snapshot_coverage,
        ..
    } = &mut source
    else {
        unreachable!("observe_git always returns a Git source binding")
    };
    opened_root.verify_descriptor()?;
    let listed = git_bytes_opened(
        opened_root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        MAX_GIT_OUTPUT,
    )?;
    opened_root.verify_descriptor()?;
    if !force_materialized
        && !*dirty
        && !requires_materialized_snapshot_opened(opened_root, &listed)?
    {
        opened_root.verify_descriptor()?;
        // A clean gitlink identifies a commit but does not capture submodule source.
        omissions.retain(|omission| *omission == CaptureOmissionV1::SubmoduleContent);
        *snapshot_coverage = if omissions.is_empty() {
            CoverageV1::Complete
        } else {
            CoverageV1::Partial
        };
        return Ok(source);
    }
    let paths: Vec<&[u8]> = listed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect();
    if paths.len() > MAX_SNAPSHOT_FILES {
        return Err(CaptureError::Limit(format!(
            "working source snapshot exceeds {MAX_SNAPSHOT_FILES} files"
        )));
    }
    let mut entries = Vec::with_capacity(paths.len());
    let mut total = 0u64;
    let mut unsupported = Vec::new();
    let mut unsupported_content = false;
    for raw_path in paths {
        let path = std::str::from_utf8(raw_path).map_err(|_| {
            CaptureError::Invalid("working source contains a non-UTF-8 path".into())
        })?;
        validate_relative_path(path)?;
        let (kind, executable, bytes) = match opened_root.read_entry(path)? {
            OpenedWorkingEntry::Deleted => {
                entries.push(WorkingSnapshotEntry {
                    path: path.to_owned(),
                    kind: "deleted",
                    executable: false,
                    artifact: None,
                });
                continue;
            }
            OpenedWorkingEntry::Directory => {
                unsupported.push("submodule_or_directory");
                if !omissions.contains(&CaptureOmissionV1::SubmoduleContent) {
                    omissions.push(CaptureOmissionV1::SubmoduleContent);
                }
                continue;
            }
            OpenedWorkingEntry::Unsupported => {
                unsupported.push("unsupported_file_type");
                unsupported_content = true;
                continue;
            }
            OpenedWorkingEntry::Content {
                kind,
                executable,
                bytes,
            } => (kind, executable, bytes),
        };
        total = total.saturating_add(bytes.len() as u64);
        if total > MAX_SNAPSHOT_TOTAL_BYTES {
            return Err(CaptureError::Limit(format!(
                "working source snapshot exceeds {MAX_SNAPSHOT_TOTAL_BYTES} content bytes"
            )));
        }
        let artifact =
            spool.retain_artifact(&bytes, ArtifactMediaV1::Binary, RetentionClassV1::WorkLocal)?;
        entries.push(WorkingSnapshotEntry {
            path: path.to_owned(),
            kind,
            executable,
            artifact: Some(artifact),
        });
    }
    entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    opened_root.verify_descriptor()?;
    if !omissions.contains(&CaptureOmissionV1::IgnoredContent) {
        omissions.push(CaptureOmissionV1::IgnoredContent);
    }
    omissions.retain(|omission| {
        !matches!(
            omission,
            CaptureOmissionV1::WorkingTreeContent
                | CaptureOmissionV1::UntrackedContent
                | CaptureOmissionV1::SymlinkTarget
        )
    });
    if unsupported_content {
        omissions.push(CaptureOmissionV1::WorkingTreeContent);
    }
    let manifest = serde_json::to_vec(&WorkingSnapshotManifest {
        schema_version: SCHEMA_VERSION_V1,
        head: head.clone(),
        entries,
        unsupported,
    })?;
    *captured_snapshot = Some(spool.retain_artifact(
        &manifest,
        ArtifactMediaV1::Json,
        RetentionClassV1::WorkLocal,
    )?);
    *snapshot_coverage = if omissions.is_empty() {
        CoverageV1::Complete
    } else {
        CoverageV1::Partial
    };
    Ok(source)
}

/// Runs one observational local command after its exact start binding is durable. This executes
/// with the current user's authority and therefore never claims protected or independent proof.
#[cfg(unix)]
pub fn run_observed_command(
    request: ObservedCommandRequest,
    spool: &LocalSpool,
) -> Result<ObservedCommand> {
    validate_command_request(&request)?;
    let recipe = RecipeIdentity {
        schema_version: SCHEMA_VERSION_V1,
        argv: &request.argv,
        timeout_ms: request.timeout.as_millis(),
        max_output_bytes: request.max_output_bytes,
    };
    let recipe_digest = sha256(&serde_json::to_vec(&recipe)?);
    run_observed_command_inner(
        request,
        spool,
        recipe_digest,
        ObservedExecution {
            kind: ActionKindV1::Execution,
            recipe_provenance: RecipeProvenanceV1::ObservedNativeInput,
            tee: false,
            opened_root: None,
            prepared_cwd: None,
            admitted_start: None,
        },
        &Cancellation::default(),
        |_, _| Ok(()),
    )
}

#[cfg(unix)]
pub fn run_configured_recipe(
    request: ConfiguredRecipeRunRequest,
    spool: &LocalSpool,
) -> Result<ObservedCommand> {
    run_configured_recipe_with_cancellation(request, spool, &Cancellation::default())
}

#[cfg(unix)]
pub fn run_configured_recipe_with_cancellation(
    request: ConfiguredRecipeRunRequest,
    spool: &LocalSpool,
    cancellation: &Cancellation,
) -> Result<ObservedCommand> {
    let (recipe_digest, opened_root, reservation) = configured_recipe_binding(&request)?;
    let Some(_fence_guard) = spool.try_recipe_fence(&reservation)? else {
        return Err(CaptureError::Conflict(
            "configured recipe checkout is busy with another live caller".into(),
        ));
    };
    let (mut attempt, created) = spool.reserve_recipe_attempt(&reservation)?;
    let completed = match (
        spool.action_start(&attempt.start_operation_id)?,
        spool.action_finish(&attempt.finish_operation_id)?,
    ) {
        (Some(_), None) => {
            return Err(CaptureError::Conflict(format!(
                "configured recipe attempt {} is unresolved; explicit reconciliation is required",
                attempt.action_id
            )));
        }
        (Some(_), Some(_)) => {
            // The previous supervisor reached a durable terminal receipt but crashed before
            // clearing the fence. The normal replay path below returns that receipt without
            // launching the child and clears the fence afterward.
            true
        }
        (None, Some(_)) => {
            return Err(CaptureError::Conflict(
                "configured recipe finish exists without its durable start".into(),
            ));
        }
        (None, None) if !created => {
            if matches!(attempt.phase, spool::RecipeAttemptPhaseV1::Spawned { .. }) {
                return Err(CaptureError::Conflict(
                    "spawned recipe attempt lost its durable action start".into(),
                ));
            }
            if let Some(order) = spool.retained_observed_order(&attempt.start_operation_id)? {
                spool.release_observed_order(&attempt.start_operation_id, order)?;
            }
            spool.clear_recipe_attempt(&attempt)?;
            (attempt, _) = spool.reserve_recipe_attempt(&reservation)?;
            false
        }
        (None, None) => false,
    };
    let command = ObservedCommandRequest {
        start_operation_id: attempt.start_operation_id.clone(),
        finish_operation_id: attempt.finish_operation_id.clone(),
        work_id: request.work_id,
        generation: request.generation,
        session_id: request.session_id,
        action_id: attempt.action_id.clone(),
        source: request.source,
        argv: request.recipe.argv,
        setup_digest: request.setup_digest,
        coverage: request.recipe.coverage,
        timeout: Duration::from_secs(request.recipe.timeout_seconds),
        max_output_bytes: request.recipe.max_output_bytes,
    };
    validate_command_request(&command)?;
    if completed {
        let observed = run_observed_command_inner(
            command,
            spool,
            recipe_digest,
            ObservedExecution {
                kind: ActionKindV1::Verification,
                recipe_provenance: RecipeProvenanceV1::ConfiguredExactMatch,
                tee: true,
                opened_root: Some(&opened_root),
                prepared_cwd: None,
                admitted_start: None,
            },
            cancellation,
            |_, _| Ok(()),
        )?;
        spool.clear_recipe_attempt(&attempt)?;
        return Ok(observed);
    }
    let prepared_cwd = opened_root.prepare_cwd()?;
    let source = capture_git_source_opened(&command.source, spool, &opened_root, true)?;
    let admitted_start = spool.admit_configured_start(
        &attempt,
        ActionStartV1 {
            schema_version: SCHEMA_VERSION_V1,
            operation_id: command.start_operation_id.clone(),
            work_id: command.work_id.clone(),
            generation: command.generation,
            session_id: command.session_id.clone(),
            action_id: command.action_id.clone(),
            source,
            kind: ActionKindV1::Verification,
            recipe_provenance: RecipeProvenanceV1::ConfiguredExactMatch,
            recipe_digest: recipe_digest.clone(),
            setup_digest: command.setup_digest.clone(),
            started_at: now_seconds()?,
            observed_order: 0,
            observation_scope_id: None,
        },
    )?;
    let callback_spool = spool.clone();
    let callback_fence = attempt.fence_id.clone();
    let callback_marker = attempt.marker_id.clone();
    let observed = run_observed_command_inner(
        command,
        spool,
        recipe_digest,
        ObservedExecution {
            kind: ActionKindV1::Verification,
            recipe_provenance: RecipeProvenanceV1::ConfiguredExactMatch,
            tee: true,
            opened_root: Some(&opened_root),
            prepared_cwd: Some(prepared_cwd),
            admitted_start: Some(admitted_start),
        },
        cancellation,
        move |pid, pgid| {
            callback_spool
                .mark_recipe_attempt_spawned(&callback_fence, &callback_marker, pid, pgid)
                .map_err(capture_io_error)
        },
    );
    match observed {
        Ok(observed) => {
            spool.clear_recipe_attempt(&attempt)?;
            Ok(observed)
        }
        Err(error) => {
            if spool.action_start(&attempt.start_operation_id)?.is_none() {
                if let Some(order) = spool.retained_observed_order(&attempt.start_operation_id)? {
                    spool.release_observed_order(&attempt.start_operation_id, order)?;
                }
                spool.clear_recipe_attempt(&attempt)?;
            }
            Err(error)
        }
    }
}

/// Bind an operator reservation before launch or receipt lookup, including the opened root inode.
#[cfg(unix)]
pub fn operator_recipe_identity(request: &ConfiguredRecipeRunRequest) -> Result<String> {
    let (recipe_digest, _, binding) = configured_recipe_binding(request)?;
    operator_identity(request, &recipe_digest, &binding)
}

#[cfg(unix)]
fn operator_identity(
    request: &ConfiguredRecipeRunRequest,
    recipe_digest: &str,
    binding: &RecipeAttemptReservationV1,
) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(&(
        "agentscm/operator-recipe/v1",
        &request.work_id,
        request.generation,
        &request.session_id,
        &request.managed_invocation,
        &request.setup_digest,
        recipe_digest,
        (
            &request.source.repository,
            &request.source.worktree_id,
            &request.source.repository_path,
            &request.source.base_ref,
            binding.root_device,
            binding.root_inode,
        ),
    ))?))
}

/// Runs an owner-selected exact recipe outside native hooks, using stable caller operation IDs.
/// `reconcile` never launches. Missing starts return None only for the reserved identity;
/// unfinished starts require independently observed process/group quiescence.
#[cfg(unix)]
pub fn run_operator_recipe<After>(
    request: ConfiguredRecipeRunRequest,
    spool: &LocalSpool,
    expected_identity: &str,
    reconcile: bool,
    quiescent: bool,
    after_spawn: After,
) -> Result<Option<ObservedCommand>>
where
    After: FnOnce(u32, u32) -> std::io::Result<()>,
{
    let (recipe_digest, opened_root, binding) = configured_recipe_binding(&request)?;
    let identity = operator_identity(&request, &recipe_digest, &binding)?;
    if identity != expected_identity {
        return Err(CaptureError::Conflict(
            "operator recipe identity changed from its reservation; preserve the original outcome"
                .into(),
        ));
    }
    let start_operation_id = format!("operator:start:{identity}");
    let finish_operation_id = format!("operator:finish:{identity}");
    let retained_start = spool.action_start(&start_operation_id)?;
    let retained_finish = spool.action_finish(&finish_operation_id)?;
    if retained_start.is_none() && retained_finish.is_some() {
        return Err(CaptureError::Conflict(
            "operator recipe finish exists without its durable start".into(),
        ));
    }
    if reconcile && retained_start.is_none() {
        return Ok(None);
    }
    if retained_start.is_some() && retained_finish.is_none() && (!reconcile || !quiescent) {
        return Err(CaptureError::Conflict("operator recipe has an unresolved start; observe the recorded process/group stopping before reconciliation".into()));
    }
    let command = ObservedCommandRequest {
        start_operation_id,
        finish_operation_id,
        work_id: request.work_id,
        generation: request.generation,
        session_id: request.session_id,
        action_id: format!("operator:{identity}"),
        source: request.source,
        argv: request.recipe.argv,
        setup_digest: request.setup_digest,
        coverage: request.recipe.coverage,
        timeout: Duration::from_secs(request.recipe.timeout_seconds),
        max_output_bytes: request.recipe.max_output_bytes,
    };
    validate_command_request(&command)?;
    let prepared_cwd = if reconcile {
        None
    } else {
        Some(opened_root.prepare_cwd()?)
    };
    run_observed_command_inner(
        command,
        spool,
        recipe_digest,
        ObservedExecution {
            kind: ActionKindV1::Verification,
            recipe_provenance: RecipeProvenanceV1::ConfiguredExactMatch,
            tee: false,
            opened_root: Some(&opened_root),
            prepared_cwd,
            admitted_start: None,
        },
        &Cancellation::default(),
        after_spawn,
    )
    .map(Some)
}

#[cfg(unix)]
pub fn reconcile_configured_recipe(
    request: ConfiguredRecipeRunRequest,
    spool: &LocalSpool,
) -> Result<RecipeReconcileOutcome> {
    reconcile_retained_configured_recipe(
        ConfiguredRecipeReconcileRequest {
            work_id: request.work_id,
            source: request.source,
            recipe_id: request.recipe.id,
        },
        spool,
    )
}

#[cfg(unix)]
pub fn reconcile_retained_configured_recipe(
    request: ConfiguredRecipeReconcileRequest,
    spool: &LocalSpool,
) -> Result<RecipeReconcileOutcome> {
    if request.work_id.is_empty()
        || request.work_id.len() > 255
        || request.recipe_id.is_empty()
        || request.recipe_id.len() > 128
    {
        return Err(CaptureError::Invalid(
            "configured recipe reconcile identity is invalid".into(),
        ));
    }
    let canonical_root = request.source.repository_path.canonicalize()?;
    if canonical_root != request.source.repository_path {
        return Err(CaptureError::Invalid(
            "configured recipe root must be canonical".into(),
        ));
    }
    let root_path = canonical_root
        .to_str()
        .ok_or_else(|| CaptureError::Invalid("configured recipe root must be UTF-8".into()))?
        .to_owned();
    let fence_id = sha256(
        format!(
            "{}\0{}\0{}\0{}\0{}",
            request.work_id,
            root_path,
            request.source.repository,
            request.source.worktree_id,
            request.recipe_id
        )
        .as_bytes(),
    );
    let Some(attempt) = spool.recipe_attempt(&fence_id)? else {
        return Ok(RecipeReconcileOutcome::None);
    };
    if attempt.work_id != request.work_id
        || attempt.repository != request.source.repository
        || attempt.worktree_id != request.source.worktree_id
        || attempt.root_path != root_path
        || attempt.recipe_id != request.recipe_id
    {
        return Err(CaptureError::Conflict(
            "retained configured recipe attempt differs from current authorization".into(),
        ));
    }
    let reservation = RecipeAttemptReservationV1 {
        fence_id: attempt.fence_id.clone(),
        work_id: attempt.work_id.clone(),
        generation: attempt.generation,
        observed_session_id: attempt.observed_session_id.clone(),
        canonical_session_id: attempt.canonical_session_id.clone(),
        repository: attempt.repository.clone(),
        worktree_id: attempt.worktree_id.clone(),
        root_path: attempt.root_path.clone(),
        root_device: attempt.root_device,
        root_inode: attempt.root_inode,
        base_ref: attempt.base_ref.clone(),
        recipe_id: attempt.recipe_id.clone(),
        recipe_digest: attempt.recipe_digest.clone(),
        setup_digest: attempt.setup_digest.clone(),
        coverage: attempt.coverage,
        // The checkout fence uses only root/repository/worktree. This bounded digest satisfies
        // the shared reservation validator; retained attempt facts remain authoritative below.
        command_digest: attempt.recipe_digest.clone(),
        host_id: attempt.host_id.clone(),
        boot_id: attempt.boot_id.clone(),
    };
    let Some(_fence_guard) = spool.try_recipe_fence(&reservation)? else {
        return Ok(RecipeReconcileOutcome::Busy {
            fence_id: reservation.fence_id,
        });
    };
    let _managed_guard = spool.lock_managed_capture()?;
    let Some(attempt) = spool.recipe_attempt(&fence_id)? else {
        return Ok(RecipeReconcileOutcome::None);
    };
    if attempt.work_id != request.work_id
        || attempt.repository != request.source.repository
        || attempt.worktree_id != request.source.worktree_id
        || attempt.root_path != root_path
        || attempt.recipe_id != request.recipe_id
    {
        return Err(CaptureError::Conflict(
            "retained configured recipe attempt changed during reconciliation".into(),
        ));
    }
    // Reconciliation addresses the stable Work/root/recipe fence. A reboot may also resume the
    // Work under a new session or generation, and recipe bytes may have changed. The terminal
    // receipt below therefore uses only retained attempt facts.
    let start = spool.action_start(&attempt.start_operation_id)?;
    if start.is_none() {
        if matches!(attempt.phase, spool::RecipeAttemptPhaseV1::Spawned { .. }) {
            return Ok(RecipeReconcileOutcome::Unresolved {
                action_id: attempt.action_id.clone(),
                phase: recipe_attempt_phase_name(&attempt),
            });
        }
        match spool.managed_invocation_by_id(&attempt.marker_id) {
            Ok(Some(marker)) if marker.marker_id == attempt.marker_id => {}
            Ok(_) => {
                return Ok(RecipeReconcileOutcome::Unresolved {
                    action_id: attempt.action_id.clone(),
                    phase: "reserved:missing_occurrence_claim".into(),
                });
            }
            Err(error) => return Err(error),
        }
        if let Some(order) = spool.retained_observed_order(&attempt.start_operation_id)? {
            spool.release_observed_order(&attempt.start_operation_id, order)?;
        }
        spool.clear_recipe_attempt(&attempt)?;
        return Ok(RecipeReconcileOutcome::ClearedPreSpawn {
            action_id: attempt.action_id.clone(),
        });
    }
    let start = start.expect("checked retained recipe start");
    if start.work_id != attempt.work_id
        || start.generation != attempt.generation
        || start.session_id != attempt.canonical_session_id
        || start.action_id != attempt.action_id
        || start.recipe_digest != attempt.recipe_digest
        || start.setup_digest != attempt.setup_digest
        || start.kind != ActionKindV1::Verification
        || start.recipe_provenance != RecipeProvenanceV1::ConfiguredExactMatch
    {
        return Err(CaptureError::Conflict(
            "retained recipe start differs from its attempt fence".into(),
        ));
    }
    if let Some(finish) = spool.action_finish(&attempt.finish_operation_id)? {
        spool.clear_recipe_attempt(&attempt)?;
        if spool
            .managed_invocation_by_id(&attempt.marker_id)?
            .is_some_and(|marker| marker.post_fingerprint.is_some())
        {
            spool.remove_managed_invocation(&attempt.marker_id)?;
        }
        return Ok(RecipeReconcileOutcome::Reconciled(finish));
    }
    let current_host = local_host_identity();
    let current_boot = local_boot_identity();
    let reboot_proven = attempt.host_id.is_some()
        && attempt.host_id == current_host
        && attempt.boot_id.is_some()
        && current_boot.is_some()
        && attempt.boot_id != current_boot;
    if !reboot_proven {
        return Ok(RecipeReconcileOutcome::Unresolved {
            action_id: attempt.action_id.clone(),
            phase: recipe_attempt_phase_name(&attempt),
        });
    }
    let finish = incomplete_recipe_finish(&attempt)?;
    spool.append(&CaptureEnvelopeV1::ActionFinish(finish.clone()))?;
    spool.clear_recipe_attempt(&attempt)?;
    if spool
        .managed_invocation_by_id(&attempt.marker_id)?
        .is_some_and(|marker| marker.post_fingerprint.is_some())
    {
        spool.remove_managed_invocation(&attempt.marker_id)?;
    }
    Ok(RecipeReconcileOutcome::Reconciled(finish))
}

#[cfg(unix)]
fn incomplete_recipe_finish(attempt: &spool::RecipeAttemptV1) -> Result<ActionFinishV1> {
    Ok(ActionFinishV1 {
        schema_version: SCHEMA_VERSION_V1,
        operation_id: attempt.finish_operation_id.clone(),
        work_id: attempt.work_id.clone(),
        generation: attempt.generation,
        action_id: attempt.action_id.clone(),
        outcome: ActionOutcomeV1::Incomplete,
        result: CheckResultV1::Unknown,
        assurance: AssuranceBasisV1::CallerAsserted,
        coverage: attempt.coverage,
        finished_at: now_seconds()?,
        drift: DriftV1::Unknown,
        output_artifacts: Vec::new(),
    })
}

#[cfg(unix)]
fn configured_recipe_binding(
    request: &ConfiguredRecipeRunRequest,
) -> Result<(String, OpenedGitRoot, RecipeAttemptReservationV1)> {
    request.recipe.validate()?;
    let bounded_id = |value: &str| {
        !value.is_empty()
            && value.len() <= 255
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'@')
            })
    };
    if !bounded_id(&request.work_id)
        || !bounded_id(&request.observed_session_id)
        || !bounded_id(&request.session_id)
        || request.generation <= 0
        || request.managed_invocation.is_empty()
        || request.managed_invocation.len() > 4_096
        || request.managed_invocation.contains('\0')
        || request.setup_digest.len() != 64
        || !request
            .setup_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CaptureError::Invalid(
            "configured recipe invocation binding is invalid".into(),
        ));
    }
    let canonical_root = request.source.repository_path.canonicalize()?;
    if canonical_root != request.source.repository_path {
        return Err(CaptureError::Invalid(
            "configured recipe root must be canonical".into(),
        ));
    }
    let opened_root = OpenedGitRoot::open(&canonical_root)?;
    let root_path = canonical_root
        .to_str()
        .ok_or_else(|| CaptureError::Invalid("configured recipe root must be UTF-8".into()))?
        .to_owned();
    let recipe_digest = request.recipe.digest()?;
    let fence_id = sha256(
        format!(
            "{}\0{}\0{}\0{}\0{}",
            request.work_id,
            root_path,
            request.source.repository,
            request.source.worktree_id,
            request.recipe.id
        )
        .as_bytes(),
    );
    let reservation = RecipeAttemptReservationV1 {
        fence_id,
        work_id: request.work_id.clone(),
        generation: request.generation,
        observed_session_id: request.observed_session_id.clone(),
        canonical_session_id: request.session_id.clone(),
        repository: request.source.repository.clone(),
        worktree_id: request.source.worktree_id.clone(),
        root_path,
        root_device: opened_root.device,
        root_inode: opened_root.inode,
        base_ref: request.source.base_ref.clone(),
        recipe_id: request.recipe.id.clone(),
        recipe_digest: recipe_digest.clone(),
        setup_digest: request.setup_digest.clone(),
        coverage: request.recipe.coverage,
        command_digest: sha256(request.managed_invocation.as_bytes()),
        host_id: local_host_identity(),
        boot_id: local_boot_identity(),
    };
    Ok((recipe_digest, opened_root, reservation))
}

fn recipe_attempt_phase_name(attempt: &RecipeAttemptV1) -> String {
    match attempt.phase {
        spool::RecipeAttemptPhaseV1::Reserved => "reserved".into(),
        spool::RecipeAttemptPhaseV1::Spawned { pid, pgid } => {
            format!("spawned:{pid}:{pgid}")
        }
    }
}

fn capture_io_error(error: CaptureError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

#[cfg(unix)]
struct ObservedExecution<'a> {
    kind: ActionKindV1,
    recipe_provenance: RecipeProvenanceV1,
    tee: bool,
    opened_root: Option<&'a OpenedGitRoot>,
    prepared_cwd: Option<PreparedRootCwd>,
    admitted_start: Option<ActionStartV1>,
}

#[cfg(unix)]
fn run_observed_command_inner<After>(
    request: ObservedCommandRequest,
    spool: &LocalSpool,
    recipe_digest: String,
    execution: ObservedExecution<'_>,
    cancellation: &Cancellation,
    after_spawn: After,
) -> Result<ObservedCommand>
where
    After: FnOnce(u32, u32) -> std::io::Result<()>,
{
    let ObservedExecution {
        kind,
        recipe_provenance,
        tee,
        opened_root,
        mut prepared_cwd,
        admitted_start,
    } = execution;
    let start = if let Some(start) = spool.action_start(&request.start_operation_id)? {
        let observed_order = spool.observed_order_for(&request.start_operation_id)?;
        let observation_scope_id = Some(spool.observation_scope_id()?);
        if start.work_id != request.work_id
            || start.generation != request.generation
            || start.session_id != request.session_id
            || start.action_id != request.action_id
            || start.kind != kind
            || start.recipe_provenance != recipe_provenance
            || start.recipe_digest != recipe_digest
            || start.setup_digest != request.setup_digest
            || start.observed_order != observed_order
            || start.observation_scope_id != observation_scope_id
        {
            return Err(CaptureError::Conflict(
                "observed command replay differs from retained action start".into(),
            ));
        }
        if let Some(finish) = spool.action_finish(&request.finish_operation_id)? {
            if finish.work_id != request.work_id
                || finish.generation != request.generation
                || finish.action_id != request.action_id
                || finish.coverage != request.coverage
            {
                return Err(CaptureError::Conflict(
                    "observed command replay differs from retained action finish".into(),
                ));
            }
            return Ok(ObservedCommand {
                start,
                stop: match finish.outcome {
                    ActionOutcomeV1::Succeeded | ActionOutcomeV1::Failed => ObservedStop::Exited,
                    ActionOutcomeV1::Incomplete => ObservedStop::Io,
                    ActionOutcomeV1::Unknown => ObservedStop::Io,
                },
                finish,
                exit_code: None,
                signal: None,
                storage_warnings: vec![
                    "configured invocation was already completed; retained receipt was reused"
                        .into(),
                ],
            });
        }
        if admitted_start.as_ref() == Some(&start) {
            start
        } else {
            let finish = ActionFinishV1 {
                schema_version: SCHEMA_VERSION_V1,
                operation_id: request.finish_operation_id,
                work_id: request.work_id,
                generation: request.generation,
                action_id: request.action_id,
                outcome: ActionOutcomeV1::Incomplete,
                result: if kind == ActionKindV1::Verification {
                    CheckResultV1::Incomplete
                } else {
                    CheckResultV1::Unknown
                },
                assurance: AssuranceBasisV1::CallerAsserted,
                coverage: request.coverage,
                finished_at: now_seconds()?,
                drift: DriftV1::Unknown,
                output_artifacts: Vec::new(),
            };
            spool.append(&CaptureEnvelopeV1::ActionFinish(finish.clone()))?;
            return Ok(ObservedCommand {
                start,
                finish,
                exit_code: None,
                signal: None,
                stop: ObservedStop::Io,
                storage_warnings: vec![
                    "retained start had no finish; command was not launched again".into(),
                ],
            });
        }
    } else {
        if admitted_start.is_some() {
            return Err(CaptureError::Conflict(
                "admitted configured start is unavailable from the spool".into(),
            ));
        }
        if spool.action_finish(&request.finish_operation_id)?.is_some() {
            return Err(CaptureError::Conflict(
                "observed command finish exists without its action start".into(),
            ));
        }
        // Clone the retained cwd descriptor before the durable ActionStart. Any fallible descriptor
        // preparation therefore leaves a Reserved/no-start attempt that the live fence owner can
        // clear without requiring reboot reconciliation.
        prepared_cwd = match prepared_cwd {
            Some(cwd) => Some(cwd),
            None => opened_root.map(OpenedGitRoot::prepare_cwd).transpose()?,
        };
        let source = match opened_root {
            Some(root) => capture_git_source_opened(&request.source, spool, root, true)?,
            None => capture_git_source(&request.source, spool)?,
        };
        let started_at = now_seconds()?;
        let (_, start) = spool.append_action_start_ordered(ActionStartV1 {
            schema_version: SCHEMA_VERSION_V1,
            operation_id: request.start_operation_id.clone(),
            work_id: request.work_id.clone(),
            generation: request.generation,
            session_id: request.session_id.clone(),
            action_id: request.action_id.clone(),
            source: source.clone(),
            kind,
            recipe_provenance,
            recipe_digest,
            setup_digest: request.setup_digest.clone(),
            started_at,
            observed_order: 0,
            observation_scope_id: None,
        })?;
        start
    };
    let source = start.source.clone();

    let mut command = Command::new(&request.argv[0]);
    command
        .args(&request.argv[1..])
        .env_clear()
        .process_group(0);
    match prepared_cwd {
        Some(cwd) => cwd.configure(&mut command),
        None => {
            command.current_dir(&request.source.repository_path);
        }
    }
    for name in [
        "PATH",
        "HOME",
        "USER",
        "TMPDIR",
        "LANG",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "CARGO_TARGET_DIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let captured = if tee {
        capture_with_input_tee_after_spawn(
            &mut command,
            None,
            request.timeout,
            request.max_output_bytes,
            cancellation,
            std::io::stdout(),
            std::io::stderr(),
            after_spawn,
        )
    } else {
        capture_with_input_tee_after_spawn(
            &mut command,
            None,
            request.timeout,
            request.max_output_bytes,
            cancellation,
            std::io::sink(),
            std::io::sink(),
            after_spawn,
        )
    };
    let (exit_code, signal, stdout, stderr, stop, outcome) = match captured {
        Ok(captured) => {
            let (stop, outcome) = match (captured.stop, captured.code, captured.signal) {
                // Incomplete collection keeps today's Io mapping whatever the primary stop.
                _ if captured.incomplete => (ObservedStop::Io, ActionOutcomeV1::Unknown),
                (None, Some(0), None) => (ObservedStop::Exited, ActionOutcomeV1::Succeeded),
                (None, Some(_), None) => (ObservedStop::Exited, ActionOutcomeV1::Failed),
                (None, None, Some(_)) => (ObservedStop::Signalled, ActionOutcomeV1::Incomplete),
                (None, _, _) => (ObservedStop::Io, ActionOutcomeV1::Unknown),
                (Some(Stop::Timeout), _, _) => {
                    (ObservedStop::TimedOut, ActionOutcomeV1::Incomplete)
                }
                (Some(Stop::Cancelled), _, _) => {
                    (ObservedStop::Cancelled, ActionOutcomeV1::Incomplete)
                }
                (Some(Stop::Output), _, _) => {
                    (ObservedStop::OutputLimit, ActionOutcomeV1::Incomplete)
                }
                (Some(Stop::Io), _, _) => (ObservedStop::Io, ActionOutcomeV1::Unknown),
            };
            (
                captured.code,
                captured.signal,
                captured.stdout,
                captured.stderr,
                stop,
                outcome,
            )
        }
        Err(_) => (
            None,
            None,
            Vec::new(),
            Vec::new(),
            ObservedStop::SpawnFailed,
            ActionOutcomeV1::Unknown,
        ),
    };
    let drift = match opened_root.map_or_else(
        || capture_git_source(&request.source, spool),
        |root| capture_git_source_opened(&request.source, spool, root, false),
    ) {
        Ok(after) if after == source => DriftV1::NoneObserved,
        Ok(_) => DriftV1::SourceChanged,
        Err(_) => DriftV1::Unknown,
    };
    let mut storage_warnings = Vec::new();
    let mut output_artifacts = Vec::new();
    retain_or_missing(
        spool,
        &stdout,
        ArtifactMediaV1::Binary,
        &mut output_artifacts,
        &mut storage_warnings,
        "stdout",
    );
    retain_or_missing(
        spool,
        &stderr,
        ArtifactMediaV1::Binary,
        &mut output_artifacts,
        &mut storage_warnings,
        "stderr",
    );
    let stop_name = match stop {
        ObservedStop::Exited => "exited",
        ObservedStop::Signalled => "signalled",
        ObservedStop::TimedOut => "timed_out",
        ObservedStop::Cancelled => "cancelled",
        ObservedStop::OutputLimit => "output_limit",
        ObservedStop::Io => "io_error",
        ObservedStop::SpawnFailed => "spawn_failed",
    };
    let local_record = serde_json::to_vec(&LocalCommandRecord {
        schema_version: SCHEMA_VERSION_V1,
        action_id: &request.action_id,
        argv: &request.argv,
        exit_code,
        signal,
        stop: stop_name,
        provenance: "agentscm_child_process_observation",
        assurance_limit: "caller_controlled_local_environment",
    })?;
    retain_or_missing(
        spool,
        &local_record,
        ArtifactMediaV1::Json,
        &mut output_artifacts,
        &mut storage_warnings,
        "command descriptor",
    );
    let finish = ActionFinishV1 {
        schema_version: SCHEMA_VERSION_V1,
        operation_id: request.finish_operation_id,
        work_id: request.work_id,
        generation: request.generation,
        action_id: request.action_id,
        outcome,
        result: if kind == ActionKindV1::Verification {
            match (&stop, exit_code) {
                (ObservedStop::Exited, Some(0)) => CheckResultV1::Passed,
                (ObservedStop::Exited, Some(_)) => CheckResultV1::Failed,
                (ObservedStop::Signalled, _) => CheckResultV1::Unknown,
                (
                    ObservedStop::TimedOut | ObservedStop::Cancelled | ObservedStop::OutputLimit,
                    _,
                ) => CheckResultV1::Incomplete,
                (ObservedStop::Io | ObservedStop::SpawnFailed, _) => CheckResultV1::Unknown,
                (ObservedStop::Exited, None) => CheckResultV1::Unknown,
            }
        } else {
            CheckResultV1::Unknown
        },
        assurance: AssuranceBasisV1::CallerAsserted,
        coverage: request.coverage,
        finished_at: now_seconds()?,
        drift,
        output_artifacts,
    };
    spool.append(&CaptureEnvelopeV1::ActionFinish(finish.clone()))?;
    Ok(ObservedCommand {
        start,
        finish,
        exit_code,
        signal,
        stop,
        storage_warnings,
    })
}

#[cfg(unix)]
fn validate_command_request(request: &ObservedCommandRequest) -> Result<()> {
    if request.argv.is_empty() || request.argv.len() > MAX_COMMAND_ARGS {
        return Err(CaptureError::Limit(format!(
            "observed command must contain 1..{MAX_COMMAND_ARGS} arguments"
        )));
    }
    if !Path::new(&request.argv[0]).is_absolute() {
        return Err(CaptureError::Invalid(
            "observed command executable must be an absolute path".into(),
        ));
    }
    let bytes = request.argv.iter().try_fold(0usize, |total, value| {
        if value.contains('\0') {
            return Err(CaptureError::Invalid(
                "command argument contains NUL".into(),
            ));
        }
        total
            .checked_add(value.len())
            .ok_or_else(|| CaptureError::Limit("command argument bytes overflow".into()))
    })?;
    if bytes > MAX_COMMAND_ARG_BYTES {
        return Err(CaptureError::Limit(format!(
            "observed command arguments exceed {MAX_COMMAND_ARG_BYTES} bytes"
        )));
    }
    if request.timeout.is_zero() || request.timeout > Duration::from_secs(300) {
        return Err(CaptureError::Invalid(
            "observed command timeout must be 1 ms..300 s".into(),
        ));
    }
    if !(256..=MAX_COMMAND_OUTPUT).contains(&request.max_output_bytes) {
        return Err(CaptureError::Invalid(format!(
            "observed command output limit must be 256..{MAX_COMMAND_OUTPUT} bytes"
        )));
    }
    Ok(())
}

fn retain_or_missing(
    spool: &LocalSpool,
    bytes: &[u8],
    media: ArtifactMediaV1,
    artifacts: &mut Vec<ArtifactRefV1>,
    warnings: &mut Vec<String>,
    label: &str,
) {
    match spool.retain_artifact(bytes, media, RetentionClassV1::WorkLocal) {
        Ok(reference) => artifacts.push(reference),
        Err(error) => {
            artifacts.push(spool.missing_artifact_reference(
                bytes,
                media,
                RetentionClassV1::WorkLocal,
            ));
            warnings.push(format!(
                "{label} was observed but local retention failed: {error}"
            ));
        }
    }
}

#[cfg(unix)]
fn git_line_opened(root: &OpenedGitRoot, args: &[&str]) -> Result<String> {
    let bytes = git_bytes_opened(root, args, 4096)?;
    let line = std::str::from_utf8(&bytes)
        .map_err(|_| CaptureError::Invalid("Git output is not UTF-8".into()))?
        .trim();
    if line.is_empty() || line.contains(['\n', '\r']) {
        return Err(CaptureError::Invalid(
            "Git returned an invalid single line".into(),
        ));
    }
    Ok(line.to_owned())
}

#[cfg(unix)]
fn git_bytes_opened(root: &OpenedGitRoot, args: &[&str], limit: u64) -> Result<Vec<u8>> {
    git_bytes_with_input_opened(root, args, None, limit)
}

#[cfg(unix)]
fn git_bytes_with_input_opened(
    root: &OpenedGitRoot,
    args: &[&str],
    input: Option<Vec<u8>>,
    limit: u64,
) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("LC_ALL", "C")
        // Source observation must not hydrate a partial clone as a side effect.
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .args(args)
        .process_group(0);
    root.configure_cwd(&mut command)?;
    let captured = capture_with_input(
        &mut command,
        input,
        Duration::from_secs(60),
        limit,
        &Cancellation::default(),
    )?;
    if captured.stop.is_some() || captured.code != Some(0) {
        return Err(CaptureError::Invalid(format!(
            "Git {} failed",
            args.first().copied().unwrap_or("operation")
        )));
    }
    Ok(captured.stdout)
}

#[cfg(unix)]
fn requires_materialized_snapshot_opened(root: &OpenedGitRoot, paths: &[u8]) -> Result<bool> {
    let attributes = git_bytes_with_input_opened(
        root,
        &[
            "check-attr",
            "--stdin",
            "-z",
            "filter",
            "working-tree-encoding",
            "ident",
            "eol",
        ],
        Some(paths.to_vec()),
        MAX_GIT_OUTPUT,
    )?;
    let fields: Vec<&[u8]> = attributes.split(|byte| *byte == 0).collect();
    if (!attributes.is_empty() && attributes.last() != Some(&0))
        || !fields.len().saturating_sub(1).is_multiple_of(3)
    {
        return Err(CaptureError::Invalid(
            "Git returned malformed working-tree attributes".into(),
        ));
    }
    if fields[..fields.len().saturating_sub(1)]
        .chunks_exact(3)
        .any(|triple| !matches!(triple[2], b"unspecified" | b"unset"))
    {
        return Ok(true);
    }
    Ok(matches!(
        git_optional_config_opened(root, "core.autocrlf")?.as_deref(),
        Some("true")
    ) || matches!(
        git_optional_config_opened(root, "core.eol")?.as_deref(),
        Some("crlf")
    ))
}

#[cfg(unix)]
fn git_optional_config_opened(root: &OpenedGitRoot, name: &str) -> Result<Option<String>> {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .args(["config", "--get", name])
        .process_group(0);
    root.configure_cwd(&mut command)?;
    let captured = capture_with_input(
        &mut command,
        None,
        Duration::from_secs(60),
        4096,
        &Cancellation::default(),
    )?;
    match (captured.stop, captured.code) {
        (None, Some(0)) => {
            let value = std::str::from_utf8(&captured.stdout)
                .map_err(|_| CaptureError::Invalid("Git config value is not UTF-8".into()))?
                .trim_end_matches(['\r', '\n']);
            Ok(Some(value.to_ascii_lowercase()))
        }
        (None, Some(1)) => Ok(None),
        _ => Err(CaptureError::Invalid(format!(
            "Git config lookup for {name} failed"
        ))),
    }
}

#[cfg(unix)]
fn validate_relative_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.contains(['\0', '\\'])
        || path.chars().any(char::is_control)
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(CaptureError::Invalid(
            "working source contains an unsafe or non-portable relative path".into(),
        ));
    }
    Ok(())
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid: {message}"),
            Self::Conflict(message) => write!(f, "conflict: {message}"),
            Self::Limit(message) => write!(f, "limit: {message}"),
            Self::NotFound(message) => write!(f, "not found: {message}"),
            Self::Io(error) => error.fmt(f),
            Self::Json(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for CaptureError {}

impl From<std::io::Error> for CaptureError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for CaptureError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub(crate) fn now_seconds() -> Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CaptureError::Invalid("system time is before the Unix epoch".into()))?
        .as_secs();
    i64::try_from(seconds).map_err(|_| CaptureError::Limit("system time is out of range".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize_git_repository(path: &Path) {
        let run = |arguments: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(arguments)
                .status()
                .unwrap();
            assert!(status.success(), "git {arguments:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "capture@example.invalid"]);
        run(&["config", "user.name", "Capture Fixture"]);
        fs::write(path.join("tracked.txt"), "tracked\n").unwrap();
        run(&["add", "tracked.txt"]);
        run(&["commit", "-q", "-m", "fixture"]);
    }

    #[test]
    fn clean_gitlink_capture_preserves_missing_submodule_coverage() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("repository");
        fs::create_dir(&root).unwrap();
        initialize_git_repository(&root);
        let head = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(head.status.success());
        let head = String::from_utf8(head.stdout).unwrap();
        fs::create_dir(root.join("dependency")).unwrap();
        for args in [
            vec![
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                head.trim(),
                "dependency",
            ],
            vec!["commit", "-qm", "Synthetic gitlink"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let status = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(status.stdout.is_empty());
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let source = capture_git_source(
            &GitSourceRequest {
                repository: "example/repository".into(),
                worktree_id: "checkout".into(),
                repository_path: root,
                base_ref: "HEAD".into(),
            },
            &spool,
        )
        .unwrap();
        let SourceBindingV1::Git {
            dirty,
            omissions,
            snapshot_coverage,
            ..
        } = source
        else {
            panic!("expected Git source")
        };
        assert!(!dirty);
        assert!(omissions.contains(&CaptureOmissionV1::SubmoduleContent));
        assert_eq!(snapshot_coverage, CoverageV1::Partial);
    }

    #[test]
    fn unsupported_special_file_capture_preserves_missing_working_content() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("repository");
        fs::create_dir(&root).unwrap();
        initialize_git_repository(&root);
        let tracked = root.join("tracked.txt");
        fs::remove_file(&tracked).unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(&tracked)
                .status()
                .unwrap()
                .success()
        );
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let source = capture_git_source(
            &GitSourceRequest {
                repository: "example/repository".into(),
                worktree_id: "checkout".into(),
                repository_path: root,
                base_ref: "HEAD".into(),
            },
            &spool,
        )
        .unwrap();
        let SourceBindingV1::Git {
            omissions,
            snapshot_coverage,
            captured_snapshot,
            ..
        } = source
        else {
            panic!("expected Git source")
        };
        assert!(captured_snapshot.is_some());
        assert!(omissions.contains(&CaptureOmissionV1::WorkingTreeContent));
        assert_eq!(snapshot_coverage, CoverageV1::Partial);
    }

    #[test]
    fn retained_root_descriptor_controls_child_cwd_after_path_replacement() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("root");
        let moved = temp.path().join("moved-root");
        fs::create_dir(&root).unwrap();
        let opened = OpenedGitRoot::open(&root).unwrap();

        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf original > observed"])
            .env_clear()
            .process_group(0);
        opened.configure_cwd(&mut command).unwrap();
        let captured = capture_with_input(
            &mut command,
            None,
            Duration::from_secs(5),
            4096,
            &Cancellation::default(),
        )
        .unwrap();

        assert_eq!(captured.code, Some(0));
        assert_eq!(fs::read(moved.join("observed")).unwrap(), b"original");
        assert!(!root.join("observed").exists());
        opened.verify_descriptor().unwrap();
        assert!(opened.verify_path(&root).is_err());
    }

    #[test]
    fn cwd_clone_failure_happens_before_durable_action_start() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let opened = OpenedGitRoot::open(&root).unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let request = ObservedCommandRequest {
            start_operation_id: "recipe:start:cwd-fault".into(),
            finish_operation_id: "recipe:finish:cwd-fault".into(),
            work_id: "wrk_cwd_fault".into(),
            generation: 1,
            session_id: "session_cwd_fault".into(),
            action_id: "act:cwd-fault".into(),
            source: GitSourceRequest {
                repository: "example/repository".into(),
                worktree_id: "worktree_cwd_fault".into(),
                repository_path: root,
                base_ref: "main".into(),
            },
            argv: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
            setup_digest: "1".repeat(64),
            coverage: CoverageV1::Complete,
            timeout: Duration::from_secs(5),
            max_output_bytes: 4096,
        };
        FAIL_NEXT_CWD_CLONE.with(|fail| fail.set(true));
        let result = run_observed_command_inner(
            request,
            &spool,
            "2".repeat(64),
            ObservedExecution {
                kind: ActionKindV1::Verification,
                recipe_provenance: RecipeProvenanceV1::ConfiguredExactMatch,
                tee: false,
                opened_root: Some(&opened),
                prepared_cwd: None,
                admitted_start: None,
            },
            &Cancellation::default(),
            |_, _| panic!("pre-spawn callback must not run"),
        );
        assert!(matches!(result, Err(CaptureError::Io(_))));
        assert!(
            spool
                .action_start("recipe:start:cwd-fault")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ordinary_event_pre_tool_use_spawns_no_source_git() {
        let temp = tempfile::TempDir::new().unwrap();
        let repository_path = temp.path().join("repository");
        fs::create_dir(&repository_path).unwrap();
        initialize_git_repository(&repository_path);
        fs::write(repository_path.join("tracked.txt"), "dirty\n").unwrap();
        let repository_path = repository_path.canonicalize().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let capture = claude_code::CaptureRequest {
            operation_prefix: "capture:read".into(),
            work_id: "wrk_read".into(),
            generation: 1,
            observed_session_id: "observed_read".into(),
            canonical_session_id: "canonical_read".into(),
            parent_session_id: None,
            capture_mode: crate::change_record::CaptureModeV1::MetadataOnly,
            live_capture_mode: crate::change_record::CaptureModeV1::MetadataOnly,
        };
        let actions = claude_code::ActionCaptureConfig {
            source: GitSourceRequest {
                repository: "example/repository".into(),
                worktree_id: "worktree_read".into(),
                repository_path,
                base_ref: "HEAD".into(),
            },
            setup_digest: "1".repeat(64),
            rules: Vec::new(),
            managed_invocations: Vec::new(),
        };
        let pre = serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": capture.observed_session_id,
            "tool_name": "Read",
            "tool_use_id": "toolu_read",
            "tool_input": {"file_path": "tracked.txt"},
        }))
        .unwrap();
        // Every source git child runs in the opened root, so an armed fault proves none spawned.
        FAIL_NEXT_CWD_CLONE.with(|fail| fail.set(true));
        let report = claude_code::capture_native_hook(&pre, &capture, &actions, &spool);
        let armed = FAIL_NEXT_CWD_CLONE.with(|fail| fail.replace(false));
        report.unwrap();
        assert!(armed, "a Read PreToolUse must not run source git");
        let marker = spool
            .managed_invocation_for_occurrence("observed_read", "toolu_read")
            .unwrap()
            .expect("ordinary occurrence claim");
        assert!(marker.source.is_none());
    }

    #[test]
    fn reconcile_and_native_post_converge_while_a_durable_start_stays_fenced() {
        for scenario in ["reconcile-first", "post-first", "durable-start"] {
            let post_before_reconcile = scenario == "post-first";
            let temp = tempfile::TempDir::new().unwrap();
            let repository_path = temp.path().join("repository");
            fs::create_dir(&repository_path).unwrap();
            initialize_git_repository(&repository_path);
            let repository_path = repository_path.canonicalize().unwrap();
            let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
            let tool_use_id = scenario;
            let managed_invocation = format!(
                "/synthetic/agentscm change-capture run-recipe --id reconcile --occurrence {tool_use_id}"
            );
            let source = GitSourceRequest {
                repository: "example/repository".into(),
                worktree_id: "worktree_reconcile".into(),
                repository_path: repository_path.clone(),
                base_ref: "HEAD".into(),
            };
            let capture = claude_code::CaptureRequest {
                operation_prefix: "capture:reconcile".into(),
                work_id: "wrk_reconcile".into(),
                generation: 1,
                observed_session_id: "observed_reconcile".into(),
                canonical_session_id: "canonical_reconcile".into(),
                parent_session_id: None,
                capture_mode: crate::change_record::CaptureModeV1::MetadataOnly,
                live_capture_mode: crate::change_record::CaptureModeV1::MetadataOnly,
            };
            let actions = claude_code::ActionCaptureConfig {
                source: source.clone(),
                setup_digest: "1".repeat(64),
                rules: Vec::new(),
                managed_invocations: vec![managed_invocation.clone()],
            };
            let pre = serde_json::to_vec(&serde_json::json!({
                "hook_event_name": "PreToolUse",
                "session_id": capture.observed_session_id,
                "tool_name": "Bash",
                "tool_use_id": tool_use_id,
                "tool_input": {"command": managed_invocation},
            }))
            .unwrap();
            claude_code::capture_native_hook(&pre, &capture, &actions, &spool).unwrap();

            let request = ConfiguredRecipeRunRequest {
                work_id: capture.work_id.clone(),
                generation: capture.generation,
                observed_session_id: capture.observed_session_id.clone(),
                session_id: capture.canonical_session_id.clone(),
                source: source.clone(),
                setup_digest: actions.setup_digest.clone(),
                recipe: ConfiguredRecipeV1 {
                    id: "reconcile".into(),
                    argv: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        format!(
                            "printf launched > {}",
                            temp.path().join("child-must-not-run").display()
                        ),
                    ],
                    coverage: CoverageV1::Complete,
                    timeout_seconds: 10,
                    max_output_bytes: 4096,
                },
                managed_invocation: managed_invocation.clone(),
            };
            let (recipe_digest, _opened, reservation) =
                configured_recipe_binding(&request).unwrap();
            let (attempt, created) = spool.reserve_recipe_attempt(&reservation).unwrap();
            assert!(created);
            if scenario == "durable-start" {
                let marker = spool
                    .managed_invocation_by_id(&attempt.marker_id)
                    .unwrap()
                    .unwrap();
                spool
                    .admit_configured_start(
                        &attempt,
                        ActionStartV1 {
                            schema_version: SCHEMA_VERSION_V1,
                            operation_id: attempt.start_operation_id.clone(),
                            work_id: attempt.work_id.clone(),
                            generation: attempt.generation,
                            session_id: attempt.canonical_session_id.clone(),
                            action_id: attempt.action_id.clone(),
                            source: marker.source.unwrap(),
                            kind: ActionKindV1::Verification,
                            recipe_provenance: RecipeProvenanceV1::ConfiguredExactMatch,
                            recipe_digest,
                            setup_digest: attempt.setup_digest.clone(),
                            started_at: now_seconds().unwrap(),
                            observed_order: 0,
                            observation_scope_id: None,
                        },
                    )
                    .unwrap();
                assert!(matches!(
                    reconcile_retained_configured_recipe(
                        ConfiguredRecipeReconcileRequest {
                            work_id: capture.work_id.clone(),
                            source: source.clone(),
                            recipe_id: "reconcile".into(),
                        },
                        &spool,
                    )
                    .unwrap(),
                    RecipeReconcileOutcome::Unresolved { .. }
                ));
                assert!(spool.recipe_attempt(&attempt.fence_id).unwrap().is_some());
                assert!(
                    spool
                        .action_finish(&attempt.finish_operation_id)
                        .unwrap()
                        .is_none()
                );
                assert!(run_configured_recipe(request, &spool).is_err());
                assert!(!temp.path().join("child-must-not-run").exists());
                continue;
            }
            let post = serde_json::to_vec(&serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": capture.observed_session_id,
                "tool_name": "Bash",
                "tool_use_id": tool_use_id,
                "tool_input": {"command": managed_invocation},
                "tool_response": {"exit_code": 0},
            }))
            .unwrap();
            if post_before_reconcile {
                let blocked =
                    claude_code::capture_native_hook(&post, &capture, &actions, &spool).unwrap();
                assert_eq!(
                    blocked.action,
                    claude_code::NativeActionOutcome::Interrupted
                );
            }
            let reconciled = reconcile_retained_configured_recipe(
                ConfiguredRecipeReconcileRequest {
                    work_id: capture.work_id.clone(),
                    source: source.clone(),
                    recipe_id: "reconcile".into(),
                },
                &spool,
            )
            .unwrap();
            let RecipeReconcileOutcome::ClearedPreSpawn { action_id } = reconciled else {
                panic!("reserved attempt without a start must clear before spawn")
            };
            assert_eq!(action_id, attempt.action_id);
            assert!(spool.recipe_attempt(&attempt.fence_id).unwrap().is_none());

            let resumed =
                claude_code::capture_native_hook(&post, &capture, &actions, &spool).unwrap();
            assert!(matches!(
                resumed.action,
                claude_code::NativeActionOutcome::Finished(AppendOutcome::Stored)
                    | claude_code::NativeActionOutcome::Finished(AppendOutcome::Duplicate)
            ));
            assert!(
                spool
                    .managed_invocation_by_id(&attempt.marker_id)
                    .unwrap()
                    .is_none()
            );
            assert!(
                !temp.path().join("child-must-not-run").exists(),
                "reconciliation and Post must never launch the configured child"
            );
        }
    }
}
