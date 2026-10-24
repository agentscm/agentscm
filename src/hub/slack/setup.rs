//! Local-only Slack installation preparation.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::{SlackConfig, invalid, origin, private_file, secret, validate_config};
use crate::hub::{HubError, Result};

const CONFIG_FILE: &str = "config.json";
const DATABASE_FILE: &str = "install.sqlite";
/// The Slack journal and the SQLite side files that can sit beside it.
pub const JOURNAL_FILES: [&str; 3] = [DATABASE_FILE, "install.sqlite-wal", "install.sqlite-shm"];
const SIGNING_FILE: &str = "signing-secret";
const BOT_FILE: &str = "bot-token";
const MANIFEST_FILE: &str = "manifest.json";
const HOOKS_FILE: &str = "hooks.json";
const ICON_FILE: &str = "icon.png";
const ICON: &[u8] = include_bytes!("../../../docs/assets/agentscm-icon.png");

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Report {
    pub config_file: PathBuf,
    pub callback_url: String,
    pub binding_ids: Vec<String>,
}

/// A validated configuration with the Slack secrets it names.
pub struct Loaded {
    pub report: Report,
    pub config: SlackConfig,
    pub signing_secret: String,
    pub bot_token: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectReport {
    pub directory: PathBuf,
    pub manifest_file: PathBuf,
    pub bootstrap: bool,
    pub next_action: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AppRecord {
    pub team_id: String,
    pub app_id: String,
    pub team_domain: Option<String>,
}

/// Read and fully validate a private Slack configuration without opening its journal.
pub fn load(path: &Path) -> Result<Loaded> {
    let config: SlackConfig =
        serde_json::from_slice(&private_file(path, 64 * 1024)?).map_err(|_| invalid())?;
    let validated = validate_config(&config)?;
    Ok(Loaded {
        report: report(path.to_path_buf(), &config, validated.callback_url),
        config,
        signing_secret: validated.signing,
        bot_token: validated.bot,
    })
}

/// Read a private agent token and validate its local credential shape.
pub fn read_agent_token(path: &Path) -> Result<String> {
    let token = secret(path)?;
    super::agent_token_digest(&token)?;
    Ok(token)
}

/// Validate a local Slack configuration without network or journal side effects.
pub fn check(config_path: &Path) -> Result<Report> {
    load(config_path).map(|loaded| loaded.report)
}

/// Create the private configuration file for an installation directory.
pub fn prepare(directory: &Path, mut config: SlackConfig) -> Result<Report> {
    // Store the canonical origin so `https://hub` and `https://hub/` name the same config.
    config.hub_url = origin(&config.hub_url)?.to_string();
    ensure_install_directory(directory)?;
    let _lease = project_lease(directory)?;
    if config.database != directory.join(DATABASE_FILE) {
        return Err(invalid());
    }
    let config_file = directory.join(CONFIG_FILE);
    if !path_present(&config_file)? && path_present(&config.database)? {
        return Err(invalid());
    }
    let validated = validate_config(&config)?;
    let bytes = serialized_config(&config)?;
    let pending = config_file.with_extension("json.prepare");
    if path_present(&config_file)? {
        let existing_bytes = private_file(&config_file, 64 * 1024)?;
        let mut existing: SlackConfig =
            serde_json::from_slice(&existing_bytes).map_err(|_| invalid())?;
        // Earlier releases stored the origin as typed; compare it canonically too.
        existing.hub_url = origin(&existing.hub_url)?.to_string();
        if existing != config {
            return Err(HubError::Invalid(format!(
                "{} already holds a different Slack configuration; remove it to replace it, or rerun with the same values",
                config_file.display()
            )));
        }
        // A leftover pending file is the published config's hard link, in whatever form it was written.
        if path_present(&pending)? {
            exact_private_file(&pending, &existing_bytes)?;
            fs::remove_file(&pending)?;
        }
    } else {
        write_pending_generated(&pending, &bytes)?;
        // A hard link publishes only complete bytes and never replaces an existing file.
        fs::hard_link(&pending, &config_file)?;
        fs::remove_file(&pending)?;
    }
    fs::File::open(directory)?.sync_all()?;
    Ok(report(config_file, &config, validated.callback_url))
}

/// Build a secret-bearing in-memory bundle for an already validated installation.
pub fn bundle(config_path: &Path, target_directory: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    validate_lexical_absolute(target_directory)?;
    let mut config = load(config_path)?.config;
    let signing = private_file(&config.signing_secret_file, 4096)?;
    let bot = private_file(&config.bot_token_file, 4096)?;
    let mut agents = Vec::with_capacity(config.bindings.len());
    for binding in &config.bindings {
        agents.push(private_file(&binding.agent_token_file, 4096)?);
    }

    config.database = target_directory.join(DATABASE_FILE);
    config.signing_secret_file = target_directory.join(SIGNING_FILE);
    config.bot_token_file = target_directory.join(BOT_FILE);
    for (index, binding) in config.bindings.iter_mut().enumerate() {
        binding.agent_token_file = target_directory.join(agent_file(index));
    }

    let contents = [serialized_config(&config)?, signing, bot]
        .into_iter()
        .chain(agents);
    Ok(bundle_file_names(config.bindings.len())
        .zip(contents)
        .collect())
}

fn agent_file(index: usize) -> String {
    format!("agent-{index}.token")
}

/// The file names `bundle` gives a configuration with `bindings` bindings, in order.
pub fn bundle_file_names(bindings: usize) -> impl Iterator<Item = String> {
    [CONFIG_FILE, SIGNING_FILE, BOT_FILE]
        .into_iter()
        .map(str::to_owned)
        .chain((0..bindings).map(agent_file))
}

/// Whether `bundle` can name a file `name`.
pub fn bundle_file_name(name: &str) -> bool {
    matches!(name, CONFIG_FILE | SIGNING_FILE | BOT_FILE)
        || name
            .strip_prefix("agent-")
            .and_then(|rest| rest.strip_suffix(".token"))
            .and_then(|index| index.parse::<usize>().ok())
            .is_some_and(|index| name == agent_file(index))
}

/// Return a Slack app manifest for bootstrap or for a live HTTPS Hub origin.
pub fn manifest(hub_origin: Option<&str>) -> Result<Value> {
    let mut manifest: Value =
        serde_json::from_str(include_str!("../../../examples/slack-manifest.json"))
            .map_err(|_| HubError::Unavailable("embedded Slack manifest is invalid".into()))?;
    match hub_origin {
        Some(value) => {
            let callback = origin(value)?
                .join("v1/hub/slack")
                .map_err(|_| invalid())?
                .to_string();
            manifest["settings"]["event_subscriptions"]["request_url"] =
                Value::String(callback.clone());
            manifest["settings"]["interactivity"]["request_url"] = Value::String(callback);
        }
        None => {
            let settings = manifest["settings"].as_object_mut().ok_or_else(invalid)?;
            settings.remove("event_subscriptions");
            settings.remove("interactivity");
        }
    }
    Ok(manifest)
}

/// Create an idempotent private project for the official Slack CLI.
pub fn project(directory: &Path, hub_origin: Option<&str>) -> Result<ProjectReport> {
    ensure_install_directory(directory)?;
    let _lease = project_lease(directory)?;
    let bootstrap = hub_origin.is_none();
    let manifest_file = directory.join(MANIFEST_FILE);
    let manifest_bytes = serialized_json(&manifest(hub_origin)?)?;
    let bootstrap_bytes = serialized_json(&manifest(None)?)?;
    let manifest_state = project_manifest_state(
        &manifest_file,
        &manifest_bytes,
        &bootstrap_bytes,
        !bootstrap,
    )?;
    let icon_file = directory.join(ICON_FILE);
    exact_private_file(&icon_file, ICON)?;

    let slack_directory = directory.join(".slack");
    let hooks_file = slack_directory.join(HOOKS_FILE);
    let hooks_bytes = b"{}\n";
    let slack_directory_exists = path_present(&slack_directory)?;
    if slack_directory_exists {
        ensure_install_directory(&slack_directory)?;
        exact_private_file(&hooks_file, hooks_bytes)?;
    }

    if matches!(manifest_state, ProjectManifestState::Drift) {
        return Err(invalid());
    }
    if !slack_directory_exists {
        ensure_install_directory(&slack_directory)?;
    }
    write_new_private_file(&icon_file, ICON)?;
    write_new_private_file(&hooks_file, hooks_bytes)?;

    match manifest_state {
        ProjectManifestState::Missing => write_new_private_file(&manifest_file, &manifest_bytes)?,
        ProjectManifestState::Exact => write_new_private_file(&manifest_file, &manifest_bytes)?,
        ProjectManifestState::BootstrapUpgrade => {
            write_new_private_file(&manifest_file, &bootstrap_bytes)?;
            replace_exact_private_file(&manifest_file, &bootstrap_bytes, &manifest_bytes)?;
        }
        ProjectManifestState::Drift => unreachable!("drift returned before project writes"),
    }

    Ok(ProjectReport {
        directory: directory.to_path_buf(),
        manifest_file,
        bootstrap,
        next_action:
            "Run `agentscm hub slack install` with this directory and the intended workspace ID."
                .into(),
    })
}

/// Read local Slack CLI app metadata for an exact workspace; this is not installation proof.
pub fn app_record(directory: &Path, team_id: &str) -> Result<Option<AppRecord>> {
    validate_lexical_absolute(directory)?;
    if !super::slack_id(team_id, 'T') {
        return Err(invalid());
    }
    let path = directory.join(".slack/apps.json");
    if !path_present(&path)? {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_slice(&private_file(&path, 64 * 1024)?).map_err(|_| invalid())?;
    let apps = value
        .get("apps")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let Some(selected) = apps.get(team_id) else {
        return Ok(None);
    };
    let selected = selected.as_object().ok_or_else(invalid)?;
    let stored_team = selected
        .get("team_id")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let app_id = selected
        .get("app_id")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if stored_team != team_id || !super::slack_id(stored_team, 'T') || !super::slack_id(app_id, 'A')
    {
        return Err(invalid());
    }
    let team_domain = match selected.get("team_domain") {
        None | Some(Value::Null) => None,
        Some(Value::String(domain)) if valid_team_domain(domain) => Some(domain.clone()),
        Some(_) => return Err(invalid()),
    };
    Ok(Some(AppRecord {
        team_id: stored_team.to_owned(),
        app_id: app_id.to_owned(),
        team_domain,
    }))
}

enum ProjectManifestState {
    Missing,
    Exact,
    BootstrapUpgrade,
    Drift,
}

fn project_manifest_state(
    path: &Path,
    desired: &[u8],
    bootstrap: &[u8],
    allow_bootstrap_upgrade: bool,
) -> Result<ProjectManifestState> {
    if !path_present(path)? {
        return Ok(ProjectManifestState::Missing);
    }
    let current = private_file(path, 64 * 1024)?;
    Ok(if current == desired {
        ProjectManifestState::Exact
    } else if allow_bootstrap_upgrade && current == bootstrap {
        ProjectManifestState::BootstrapUpgrade
    } else {
        ProjectManifestState::Drift
    })
}

fn valid_team_domain(value: &str) -> bool {
    (1..=63).contains(&value.len())
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn report(config_file: PathBuf, config: &SlackConfig, callback_url: String) -> Report {
    Report {
        config_file,
        callback_url,
        binding_ids: config
            .bindings
            .iter()
            .map(|binding| binding.slack_user.clone())
            .collect(),
    }
}

fn serialized_config(config: &SlackConfig) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(config)
        .map_err(|_| HubError::Unavailable("Slack configuration serialization failed".into()))?;
    bytes.push(b'\n');
    if bytes.len() > 64 * 1024 {
        return Err(invalid());
    }
    Ok(bytes)
}

fn serialized_json(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| HubError::Unavailable("Slack manifest serialization failed".into()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn rejected(path: &Path, reason: &str) -> HubError {
    HubError::Invalid(format!("Slack directory {}: {reason}", path.display()))
}

fn validate_lexical_absolute(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(rejected(path, "use an absolute path"));
    }
    let mut saw_normal = false;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(_) => saw_normal = true,
            _ => return Err(rejected(path, "remove `.` and `..` components")),
        }
    }
    if !saw_normal {
        return Err(rejected(path, "use a directory below the filesystem root"));
    }
    Ok(())
}

fn ensure_install_directory(path: &Path) -> Result<()> {
    if cfg!(not(unix)) {
        return Err(HubError::Invalid(
            "Slack adapter requires Unix private-file permissions".into(),
        ));
    }
    validate_lexical_absolute(path)?;
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_private_directory(&current)?;
                fs::symlink_metadata(&current)?
            }
            Err(error) => return Err(error.into()),
        };
        validate_directory_component(&current, &metadata, current == path)?;
    }
    Ok(())
}

