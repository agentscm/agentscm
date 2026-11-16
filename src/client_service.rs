//! The existing native executable's optional per-user resident lifecycle; no listener or model.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agentscm::lease::Lease;
use agentscm::{Error, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::quickstart::manifest::{acquire_lease, sha_hex};

const SCHEMA: u8 = 1;
const RECORD_LIMIT: u64 = 32 * 1024;
const START_WAIT: Duration = Duration::from_secs(10);
const CONTROL_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Subcommand)]
pub(crate) enum ServiceCommand {
    /// Enable and start this installed native engine's resident.
    Start,
    /// Stop the resident and keep it stopped across setup, updates and login.
    Stop,
    /// Print resident lifecycle and version coverage as JSON.
    Status,
}

#[derive(Debug, Serialize)]
pub(crate) struct Status {
    pub(crate) managed_startup_supported: bool,
    pub(crate) configured: bool,
    pub(crate) desired_running: Option<bool>,
    pub(crate) running: bool,
    pub(crate) invoked_version: &'static str,
    pub(crate) installed_version: Option<String>,
    pub(crate) running_version: Option<String>,
    pub(crate) update_pending: bool,
    pub(crate) recovery_pending: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct SetupOutcome {
    pub(crate) managed_startup_supported: bool,
    pub(crate) changed: bool,
    pub(crate) running: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: u8,
    generation: String,
    executable: PathBuf,
    executable_sha256: String,
    version: String,
    plist: PathBuf,
    plist_sha256: String,
    label: String,
    installed: bool,
    desired_running: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    schema: u8,
    previous: Option<Receipt>,
    candidate: Receipt,
    previous_loaded: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Running {
    schema: u8,
    generation: String,
    executable: PathBuf,
    version: String,
    pid: u32,
}

struct Paths {
    home: PathBuf,
    config: PathBuf,
    dir: PathBuf,
    plist: PathBuf,
    label: String,
}

impl Paths {
    fn new() -> Result<Self> {
        let config = crate::hub_cli::config_dir()?;
        let home = PathBuf::from(
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .ok_or_else(|| invalid("home directory is not set"))?,
        );
        if !normalized(&config) || !normalized(&home) {
            return Err(invalid("resident paths must be absolute and normalized"));
        }
        let label = format!(
            "com.counterbranch.agentscm.client.{}",
            &sha_hex(config.as_os_str().as_encoded_bytes())[..16]
        );
        Ok(Self {
            dir: config.join("agentscm/client-service"),
            plist: home
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist")),
            config,
            home,
            label,
        })
    }

    fn receipt(&self) -> PathBuf {
        self.dir.join("receipt.json")
    }
    fn pending(&self) -> PathBuf {
        self.dir.join("pending.json")
    }
    fn running(&self) -> PathBuf {
        self.dir.join("running.json")
    }
    fn lock(&self) -> PathBuf {
        self.dir.join("run.lock")
    }

    fn ensure(&self) -> Result<()> {
        directory(&self.config.join("agentscm"), false)?;
        directory(&self.dir, true)
    }
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

fn normalized(path: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
}

fn directory(path: &Path, private: bool) -> Result<()> {
    #[cfg(not(unix))]
    let _ = private;
    if !path.try_exists()? {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid(
            "resident directory is not an owned regular directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & if private { 0o077 } else { 0o022 } != 0
        {
            return Err(invalid("resident directory permissions or owner changed"));
        }
    }
    Ok(())
}

fn private_file(path: &Path, create: bool) -> Result<Option<File>> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > RECORD_LIMIT {
        return Err(invalid("resident record is not a bounded regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(invalid("resident record permissions or ownership changed"));
        }
    }
    Ok(Some(file))
}

fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    let Some(file) = private_file(path, false)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > RECORD_LIMIT {
        return Err(invalid("resident record exceeds its limit"));
    }
    serde_json::from_slice(&bytes).map(Some).map_err(|_| {
        invalid(
            "resident record has an unknown schema or is corrupt; recovery evidence was retained",
        )
    })
}

fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_string(value)?;
    if bytes.len() as u64 > RECORD_LIMIT {
        return Err(invalid("resident record exceeds its limit"));
    }
    crate::hub_cli::write_private(path, &bytes)
}

fn receipt(paths: &Paths) -> Result<Option<Receipt>> {
    let saved: Option<Receipt> = read(&paths.receipt())?;
    if let Some(saved) = &saved {
        validate(paths, saved)?;
    }
    Ok(saved)
}

fn validate(paths: &Paths, saved: &Receipt) -> Result<()> {
    if saved.schema != SCHEMA
        || saved.label != paths.label
        || saved.plist != paths.plist
        || !normalized(&saved.executable)
        || !hex(&saved.generation, 32)
        || !hex(&saved.executable_sha256, 64)
        || !hex(&saved.plist_sha256, 64)
        || saved.version.is_empty()
        || saved.version.len() > 128
        || saved.version.chars().any(char::is_control)
        || sha_hex(plist(paths, saved).as_bytes()) != saved.plist_sha256
    {
        return Err(invalid(
            "resident receipt identity or schema changed; no service was altered",
        ));
    }
    Ok(())
}

fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn executable_hash(path: &Path) -> Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > 128 * 1024 * 1024
    {
        return Err(invalid(
            "resident executable is not a bounded immutable regular file",
        ));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn check_files(paths: &Paths, saved: &Receipt) -> Result<()> {
    validate(paths, saved)?;
    if executable_hash(&saved.executable)? != saved.executable_sha256 {
        return Err(invalid(
            "resident executable changed; recovery receipt was retained",
        ));
    }
    check_plist(paths, saved)
}

fn check_plist(paths: &Paths, saved: &Receipt) -> Result<()> {
    let bytes = private_file(&paths.plist, false)?.ok_or_else(|| {
        invalid("resident service file is missing; recovery receipt was retained")
    })?;
    let mut text = String::new();
    bytes.take(RECORD_LIMIT + 1).read_to_string(&mut text)?;
    if sha_hex(text.as_bytes()) != saved.plist_sha256 {
        return Err(invalid(
            "resident service file changed; it was not replaced or removed",
        ));
    }
    Ok(())
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn plist(paths: &Paths, saved: &Receipt) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>--no-telemetry</string><string>client</string><string>run</string></array>\n<key>EnvironmentVariables</key><dict><key>XDG_CONFIG_HOME</key><string>{}</string><key>HOME</key><string>{}</string></dict>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n<key>ThrottleInterval</key><integer>10</integer>\n<key>ProcessType</key><string>Background</string>\n<key>StandardOutPath</key><string>/dev/null</string>\n<key>StandardErrorPath</key><string>/dev/null</string>\n</dict></plist>\n",
        xml(&saved.label),
        xml(&saved.executable.display().to_string()),
        xml(&paths.config.display().to_string()),
        xml(&paths.home.display().to_string())
    )
}

fn manager(args: &[&str]) -> Result<crate::quickstart::process::Output> {
    crate::quickstart::process::run(Command::new("launchctl").args(args), CONTROL_WAIT)
        .map_err(|_| invalid("resident service manager could not be run"))
}

fn target(paths: &Paths) -> Result<String> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        Ok(format!(
            "gui/{}/{}",
            unsafe { libc::geteuid() },
            paths.label
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = paths;
        Err(invalid(
            "managed resident startup is supported only on macOS",
        ))
    }
}

fn loaded(paths: &Paths, allowed: &[&Receipt]) -> Result<bool> {
    let output = manager(&["print", &target(paths)?])?;
    if output.timed_out || output.truncated || output.incomplete || !output.stdout_valid_utf8 {
        return Err(invalid(
            "resident service manager status is incomplete; no service was altered",
        ));
    }
    if !output.success {
        if output.code == Some(113)
            && output
                .stderr
                .contains(&format!("Could not find service \"{}\"", paths.label))
        {
            return Ok(false);
        }
        return Err(invalid(
            "resident service manager status is unknown; recovery receipt was retained",
        ));
    }
    let line = |key: &str| {
        output
            .stdout
            .lines()
            .find_map(|line| line.trim().strip_prefix(key))
    };
    if line("path = ") != Some(paths.plist.to_string_lossy().as_ref())
        || !allowed
            .iter()
            .any(|saved| line("program = ") == Some(saved.executable.to_string_lossy().as_ref()))
    {
        return Err(invalid(
            "loaded resident identity differs from its receipt; no process was stopped",
        ));
    }
    Ok(true)
}

fn stop_loaded(paths: &Paths, allowed: &[&Receipt]) -> Result<()> {
    if loaded(paths, allowed)? {
        let _ = manager(&["bootout", &target(paths)?])?;
        if loaded(paths, allowed)? {
            return Err(invalid(
                "resident did not stop; recovery receipt was retained",
            ));
        }
    }
    Ok(())
}

fn start_loaded(paths: &Paths) -> Result<()> {
    let domain = target(paths)?
        .rsplit_once('/')
        .ok_or_else(|| invalid("bad resident service target"))?
        .0
        .to_owned();
    if !manager(&["bootstrap", &domain, &paths.plist.display().to_string()])?.success {
        return Err(invalid(
            "resident startup failed; recovery receipt was retained",
        ));
    }
    Ok(())
}

fn running(paths: &Paths) -> Result<Option<Running>> {
    let Some(lock) = private_file(&paths.lock(), false)? else {
        return Ok(None);
    };
    match Lease::try_shared(lock) {
        Ok(_) => Ok(None),
        Err(TryLockError::WouldBlock) => {
            let Some(saved): Option<Running> = read(&paths.running())? else {
                return Ok(None);
            };
            if saved.schema != SCHEMA || saved.pid == 0 || !hex(&saved.generation, 32) {
                return Err(invalid("resident running record is unknown"));
            }
            Ok(Some(saved))
        }
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

async fn healthy(paths: &Paths, saved: &Receipt) -> Result<()> {
    let deadline = tokio::time::Instant::now() + START_WAIT;
    while tokio::time::Instant::now() < deadline {
        if running(paths)?.is_some_and(|run| {
            run.generation == saved.generation
                && run.executable == saved.executable
                && run.version == saved.version
        }) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(invalid(
        "resident did not report the candidate generation/version in time",
    ))
}

/// Finish an interrupted handoff conservatively before another setup/stop/uninstall mutation.
async fn recover(paths: &Paths) -> Result<()> {
    let Some(pending) = read::<Pending>(&paths.pending())? else {
        return Ok(());
    };
    if pending.schema != SCHEMA {
        return Err(invalid(
            "resident handoff schema is unknown; evidence retained",
        ));
    }
    validate(paths, &pending.candidate)?;
    if let Some(previous) = &pending.previous {
        validate(paths, previous)?;
    }
    let current = receipt(paths)?;
    if current.as_ref() != Some(&pending.candidate) && current != pending.previous {
        return Err(invalid(
            "resident handoff receipt changed; recovery requires inspection",
        ));
    }
    match private_file(&paths.plist, false)? {
        Some(file) => {
            let mut text = Vec::new();
            file.take(RECORD_LIMIT + 1).read_to_end(&mut text)?;
            if sha_hex(&text) != pending.candidate.plist_sha256
                && pending
                    .previous
                    .as_ref()
                    .is_none_or(|old| sha_hex(&text) != old.plist_sha256)
            {
                return Err(invalid(
                    "resident handoff service file changed; evidence retained",
                ));
            }
        }
        None if pending.previous.as_ref().is_some_and(|old| old.installed) => {
            return Err(invalid(
                "previous resident service file disappeared during handoff; evidence retained",
            ));
        }
        None => {}
    }
    let mut allowed = vec![&pending.candidate];
    if let Some(previous) = &pending.previous {
        allowed.push(previous);
    }
    for saved in &allowed {
        if saved.installed && executable_hash(&saved.executable)? != saved.executable_sha256 {
            return Err(invalid(
                "resident executable changed during handoff; no process was stopped",
            ));
        }
    }
    stop_loaded(paths, &allowed)?;
    if let Some(previous) = &pending.previous {
        if previous.installed {
            if executable_hash(&previous.executable)? != previous.executable_sha256 {
                return Err(invalid(
                    "previous resident executable changed; rollback evidence retained",
                ));
            }
            crate::hub_cli::write_private(&paths.plist, &plist(paths, previous))?;
        } else if paths.plist.try_exists()? {
            std::fs::remove_file(&paths.plist)?;
        }
        write(&paths.receipt(), previous)?;
        if previous.installed && pending.previous_loaded && previous.desired_running {
            start_loaded(paths)?;
            healthy(paths, previous).await?;
        }
    } else {
        if paths.plist.try_exists()? {
            std::fs::remove_file(&paths.plist)?;
        }
        if paths.receipt().try_exists()? {
            std::fs::remove_file(paths.receipt())?;
        }
    }
    std::fs::remove_file(paths.pending())?;
    Ok(())
}

/// Called under quickstart's existing installer lease. Unsupported hosts keep foreground coverage.
pub(crate) async fn setup(program: &Path) -> Result<SetupOutcome> {
    if !cfg!(target_os = "macos") {
        return Ok(SetupOutcome {
            managed_startup_supported: false,
            changed: false,
            running: false,
        });
    }
    setup_inner(program, None).await
}

/// Existing stopped/recovery records also require an explicit installer handoff, never recreation.
pub(crate) fn has_receipt() -> Result<bool> {
    Ok(receipt(&Paths::new()?)?.is_some())
}

/// The marker selects the existing installer flow; it confers no new process or Hub authority.
pub(crate) fn auto_setup_requested() -> Result<bool> {
    let paths = Paths::new()?;
    let existing = receipt(&paths)?.is_some();
    let installer = std::env::var("AGENTSCM_VERIFIED_INSTALL").as_deref() == Ok("1");
    if installer {
        let selected = crate::quickstart::binary::bin_dir(&paths.home).join(format!(
            "agentscm-{}{}",
            env!("AGENTSCM_INSTALL_VERSION"),
            std::env::consts::EXE_SUFFIX
        ));
        if selected.canonicalize().ok() != std::env::current_exe()?.canonicalize().ok() {
            return Err(invalid(
                "installer invocation does not match its selected versioned native executable; no resident handoff was attempted",
            ));
        }
    }
    Ok(existing || installer)
}

/// Existing setup helpers may replace dev builds, but never an executable a receipt pins.
pub(crate) fn protects_executable(path: &Path) -> Result<bool> {
    Ok(receipt(&Paths::new()?)?.is_some_and(|saved| {
        saved.installed && path.canonicalize().ok().as_deref() == Some(saved.executable.as_path())
    }))
}

async fn setup_inner(program: &Path, desired: Option<bool>) -> Result<SetupOutcome> {
    let paths = Paths::new()?;
    paths.ensure()?;
    recover(&paths).await?;
    let previous = receipt(&paths)?;
    if previous.as_ref().is_some_and(|saved| !saved.installed) && paths.plist.try_exists()? {
        let saved = previous
            .as_ref()
            .ok_or_else(|| invalid("resident receipt missing"))?;
        check_plist(&paths, saved)?;
        if loaded(&paths, &[saved])? {
            return Err(invalid(
                "an uninstalled resident is still loaded; recovery evidence retained",
            ));
        }
        std::fs::remove_file(&paths.plist)?;
    }
    let was_loaded = if let Some(previous) = &previous {
        if previous.installed {
            check_files(&paths, previous)?;
        }
        loaded(&paths, &[previous])?
    } else {
        if paths.plist.try_exists()? {
            return Err(invalid(
                "an unrecorded resident service file exists; it was not replaced",
            ));
        }
        // Even a missing plist cannot authorize replacing an unknown loaded service.
        loaded(&paths, &[])?
    };
    if previous.as_ref().is_some_and(|saved| !saved.installed) && was_loaded {
        return Err(invalid(
            "an uninstalled resident is unexpectedly loaded; no service was altered",
        ));
    }
    if !was_loaded && running(&paths)?.is_some() {
        return Err(invalid(
            "a foreground resident is still running; stop it before changing managed startup",
        ));
    }
    let source = program.canonicalize()?;
    let bin_dir = crate::quickstart::binary::bin_dir(&paths.home);
    let versioned = bin_dir.join(format!(
        "agentscm-{}{}",
        env!("AGENTSCM_INSTALL_VERSION"),
        std::env::consts::EXE_SUFFIX
    ));
    if versioned.try_exists()? && executable_hash(&versioned)? != executable_hash(&source)? {
        return Err(invalid(
            "the selected resident version already has different bytes; retain a distinct immutable version before setup",
        ));
    }
    crate::quickstart::binary::install(&bin_dir, &source, env!("AGENTSCM_INSTALL_VERSION"))
        .map_err(|why| {
            Error::Invalid(format!("could not retain the resident executable: {why}"))
        })?;
    let executable = versioned.canonicalize()?;
    let executable_sha256 = executable_hash(&executable)?;
    let desired_running =
        desired.unwrap_or_else(|| previous.as_ref().is_none_or(|saved| saved.desired_running));
    if let Some(previous) = &previous
        && previous.executable == executable
        && previous.executable_sha256 == executable_sha256
        && previous.installed
        && previous.desired_running == desired_running
        && (was_loaded || !desired_running)
    {
        if desired_running {
            healthy(&paths, previous).await?;
        } else if was_loaded {
            stop_loaded(&paths, &[previous])?;
        }
        return Ok(SetupOutcome {
            managed_startup_supported: true,
            changed: was_loaded && !desired_running,
            running: desired_running,
        });
    }
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| invalid("resident generation random source unavailable"))?;
    let mut candidate = Receipt {
        schema: SCHEMA,
        generation: random.iter().map(|byte| format!("{byte:02x}")).collect(),
        executable,
        executable_sha256,
        version: env!("AGENTSCM_INSTALL_VERSION").into(),
        plist: paths.plist.clone(),
        plist_sha256: String::new(),
        label: paths.label.clone(),
        installed: true,
        desired_running,
    };
    candidate.plist_sha256 = sha_hex(plist(&paths, &candidate).as_bytes());
    directory(
        paths
            .plist
            .parent()
            .ok_or_else(|| invalid("resident service parent missing"))?,
        false,
    )?;
    let pending = Pending {
        schema: SCHEMA,
        previous: previous.clone(),
        candidate: candidate.clone(),
        previous_loaded: was_loaded,
    };
    write(&paths.pending(), &pending)?;
    let install = async {
        if let Some(previous) = &previous {
            stop_loaded(&paths, &[previous])?;
        }
        crate::hub_cli::write_private(&paths.plist, &plist(&paths, &candidate))?;
        write(&paths.receipt(), &candidate)?;
        if desired_running {
            start_loaded(&paths)?;
            healthy(&paths, &candidate).await?;
        }
        Ok::<(), Error>(())
    }
    .await;
    if let Err(cause) = install {
        return match recover(&paths).await {
            Ok(()) => Err(Error::Invalid(format!(
                "{cause}; previous resident state restored"
            ))),
            Err(recovery) => Err(Error::Invalid(format!(
                "{cause}; resident rollback incomplete: {recovery}"
            ))),
        };
    }
    std::fs::remove_file(paths.pending())?;
    Ok(SetupOutcome {
        managed_startup_supported: true,
        changed: true,
        running: desired_running,
    })
}

/// Read-only lifecycle status; an unknown/corrupt record is never silently replaced.
pub(crate) fn status() -> Result<Status> {
    let paths = Paths::new()?;
    let saved = receipt(&paths)?;
    let running = running(&paths)?;
    let installed_version = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| std::fs::read_link(home.join(".agentscm/bin/agentscm")).ok())
        .and_then(|target| {
            target
                .file_name()?
                .to_str()?
                .strip_prefix("agentscm-")
                .map(str::to_owned)
        })
        .filter(|version| {
            !version.is_empty() && version.len() <= 128 && !version.chars().any(char::is_control)
        });
    Ok(Status {
        managed_startup_supported: cfg!(target_os = "macos"),
        configured: saved.as_ref().is_some_and(|saved| saved.installed),
        desired_running: saved.as_ref().map(|saved| saved.desired_running),
        running: running.is_some(),
        invoked_version: env!("AGENTSCM_INSTALL_VERSION"),
        update_pending: running.as_ref().is_some_and(|run| {
            installed_version
                .as_ref()
                .is_some_and(|installed| run.version != *installed)
        }),
        installed_version,
        running_version: running.map(|run| run.version),
        recovery_pending: paths.pending().try_exists()?,
    })
}

pub(crate) async fn run(command: ServiceCommand) -> Result<()> {
    if matches!(command, ServiceCommand::Status) {
        println!("{}", serde_json::to_string(&status()?)?);
        return Ok(());
    }
    if !cfg!(target_os = "macos") {
        return Err(invalid(
            "managed resident startup is supported only on macOS; on Unix run `agentscm client run` in the foreground",
        ));
    }
    let _installer = acquire_lease()?;
    match command {
        ServiceCommand::Start => {
            setup_inner(&std::env::current_exe()?, Some(true)).await?;
        }
        ServiceCommand::Stop => {
            let paths = Paths::new()?;
            recover(&paths).await?;
            if let Some(mut saved) = receipt(&paths)? {
                if saved.installed {
                    check_files(&paths, &saved)?;
                }
                // Desired state precedes stopping so a restart/login cannot undo an explicit stop.
                saved.desired_running = false;
                write(&paths.receipt(), &saved)?;
                stop_loaded(&paths, &[&saved])?;
            }
        }
        ServiceCommand::Status => unreachable!(),
    }
    println!("{}", serde_json::to_string(&status()?)?);
    Ok(())
}

/// Caller holds quickstart's lease; retain the stopped preference and all journal/data records.
pub(crate) async fn uninstall() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let paths = Paths::new()?;
    recover(&paths).await?;
    let Some(mut saved) = receipt(&paths)? else {
        return Ok(());
    };
    if !saved.installed {
        if paths.plist.try_exists()? {
            check_plist(&paths, &saved)?;
            if loaded(&paths, &[&saved])? {
                return Err(invalid(
                    "an uninstalled resident is still loaded; recovery evidence retained",
                ));
            }
            std::fs::remove_file(&paths.plist)?;
        }
        return Ok(());
    }
    check_files(&paths, &saved)?;
    saved.desired_running = false;
    write(&paths.receipt(), &saved)?;
    stop_loaded(&paths, &[&saved])?;
    saved.installed = false;
    write(&paths.receipt(), &saved)?;
    std::fs::remove_file(&paths.plist)?;
    // The sidecar remains as an explicit stopped choice, independent of legacy manifest writers.
    Ok(())
}

pub(crate) struct RunGuard {
    paths: Paths,
    generation: String,
    configured: bool,
    _lease: Lease,
}

impl RunGuard {
    /// A stale/disabled generation may not continue remote work after an update or stop.
    pub(crate) fn is_current(&self) -> Result<bool> {
        Ok(match receipt(&self.paths)? {
            Some(saved) => saved.desired_running && saved.generation == self.generation,
            None => !self.configured,
        })
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if read::<Running>(&self.paths.running())
            .ok()
            .flatten()
            .is_some_and(|saved| {
                saved.generation == self.generation && saved.pid == std::process::id()
            })
        {
            let _ = std::fs::remove_file(self.paths.running());
        }
    }
}

/// The explicit `client run` entry holds this guard for its whole deterministic loop.
pub(crate) fn begin_run() -> Result<Option<RunGuard>> {
    if !cfg!(unix) {
        return Err(invalid(
            "the resident needs a Unix host; Windows retains next-call delivery",
        ));
    }
    begin_run_at(Paths::new()?, &std::env::current_exe()?)
}

fn begin_run_at(paths: Paths, program: &Path) -> Result<Option<RunGuard>> {
    paths.ensure()?;
    let saved = receipt(&paths)?;
    if saved.as_ref().is_some_and(|saved| !saved.desired_running) {
        return Ok(None);
    }
    let executable = program.canonicalize()?;
    if let Some(saved) = &saved
        && saved.executable != executable
    {
        return Err(invalid(
            "resident executable differs from the installed generation; run setup to finish the handoff",
        ));
    }
    if let Some(saved) = &saved
        && saved.installed
    {
        check_files(&paths, saved)?;
    }
    let lock =
        private_file(&paths.lock(), true)?.ok_or_else(|| invalid("resident lock unavailable"))?;
    let lease = Lease::try_exclusive(lock).map_err(|error| match error {
        TryLockError::WouldBlock => {
            invalid("another resident already owns this computer's journal")
        }
        TryLockError::Error(error) => error.into(),
    })?;
    let generation = saved.as_ref().map_or_else(
        || {
            sha_hex(
                format!(
                    "{}:{}",
                    std::process::id(),
                    agentscm::hub::store::unix_now()
                )
                .as_bytes(),
            )[..32]
                .to_owned()
        },
        |saved| saved.generation.clone(),
    );
    write(
        &paths.running(),
        &Running {
            schema: SCHEMA,
            generation: generation.clone(),
            executable,
            version: env!("AGENTSCM_INSTALL_VERSION").into(),
            pid: std::process::id(),
        },
    )?;
    Ok(Some(RunGuard {
        paths,
        generation,
        configured: saved.is_some(),
        _lease: lease,
    }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture {
        _root: tempfile::TempDir,
        paths: Paths,
        executable: PathBuf,
        saved: Receipt,
    }

    impl Fixture {
        fn new(desired_running: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let config = root.path().join("config");
            let paths = Paths {
                home: root.path().to_owned(),
                dir: config.join("agentscm/client-service"),
                plist: root.path().join("Library/LaunchAgents/test.plist"),
                label: "com.counterbranch.agentscm.client.fixture".into(),
                config,
            };
            paths.ensure().unwrap();
            directory(paths.plist.parent().unwrap(), false).unwrap();
            let executable = root.path().join("native-fixture");
            std::fs::write(&executable, b"synthetic native artifact").unwrap();
            let executable = executable.canonicalize().unwrap();
            let mut saved = Receipt {
                schema: SCHEMA,
                generation: "a".repeat(32),
                executable: executable.clone(),
                executable_sha256: executable_hash(&executable).unwrap(),
                version: "fixture-old".into(),
                plist: paths.plist.clone(),
                plist_sha256: String::new(),
                label: paths.label.clone(),
                installed: true,
                desired_running,
            };
            saved.plist_sha256 = sha_hex(plist(&paths, &saved).as_bytes());
            crate::hub_cli::write_private(&paths.plist, &plist(&paths, &saved)).unwrap();
            write(&paths.receipt(), &saved).unwrap();
            Self {
                _root: root,
                paths,
                executable,
                saved,
            }
        }
    }

    #[test]
    fn stopped_preference_prevents_launch_without_touching_its_receipt() {
        let fixture = Fixture::new(false);
        let before = std::fs::read(fixture.paths.receipt()).unwrap();
        let receipt = fixture.paths.receipt();
        assert!(
            begin_run_at(fixture.paths, &fixture.executable)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(receipt).unwrap(), before);
    }

    #[test]
    fn resident_lock_is_exclusive_and_stopped_or_replaced_generation_ends_eligibility() {
        let fixture = Fixture::new(true);
        let guard = begin_run_at(fixture.paths, &fixture.executable)
            .unwrap()
            .unwrap();
        assert!(guard.is_current().unwrap());
        assert!(matches!(
            Lease::try_exclusive(private_file(&guard.paths.lock(), false).unwrap().unwrap()),
            Err(TryLockError::WouldBlock)
        ));
        let mut saved = fixture.saved;
        saved.desired_running = false;
        write(&guard.paths.receipt(), &saved).unwrap();
        assert!(!guard.is_current().unwrap());
        saved.desired_running = true;
        saved.generation = "b".repeat(32);
        write(&guard.paths.receipt(), &saved).unwrap();
        assert!(!guard.is_current().unwrap());
        let record = guard.paths.running();
        drop(guard);
        assert!(!record.exists());
    }

    #[test]
    fn executable_and_service_file_drift_are_refused_without_deleting_receipts() {
        let fixture = Fixture::new(true);
        std::fs::write(&fixture.executable, b"changed").unwrap();
        assert!(check_files(&fixture.paths, &fixture.saved).is_err());
        assert!(fixture.paths.receipt().exists());
        crate::hub_cli::write_private(&fixture.paths.plist, "changed service").unwrap();
        assert!(check_plist(&fixture.paths, &fixture.saved).is_err());
        assert!(fixture.paths.plist.exists());
    }

    #[test]
    fn unknown_schema_and_symlinked_or_nonprivate_records_are_preserved_and_refused() {
        let fixture = Fixture::new(false);
        let mut value = serde_json::to_value(&fixture.saved).unwrap();
        value["schema"] = serde_json::json!(2);
        write(&fixture.paths.receipt(), &value).unwrap();
        assert!(receipt(&fixture.paths).is_err());
        assert!(fixture.paths.receipt().exists());
        std::fs::remove_file(fixture.paths.receipt()).unwrap();
        symlink(&fixture.executable, fixture.paths.receipt()).unwrap();
        assert!(receipt(&fixture.paths).is_err());
        assert_eq!(
            std::fs::read(&fixture.executable).unwrap(),
            b"synthetic native artifact"
        );
        std::fs::remove_file(fixture.paths.receipt()).unwrap();
        write(&fixture.paths.receipt(), &fixture.saved).unwrap();
        std::fs::set_permissions(
            fixture.paths.receipt(),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(receipt(&fixture.paths).is_err());
    }

    #[test]
    fn plist_pins_only_native_mode_and_exact_config_without_a_listener_or_secret() {
        let fixture = Fixture::new(true);
        let body = plist(&fixture.paths, &fixture.saved);
        assert!(body.contains("<string>client</string><string>run</string>"));
        assert!(body.contains("<key>SuccessfulExit</key><false/>"));
        assert!(body.contains("<key>XDG_CONFIG_HOME</key>"));
        for forbidden in [
            "--front-door",
            "--resume",
            "token",
            "hub</string>",
            "socket",
            "AWS_",
            "PATH</key>",
        ] {
            assert!(!body.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn ordinary_binary_setup_cannot_overwrite_a_dev_executable_pinned_by_a_receipt() {
        crate::client_config::in_isolated_config_home(|config| {
            let paths = Paths::new().unwrap();
            paths.ensure().unwrap();
            let bin = config.join("test-bin");
            directory(&bin, true).unwrap();
            let version = "0.0.0+fixture";
            let executable = bin.join(format!("agentscm-{version}"));
            std::fs::write(&executable, b"previous resident").unwrap();
            let executable = executable.canonicalize().unwrap();
            let mut saved = Receipt {
                schema: SCHEMA,
                generation: "c".repeat(32),
                executable: executable.clone(),
                executable_sha256: executable_hash(&executable).unwrap(),
                version: version.into(),
                plist: paths.plist.clone(),
                plist_sha256: String::new(),
                label: paths.label.clone(),
                installed: true,
                desired_running: false,
            };
            saved.plist_sha256 = sha_hex(plist(&paths, &saved).as_bytes());
            write(&paths.receipt(), &saved).unwrap();
            let replacement = config.join("replacement");
            std::fs::write(&replacement, b"different resident").unwrap();
            assert!(crate::quickstart::binary::install(&bin, &replacement, version).is_err());
            assert_eq!(std::fs::read(executable).unwrap(), b"previous resident");
        });
    }
}
