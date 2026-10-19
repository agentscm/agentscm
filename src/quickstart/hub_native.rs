//! A per-user macOS Hub managed by launchd.
use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs::File;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::hub_setup::{self, Runtime, Started};
use super::manifest::{Manifest, sha_hex};
use super::process::{diagnostic, run};
use crate::prompt::Ui;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Serialize, Deserialize)]
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
}

#[derive(Clone, Debug)]
pub(crate) struct Options {
    pub(crate) executable: PathBuf,
    pub(crate) port: u16,
    pub(crate) project: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    project: String,
    environment: BTreeMap<String, String>,
}

fn service_label(home: &Path, project: &str) -> String {
    let home_id = &sha_hex(home.as_os_str().as_encoded_bytes())[..10];
    format!("com.counterbranch.agentscm.{project}.{home_id}")
}

fn receipt_path(config_dir: &Path) -> PathBuf {
    config_dir.join("retained.json")
}

fn environment_ok(environment: &BTreeMap<String, String>) -> bool {
    const KEYS: [&str; 6] = [
        "AGENTSCM_DATABASE_URL",
        "AGENTSCM_GITHUB_APP_NAME",
        "AGENTSCM_GITHUB_APP_ORG",
        "AGENTSCM_GITHUB_CLIENT_ID",
        "AGENTSCM_GITHUB_CLIENT_SECRET_FILE",
        "AGENTSCM_ADMINS",
    ];
    environment.iter().all(|(key, value)| {
        KEYS.contains(&key.as_str()) && !value.is_empty() && !value.chars().any(char::is_control)
    })
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
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(
                "The retained native Hub receipt is not private (expected mode 0600).".into(),
            );
        }
    }
    let receipt: Receipt =
        serde_json::from_slice(&std::fs::read(&path).map_err(|error| error.to_string())?)
            .map_err(|_| "The retained native Hub receipt is invalid.".to_owned())?;
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
    Ok(receipt)
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