fn validate_directory_component(
    path: &Path,
    metadata: &fs::Metadata,
    #[cfg_attr(not(unix), allow(unused_variables))] destination: bool,
) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(rejected(
            path,
            "is a symbolic link; pass the resolved path (on macOS, /private/tmp rather than /tmp)",
        ));
    }
    if !metadata.is_dir() {
        return Err(rejected(path, "is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owner = metadata.uid();
        let mode = metadata.mode();
        if destination && (owner != super::effective_uid() || mode & 0o077 != 0) {
            return Err(rejected(
                path,
                "must be owned by you with no group or other access (mode 0700)",
            ));
        }
        let writable = mode & 0o022 != 0;
        let protected_temporary = owner == 0 && mode & 0o1000 != 0;
        if (owner != super::effective_uid() && owner != 0) || (writable && !protected_temporary) {
            return Err(rejected(
                path,
                "must be owned by you or root and not writable by other users",
            ));
        }
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn path_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn exact_private_file(path: &Path, expected: &[u8]) -> Result<bool> {
    if !path_present(path)? {
        return Ok(false);
    }
    let maximum = u64::try_from(expected.len()).map_err(|_| invalid())?;
    if private_file(path, maximum)? != expected {
        return Err(invalid());
    }
    Ok(true)
}

fn write_new_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let leaf = path
        .file_name()
        .ok_or_else(invalid)?
        .to_str()
        .ok_or_else(invalid)?;
    let pending = path.with_file_name(format!("{leaf}.create"));
    if exact_private_file(path, bytes)? {
        if path_present(&pending)? {
            let previous = private_file(&pending, bytes.len() as u64)?;
            if !bytes.starts_with(&previous) {
                return Err(invalid());
            }
            fs::remove_file(&pending)?;
        }
        return Ok(());
    }
    write_pending_generated(&pending, bytes)?;
    fs::hard_link(&pending, path)?;
    fs::remove_file(&pending)?;
    fs::File::open(path.parent().ok_or_else(invalid)?)?.sync_all()?;
    Ok(())
}

fn replace_exact_private_file(path: &Path, expected: &[u8], replacement: &[u8]) -> Result<()> {
    if private_file(path, 64 * 1024)? != expected {
        return Err(invalid());
    }
    let temporary = path.with_extension("json.activate");
    write_pending_generated(&temporary, replacement)?;
    if private_file(path, 64 * 1024)? != expected {
        return Err(invalid());
    }
    fs::rename(temporary, path)?;
    fs::File::open(path.parent().ok_or_else(invalid)?)?.sync_all()?;
    Ok(())
}

// Callers hold the private directory's lease before recovering a generated write.
fn write_pending_generated(temporary: &Path, replacement: &[u8]) -> Result<()> {
    if path_present(temporary)? {
        let previous = private_file(temporary, replacement.len() as u64)?;
        // A process can stop at any byte during the generated activation write.
        // The project lease excludes another activation while reclaiming its prefix.
        if !replacement.starts_with(&previous) {
            return Err(invalid());
        }
        fs::remove_file(temporary)?;
    }
    let mut file = create_private_file(temporary)?;
    file.write_all(replacement)?;
    file.sync_all()?;
    Ok(())
}

fn project_lease(directory: &Path) -> Result<crate::lease::Lease> {
    let path = directory.join(".agentscm-setup.lock");
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    super::validate_private_metadata(&file.metadata()?, 0)?;
    crate::lease::Lease::try_exclusive(file).map_err(|_| {
        HubError::Unavailable(
            "Slack project setup is already running; retry when it finishes".into(),
        )
    })
}

fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn private(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, SlackConfig) {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        private(&directory.path().join("signing"), b"synthetic-signing");
        private(&directory.path().join("bot"), b"xoxb-synthetic");
        private(&directory.path().join("agent"), &[b'a'; 64]);
        let config = SlackConfig {
            version: 1,
            app_id: "A123".into(),
            bot_id: "B123".into(),
            team_id: "T123".into(),
            hub_url: "https://hub.example.test/".into(),
            signing_secret_file: directory.path().join("signing"),
            bot_token_file: directory.path().join("bot"),
            database: directory.path().join(DATABASE_FILE),
            bindings: vec![super::super::BindingConfig {
                slack_user: "U123".into(),
                provider: "github".into(),
                host: "github.com".into(),
                forge_user_id: 42,
                repository: "acme/widgets".into(),
                repository_id: 101,
                agent_token_file: directory.path().join("agent"),
            }],
        };
        (directory, config)
    }

    #[test]
    fn prepare_is_create_only_idempotent_and_does_not_create_journal() {
        let (directory, mut config) = fixture();
        // The origin is stored canonically, so a rerun with or without the slash matches.
        config.hub_url = "https://hub.example.test".into();
        let first = prepare(directory.path(), config.clone()).unwrap();
        assert_eq!(first.binding_ids, ["U123"]);
        assert!(!config.database.exists());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(directory.path().join(CONFIG_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let original = fs::read(directory.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(
            load(&directory.path().join(CONFIG_FILE))
                .unwrap()
                .config
                .hub_url,
            "https://hub.example.test/"
        );
        config.hub_url = "https://hub.example.test/".into();
        prepare(directory.path(), config.clone()).unwrap();
        assert_eq!(
            fs::read(directory.path().join(CONFIG_FILE)).unwrap(),
            original
        );
        let mut changed = config;
        changed.team_id = "T999".into();
        let error = prepare(directory.path(), changed.clone())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("already holds a different Slack configuration"),
            "{error}"
        );

        // A config an earlier release stored with the origin as typed still matches a rerun.
        let (legacy, mut config) = fixture();
        config.hub_url = "https://hub.example.test".into();
        let legacy_bytes = serialized_config(&config).unwrap();
        private(&legacy.path().join(CONFIG_FILE), &legacy_bytes);
        for typed in ["https://hub.example.test", "https://hub.example.test/"] {
            config.hub_url = typed.into();
            prepare(legacy.path(), config.clone()).unwrap();
        }
        assert_eq!(
            fs::read(legacy.path().join(CONFIG_FILE)).unwrap(),
            legacy_bytes
        );
        assert_eq!(
            fs::read(directory.path().join(CONFIG_FILE)).unwrap(),
            original
        );
    }

    #[test]
    fn prepare_publishes_complete_config_and_recovers_an_interrupted_generated_write() {
        let (directory, config) = fixture();
        let pending = directory.path().join("config.json.prepare");
        let bytes = serialized_config(&config).unwrap();
        private(&pending, &bytes[..17]);
        prepare(directory.path(), config.clone()).unwrap();
        let loaded = load(&directory.path().join(CONFIG_FILE)).unwrap();
        assert!(loaded.config == config);
        assert_eq!(loaded.signing_secret, "synthetic-signing");
        assert_eq!(loaded.bot_token, "xoxb-synthetic");
        assert!(!pending.exists());
        // A stop between link publication and pending-file cleanup is also resumable.
        fs::hard_link(directory.path().join(CONFIG_FILE), &pending).unwrap();
        prepare(directory.path(), config).unwrap();
        assert!(!pending.exists());

        let (other, config) = fixture();
        private(
            &other.path().join("config.json.prepare"),
            b"unmanaged content",
        );
        assert!(prepare(other.path(), config).is_err());
        assert!(!other.path().join(CONFIG_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_creates_private_destination_and_refuses_unmanaged_journal() {
        let (source, mut config) = fixture();
        let destination = source.path().join("new").join("nested");
        config.database = destination.join(DATABASE_FILE);
        prepare(&destination, config.clone()).unwrap();
        for path in [source.path().join("new"), destination.clone()] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(!config.database.exists());

        let unmanaged = source.path().join("unmanaged");
        create_private_directory(&unmanaged).unwrap();
        private(&unmanaged.join(DATABASE_FILE), b"not-a-journal");
        config.database = unmanaged.join(DATABASE_FILE);
        assert!(prepare(&unmanaged, config).is_err());
        assert!(!unmanaged.join(CONFIG_FILE).exists());
    }

    #[test]
    fn check_rejects_public_config_and_has_no_journal_side_effect() {
        let (directory, config) = fixture();
        let path = directory.path().join("config-source");
        private(&path, &serialized_config(&config).unwrap());
        let report = check(&path).unwrap();
        assert_eq!(report.binding_ids, ["U123"]);
        assert_eq!(
            read_agent_token(&config.bindings[0].agent_token_file).unwrap(),
            "a".repeat(64)
        );
        assert!(!config.database.exists());
        #[cfg(unix)]
        {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(check(&path).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn prepare_rejects_symlink_components_and_public_directory() {
        let (directory, mut config) = fixture();
        let public = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        fs::set_permissions(public.path(), fs::Permissions::from_mode(0o755)).unwrap();
        config.database = public.path().join(DATABASE_FILE);
        let error = prepare(public.path(), config.clone())
            .unwrap_err()
            .to_string();
        assert!(error.contains("mode 0700"), "{error}");

        let link = directory.path().with_extension("link");
        std::os::unix::fs::symlink(directory.path(), &link).unwrap();
        config.database = link.join(DATABASE_FILE);
        let error = prepare(&link, config.clone()).unwrap_err().to_string();
        assert!(error.contains("is a symbolic link"), "{error}");
        assert!(error.contains(&link.display().to_string()), "{error}");
        fs::remove_file(link).unwrap();

        let error = prepare(Path::new("relative/slack"), config.clone())
            .unwrap_err()
            .to_string();
        assert!(error.contains("use an absolute path"), "{error}");
        let error = prepare(&directory.path().join("../slack"), config)
            .unwrap_err()
            .to_string();
        assert!(error.contains("`..`"), "{error}");
    }

    #[test]
    fn bundle_remaps_fixed_files_without_creating_journal() {
        let (directory, config) = fixture();
        let path = directory.path().join("config-source");
        private(&path, &serialized_config(&config).unwrap());
        let target = Path::new("/runtime/slack");
        let bundle = bundle(&path, target).unwrap();
        let names: Vec<_> = bundle.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "config.json",
                "signing-secret",
                "bot-token",
                "agent-0.token"
            ]
        );
        assert!(names.iter().all(|name| bundle_file_name(name)));
        assert!(!bundle_file_name("agent-01.token") && !bundle_file_name("install.sqlite"));
        let bundled: SlackConfig = serde_json::from_slice(&bundle[0].1).unwrap();
        assert_eq!(bundled.database, target.join(DATABASE_FILE));
        assert_eq!(bundled.signing_secret_file, target.join("signing-secret"));
        assert_eq!(
            bundled.bindings[0].agent_token_file,
            target.join("agent-0.token")
        );
        assert!(!config.database.exists());
    }

    #[test]
    fn manifests_separate_bootstrap_from_live_callbacks() {
        let bootstrap = manifest(None).unwrap();
        assert!(bootstrap["settings"].get("event_subscriptions").is_none());
        assert!(bootstrap["settings"].get("interactivity").is_none());
        assert_eq!(
            bootstrap["oauth_config"]["scopes"],
            serde_json::json!({"bot":["chat:write"]})
        );
        let home = &bootstrap["features"]["app_home"];
        assert_eq!(home["home_tab_enabled"], false);
        assert_eq!(home["messages_tab_enabled"], true);
        assert_eq!(home["messages_tab_read_only_enabled"], true);

        let live = manifest(Some("https://hub.example.test/")).unwrap();
        let callback = "https://hub.example.test/v1/hub/slack";
        assert_eq!(
            live["settings"]["event_subscriptions"]["request_url"],
            callback
        );
        assert_eq!(live["settings"]["interactivity"]["request_url"], callback);
        assert!(manifest(Some("http://hub.example.test/")).is_err());
        assert!(manifest(Some("https://hub.example.test/path")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn project_creates_minimal_private_bootstrap_files() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = root.path().join("slack-project");
        let report = project(&directory, None).unwrap();
        assert!(report.bootstrap);
        assert_eq!(report.manifest_file, directory.join(MANIFEST_FILE));
        assert!(report.next_action.contains("agentscm hub slack install"));
        let value: Value =
            serde_json::from_slice(&fs::read(&report.manifest_file).unwrap()).unwrap();
        assert_eq!(
            value["oauth_config"]["scopes"],
            serde_json::json!({"bot":["chat:write"]})
        );
        assert!(value["settings"].get("interactivity").is_none());
        assert_eq!(
            fs::read(directory.join(".slack/hooks.json")).unwrap(),
            b"{}\n"
        );
        assert_eq!(fs::read(directory.join(ICON_FILE)).unwrap(), ICON);
        for path in [
            directory.clone(),
            directory.join(".slack"),
            report.manifest_file,
            directory.join(".slack/hooks.json"),
            directory.join(ICON_FILE),
        ] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert!(mode == 0o700 || mode == 0o600);
        }
        project(&directory, None).unwrap();
    }

    #[test]
    fn project_refuses_drift_and_preserves_cli_state() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = root.path().join("slack-project");
        project(&directory, None).unwrap();
        let app_state = directory.join(".slack/apps.local.json");
        let credentials = directory.join(".slack/credentials.json");
        private(&app_state, b"synthetic-app-state");
        private(&credentials, b"synthetic-credentials");
        private(&directory.join(MANIFEST_FILE), b"drift");

        assert!(project(&directory, Some("https://hub.example.test/")).is_err());
        assert_eq!(fs::read(app_state).unwrap(), b"synthetic-app-state");
        assert_eq!(fs::read(credentials).unwrap(), b"synthetic-credentials");
        assert_eq!(fs::read(directory.join(MANIFEST_FILE)).unwrap(), b"drift");
    }

    #[test]
    fn project_allows_only_bootstrap_to_requested_live_activation() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = root.path().join("slack-project");
        project(&directory, None).unwrap();
        let bootstrap = fs::read(directory.join(MANIFEST_FILE)).unwrap();
        let app_state = directory.join(".slack/apps.local.json");
        private(&app_state, b"synthetic-app-state");

        private(&directory.join(ICON_FILE), b"drift");
        assert!(project(&directory, Some("https://hub.example.test/")).is_err());
        assert_eq!(fs::read(directory.join(MANIFEST_FILE)).unwrap(), bootstrap);
        private(&directory.join(ICON_FILE), ICON);

        private(&directory.join(".slack/hooks.json"), b"drift");
        assert!(project(&directory, Some("https://hub.example.test/")).is_err());
        assert_eq!(fs::read(directory.join(MANIFEST_FILE)).unwrap(), bootstrap);
        private(&directory.join(".slack/hooks.json"), b"{}\n");

        let report = project(&directory, Some("https://hub.example.test/")).unwrap();
        assert!(!report.bootstrap);
        let live = fs::read(&report.manifest_file).unwrap();
        assert_ne!(live, bootstrap);
        project(&directory, Some("https://hub.example.test/")).unwrap();
        assert_eq!(fs::read(&report.manifest_file).unwrap(), live);

        assert!(project(&directory, None).is_err());
        assert!(project(&directory, Some("https://other.example.test/")).is_err());
        assert_eq!(fs::read(&report.manifest_file).unwrap(), live);
        assert_eq!(fs::read(app_state).unwrap(), b"synthetic-app-state");
    }

    #[test]
    fn project_activation_recovers_generated_prefixes_and_refuses_unowned_temp_content() {
        for partial in [false, true] {
            let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
            #[cfg(unix)]
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
            project(root.path(), None).unwrap();
            let live =
                serialized_json(&manifest(Some("https://hub.example.test")).unwrap()).unwrap();
            let pending = root.path().join("manifest.json.activate");
            private(&pending, if partial { &live[..31] } else { &live });
            project(root.path(), Some("https://hub.example.test")).unwrap();
            assert_eq!(fs::read(root.path().join(MANIFEST_FILE)).unwrap(), live);
            assert!(!pending.exists());
        }
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        project(root.path(), None).unwrap();
        let bootstrap = fs::read(root.path().join(MANIFEST_FILE)).unwrap();
        private(
            &root.path().join("manifest.json.activate"),
            b"unmanaged content",
        );
        assert!(project(root.path(), Some("https://hub.example.test")).is_err());
        assert_eq!(
            fs::read(root.path().join(MANIFEST_FILE)).unwrap(),
            bootstrap
        );
        let lease = project_lease(root.path()).unwrap();
        assert!(project(root.path(), None).is_err());
        drop(lease);
        project(root.path(), None).unwrap();
    }

    #[test]
    fn project_initial_generated_files_resume_after_an_interrupted_write() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let hooks_directory = root.path().join(".slack");
        ensure_install_directory(&hooks_directory).unwrap();
        let manifest = serialized_json(&manifest(None).unwrap()).unwrap();
        for (path, bytes) in [
            (root.path().join(MANIFEST_FILE), manifest.as_slice()),
            (root.path().join(ICON_FILE), ICON),
            (hooks_directory.join(HOOKS_FILE), b"{}\n".as_slice()),
        ] {
            let pending = path.with_file_name(format!(
                "{}.create",
                path.file_name().unwrap().to_str().unwrap()
            ));
            private(&pending, &bytes[..bytes.len() / 2]);
        }
        project(root.path(), None).unwrap();
        assert_eq!(fs::read(root.path().join(MANIFEST_FILE)).unwrap(), manifest);
        assert_eq!(fs::read(root.path().join(ICON_FILE)).unwrap(), ICON);
        assert_eq!(fs::read(hooks_directory.join(HOOKS_FILE)).unwrap(), b"{}\n");
        let paths = [
            root.path().join(MANIFEST_FILE),
            root.path().join(ICON_FILE),
            hooks_directory.join(HOOKS_FILE),
        ];
        for path in &paths {
            let pending = path.with_file_name(format!(
                "{}.create",
                path.file_name().unwrap().to_str().unwrap()
            ));
            fs::hard_link(path, pending).unwrap();
        }
        project(root.path(), Some("https://hub.example.test")).unwrap();
        for path in &paths {
            assert!(
                !path
                    .with_file_name(format!(
                        "{}.create",
                        path.file_name().unwrap().to_str().unwrap()
                    ))
                    .exists()
            );
        }
    }

    #[test]
    fn app_record_selects_only_the_requested_private_workspace_record() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = root.path().join("slack-project");
        project(&directory, None).unwrap();
        assert_eq!(app_record(&directory, "T123").unwrap(), None);
        let path = directory.join(".slack/apps.json");
        private(
            &path,
            br#"{"apps":{"T123":{"app_id":"A123","team_domain":"acme","team_id":"T123"},"T999":{"future":"metadata"}},"default":"other","workspace_metadata":{"future":true}}"#,
        );
        assert_eq!(
            app_record(&directory, "T123").unwrap(),
            Some(AppRecord {
                team_id: "T123".into(),
                app_id: "A123".into(),
                team_domain: Some("acme".into()),
            })
        );
        assert!(app_record(&directory, "T999").is_err());
        // No entry for the workspace is the CLI's "not recorded" case, not malformed state.
        assert_eq!(app_record(&directory, "T555").unwrap(), None);
        assert!(app_record(&directory, "invalid").is_err());

        private(
            &path,
            br#"{"apps":{"T123":{"app_id":"A123","team_id":"T999"}}}"#,
        );
        assert!(app_record(&directory, "T123").is_err());
        #[cfg(unix)]
        {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(app_record(&directory, "T123").is_err());
            fs::remove_file(&path).unwrap();
            let target = directory.join("apps-target.json");
            private(
                &target,
                br#"{"apps":{"T123":{"app_id":"A123","team_id":"T123"}}}"#,
            );
            std::os::unix::fs::symlink(target, &path).unwrap();
            assert!(app_record(&directory, "T123").is_err());
        }
    }
}