fn atomic_private(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path.parent().ok_or("service path has no parent")?;
    super::private_dir(parent).map_err(|error| error.to_string())?;
    let temp = parent.join(format!(".agentscm-service-{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    crate::hub_cli::write_private(&temp, contents).map_err(|error| error.to_string())?;
    std::fs::rename(&temp, path).map_err(|error| error.to_string())
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

#[cfg(unix)]
struct InstallLock {
    _file: File,
}

#[cfg(unix)]
fn install_lock(config_dir: &Path) -> Result<InstallLock, String> {
    super::private_dir(config_dir).map_err(|error| error.to_string())?;
    let path = config_dir.join("install.lock");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    // SAFETY: flock only observes the valid descriptor owned by `file`; the descriptor stays
    // alive in InstallLock until the operation finishes.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Another native Hub install or uninstall is already running.".into());
    }
    Ok(InstallLock { _file: file })
}

#[cfg(not(unix))]
struct InstallLock;

#[cfg(not(unix))]
fn install_lock(_config_dir: &Path) -> Result<InstallLock, String> {
    Err("native Hub service locking is available only on Unix".into())
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
        let previous_plist =
            std::fs::read_to_string(&self.record.plist).map_err(|error| error.to_string())?;
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
        atomic_private(&next_record.plist, &text)?;
        self.manifest.native_hub = Some(next_record.clone());
        if let Err(error) = self.manifest.save() {
            self.manifest.native_hub = Some(previous_record);
            atomic_private(&self.record.plist, &previous_plist).map_err(|rollback| {
                format!("{error}; restoring the previous plist failed: {rollback}")
            })?;
            return Err(error.to_string());
        }
        let target = format!("{}/{}", self.domain, self.record.label);
        let mut previous_stopped = false;
        let replacement = async {
            launchctl(&["bootout", &target])?;
            previous_stopped = true;
            launchctl(&[
                "bootstrap",
                &self.domain,
                &self.record.plist.display().to_string(),
            ])?;
            super::wait_healthy(&self.url, Duration::from_secs(60))
                .await
                .then_some(())
                .ok_or_else(|| "The configured native Hub did not become healthy.".to_owned())
        }
        .await;
        if let Err(error) = replacement {
            // The service identity remains ours throughout. Restore disk and record before
            // starting the previous configuration, and retain explicit recovery evidence on error.
            if previous_stopped {
                let _ = launchctl(&["bootout", &target]);
            }
            atomic_private(&self.record.plist, &previous_plist).map_err(|rollback| {
                format!("{error}; restoring the previous plist failed: {rollback}")
            })?;
            self.manifest.native_hub = Some(previous_record.clone());
            self.manifest.save().map_err(|rollback| {
                format!("{error}; restoring the previous record failed: {rollback}")
            })?;
            if previous_stopped {
                launchctl(&[
                    "bootstrap",
                    &self.domain,
                    &self.record.plist.display().to_string(),
                ])
                .map_err(|rollback| {
                    format!("{error}; restarting the previous Hub failed: {rollback}")
                })?;
            }
            if !super::wait_healthy(&self.url, Duration::from_secs(60)).await {
                return Err(format!(
                    "{error}; the previous Hub configuration was restored but is not healthy."
                ));
            }
            return Err(format!(
                "{error} The previous Hub configuration was restored."
            ));
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
    let existing = manifest.native_hub.clone();
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
    };
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
    } else if record.plist.exists() {
        return Err("A native Hub LaunchAgent already exists without a quickstart ownership record; refusing to replace it.".into());
    } else if record.config_dir.exists() {
        record.environment = load_receipt(&record.config_dir, &options.project)?.environment;
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
    let _install_lock = install_lock(&record.config_dir)?;
    super::private_dir(&record.data_dir).map_err(|error| error.to_string())?;
    let environment = if record.environment.is_empty() {
        base_environment(&record)
    } else {
        record.environment.clone()
    };
    record.environment = environment.clone();
    let receipt = Receipt {
        project: record.project.clone(),
        environment: environment.clone(),
    };
    if !receipt_path(&record.config_dir).exists() {
        let receipt = serde_json::to_string_pretty(&receipt)
            .map_err(|_| "cannot encode native Hub ownership receipt")?;
        atomic_private(&receipt_path(&record.config_dir), &format!("{receipt}\n"))?;
    }
    let domain = user_domain()?;
    let text = plist_text(&record, &environment);
    record.plist_sha256 = sha_hex(text.as_bytes());
    let prior_plist = std::fs::read(&record.plist).ok();
    let target = format!("{domain}/{}", record.label);
    let unchanged = existing.as_ref().is_some_and(|previous| {
        previous.executable == record.executable
            && previous.executable_sha256 == record.executable_sha256
            && previous.plist_sha256 == record.plist_sha256
            && previous.environment == record.environment
    });
    let service_loaded = run(
        Command::new("launchctl").args(["print", &target]),
        CONTROL_TIMEOUT,
    )?
    .success;
    let install = if unchanged && service_loaded {
        Ok(())
    } else if unchanged {
        launchctl(&["bootstrap", &domain, &record.plist.display().to_string()])
    } else {
        atomic_private(&record.plist, &text)?;
        manifest.native_hub = Some(record.clone());
        if let Err(error) = manifest.save() {
            manifest.native_hub = existing.clone();
            match prior_plist.as_ref() {
                Some(bytes) => {
                    if let Ok(text) = std::str::from_utf8(bytes) {
                        let _ = atomic_private(&record.plist, text);
                    }
                }
                None => {
                    let _ = std::fs::remove_file(&record.plist);
                }
            }
            return Err(error.to_string());
        }
        if service_loaded {
            let _ = launchctl(&["bootout", &target]);
        }
        launchctl(&["bootstrap", &domain, &record.plist.display().to_string()])
    };
    if let Err(error) = install {
        if let (Some(previous), Some(bytes)) = (existing.as_ref(), prior_plist.as_ref())
            && let Ok(text) = std::str::from_utf8(bytes)
        {
            let _ = atomic_private(&record.plist, text);
            manifest.native_hub = Some(previous.clone());
            let _ = manifest.save();
            let _ = launchctl(&["bootstrap", &domain, &record.plist.display().to_string()]);
        }
        return Err(format!(
            "{error} The native Hub remains recorded for safe cleanup."
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
        let _ = launchctl(&[
            "bootout",
            &format!("{}/{}", runtime.domain, runtime.record.label),
        ]);
        if let Some(bytes) = prior_plist
            && let Ok(text) = String::from_utf8(bytes)
        {
            let _ = atomic_private(&runtime.record.plist, &text);
            let _ = launchctl(&[
                "bootstrap",
                &runtime.domain,
                &runtime.record.plist.display().to_string(),
            ]);
            runtime.manifest.native_hub = existing.clone();
            let _ = runtime.manifest.save();
        }
        return Err(format!(
            "The hub at {} didn't become healthy within 60 seconds. launchd status: {}. Persistent service output is disabled to prevent unbounded logs; run the recorded executable with the plist arguments in a terminal for foreground diagnostics.",
            runtime.url,
            if status.is_empty() {
                "unavailable"
            } else {
                &status
            }
        ));
    }
    let started = hub_setup::finish(&mut runtime, ui, login, preset, tty, repo).await?;
    runtime.manifest.native_hub = Some(runtime.record);
    runtime.manifest.save().map_err(|error| error.to_string())?;
    Ok(started)
}

fn service_absent(output: &super::process::Output, label: &str) -> bool {
    !output.timed_out
        && output.code == Some(113)
        && output
            .stderr
            .contains(&format!("Could not find service \"{label}\""))
}

pub(crate) fn uninstall(record: &Record, home: &Path, wipe: bool) -> Result<(), String> {
    let native_root = home.join(".agentscm").join("hub-native");
    if !crate::quickstart::valid_hub_project(&record.project)
        || record.label != service_label(home, &record.project)
        || record.config_dir != native_root.join(&record.project)
        || record.data_dir != record.config_dir.join("data")
        || record.plist
            != home.join("Library").join("LaunchAgents").join(format!(
                "com.counterbranch.agentscm.{}.plist",
                record.project
            ))
    {
        return Err(
            "The recorded native Hub identity has drifted; refusing to stop or delete it.".into(),
        );
    }
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
    reject_symlink(&home.join(".agentscm"), "AgentSCM directory")?;
    reject_symlink(&native_root, "native Hub root")?;
    reject_symlink(&home.join("Library"), "Library directory")?;
    reject_symlink(&home.join("Library/LaunchAgents"), "LaunchAgents directory")?;
    reject_symlink(&record.config_dir, "configuration directory")?;
    reject_symlink(&record.data_dir, "data directory")?;
    let _uninstall_lock = install_lock(&record.config_dir)?;
    if !wipe {
        let receipt = Receipt {
            project: record.project.clone(),
            environment: record.environment.clone(),
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
    fn retained_receipt_recovers_only_whitelisted_environment() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("hub-native").join("work");
        super::super::private_dir(&config).unwrap();
        let environment = BTreeMap::from([
            (
                "AGENTSCM_DATABASE_URL".into(),
                format!("sqlite://{}", config.join("data/hub.sqlite").display()),
            ),
            ("AGENTSCM_ADMINS".into(), "github:octocat".into()),
        ]);
        let receipt = Receipt {
            project: "work".into(),
            environment: environment.clone(),
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
        };
        atomic_private(
            &receipt_path(&config),
            &serde_json::to_string(&unsafe_receipt).unwrap(),
        )
        .unwrap();
        assert!(load_receipt(&config, "work").is_err());
    }
}
