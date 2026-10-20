use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use agentscm::team::TeamError;
use agentscm::team::auth::{self, Credentials};
use agentscm::team::http;
use agentscm::team::publication::RepositoryScope;
use agentscm::team::service::{
    self, BootstrapRequest, RetireAccessRequest, TeamConfig, TeamService,
};
use agentscm::{Error, Result};
use clap::Subcommand;
use serde::Serialize;
use serde::de::DeserializeOwned;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const MAX_TEAM_INPUT_BYTES: u64 = 8 * 1024;
const CREDENTIAL_BYTES: usize = 64;
const MAX_CREDENTIAL_FILE_BYTES: u64 = CREDENTIAL_BYTES as u64 + 1;

#[cfg(any(target_os = "linux", target_os = "android"))]
const NO_FOLLOW: i32 = 0x2_0000;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
const NO_FOLLOW: i32 = 0x100;

#[derive(Debug, Subcommand)]
pub enum TeamCommand {
    /// Serve the authenticated team API behind a same-origin HTTPS proxy.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Create a new private operator credential file.
    Credential {
        #[arg(long)]
        output: PathBuf,
    },
    /// Bootstrap one repository and its first administrator.
    Bootstrap {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, default_value = "-")]
        input: PathBuf,
        #[arg(long)]
        credential_file: PathBuf,
    },
    /// Permanently retire application access through trusted local authority.
    RetireAccess {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, default_value = "-")]
        input: PathBuf,
    },
    /// Show the current credential's repository-scoped identity metadata.
    Whoami {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, value_name = "INPUT")]
        scope: PathBuf,
        #[arg(long)]
        credential_file: PathBuf,
    },
}

pub(crate) fn telemetry_action(command: &TeamCommand) -> bool {
    matches!(
        command,
        TeamCommand::Credential { .. }
            | TeamCommand::Bootstrap { .. }
            | TeamCommand::RetireAccess { .. }
    )
}

pub async fn run(command: TeamCommand) -> Result<()> {
    match command {
        TeamCommand::Serve { config } => {
            let config = blocking(move || read_config(&config)).await?;
            http::serve(config).await.map_err(map_team_error)
        }
        TeamCommand::Credential { output } => {
            blocking(move || {
                let secret = auth::generate_secret().map_err(map_team_error)?;
                if !create_new_credential(&output, secret.expose())? {
                    return Err(Error::Conflict(
                        "team credential file already exists".into(),
                    ));
                }
                Ok(())
            })
            .await?;
            print_json(&serde_json::json!({"ok": true}))
        }
        TeamCommand::Bootstrap {
            config,
            input,
            credential_file,
        } => {
            let encoded = blocking(move || {
                let config = read_config(&config)?;
                let request: BootstrapRequest = read_json_input(&input)?;
                let credential = read_or_create_credential(&credential_file)?;
                let outcome = service::operator_bootstrap(&config.database, &request, &credential)
                    .map_err(map_team_error)?;
                encode_json(&outcome)
            })
            .await?;
            println!("{encoded}");
            Ok(())
        }
        TeamCommand::RetireAccess { config, input } => {
            let encoded = blocking(move || {
                let config = read_config(&config)?;
                let request: RetireAccessRequest = read_json_input(&input)?;
                let result = service::operator_retire_access(&config.database, &request)
                    .map_err(map_team_error)?;
                encode_json(&result)
            })
            .await?;
            println!("{encoded}");
            Ok(())
        }
        TeamCommand::Whoami {
            config,
            scope,
            credential_file,
        } => {
            let encoded = blocking(move || {
                let config = read_config(&config)?;
                let scope: RepositoryScope = read_json_input(&scope)?;
                let credential = read_credential(&credential_file)?;
                let credentials = Credentials::bearer(&credential).map_err(map_team_error)?;
                let mut service = TeamService::connect(&config.database, scope, credentials)
                    .map_err(map_team_error)?;
                let session = service.session().map_err(map_team_error)?;
                encode_json(&session)
            })
            .await?;
            println!("{encoded}");
            Ok(())
        }
    }
}

async fn blocking<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| Error::Invalid("team operation unavailable".into()))?
}

fn read_config(path: &Path) -> Result<TeamConfig> {
    let config: TeamConfig = read_json_file(path)?;
    config.validate()?;
    Ok(config)
}

fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path)?;
    read_json_from(file)
}

fn read_json_input<T: DeserializeOwned>(path: &Path) -> Result<T> {
    if path == Path::new("-") {
        read_json_from(io::stdin().lock())
    } else {
        read_json_file(path)
    }
}

fn read_json_from<T: DeserializeOwned>(reader: impl Read) -> Result<T> {
    let bytes = read_bounded(reader, MAX_TEAM_INPUT_BYTES, "team input")?;
    serde_json::from_slice(&bytes).map_err(|_| Error::Invalid("invalid team input".into()))
}

fn read_bounded(mut reader: impl Read, maximum: u64, label: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.by_ref().take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(Error::Limit(format!("{label} exceeds {maximum} bytes")));
    }
    Ok(bytes)
}

fn read_or_create_credential(path: &Path) -> Result<String> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_credential_mode(path, true),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let secret = auth::generate_secret().map_err(map_team_error)?;
            let credential = secret.expose().to_owned();
            if create_new_credential(path, &credential)? {
                Ok(credential)
            } else {
                read_credential_mode(path, true)
            }
        }
        Err(_) => Err(credential_file_error()),
    }
}

// The credential file is created only where owner-only permissions can be set (Unix).
#[cfg_attr(not(unix), allow(unreachable_code, unused_variables, unused_mut))]
fn create_new_credential(path: &Path, credential: &str) -> Result<bool> {
    if !valid_credential(credential) {
        return Err(credential_file_error());
    }
    let parent = parent_directory(path);
    let parent_metadata = fs::symlink_metadata(parent).map_err(|_| credential_file_error())?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(credential_file_error());
    }

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(_) => return Err(credential_file_error()),
    };

    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| credential_file_error())?;
    #[cfg(not(unix))]
    return Err(credential_file_error());

    file.write_all(credential.as_bytes())
        .map_err(|_| credential_file_error())?;
    file.sync_all().map_err(|_| credential_file_error())?;
    if !private_regular_file(&file.metadata().map_err(|_| credential_file_error())?) {
        return Err(credential_file_error());
    }
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| credential_file_error())?;
    Ok(true)
}

fn read_credential(path: &Path) -> Result<String> {
    read_credential_mode(path, false)
}

fn read_credential_mode(path: &Path, durable: bool) -> Result<String> {
    let before = fs::symlink_metadata(path).map_err(|_| credential_file_error())?;
    if !private_regular_file(&before) {
        return Err(credential_file_error());
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    options.custom_flags(NO_FOLLOW);
    let mut file = options.open(path).map_err(|_| credential_file_error())?;
    let opened = file.metadata().map_err(|_| credential_file_error())?;
    let after = fs::symlink_metadata(path).map_err(|_| credential_file_error())?;
    if !same_private_file(&before, &opened) || !same_private_file(&opened, &after) {
        return Err(credential_file_error());
    }

    let bytes = read_bounded(&mut file, MAX_CREDENTIAL_FILE_BYTES, "team credential file")
        .map_err(|_| credential_file_error())?;
    let credential = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let credential = std::str::from_utf8(credential).map_err(|_| credential_file_error())?;
    if !valid_credential(credential) {
        return Err(credential_file_error());
    }
    if durable {
        // A prior interrupted creation may contain valid bytes that were never
        // synced. Reuse must establish durability before database bootstrap.
        file.sync_all().map_err(|_| credential_file_error())?;
        File::open(parent_directory(path))
            .and_then(|directory| directory.sync_all())
            .map_err(|_| credential_file_error())?;
    }
    Ok(credential.to_owned())
}

fn valid_credential(value: &str) -> bool {
    value.len() == CREDENTIAL_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
fn private_regular_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && metadata.mode() & 0o777 & !0o600 == 0 && metadata.mode() & 0o400 != 0
}

#[cfg(not(unix))]
fn private_regular_file(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn same_private_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    private_regular_file(left) && left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_private_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    false
}

fn credential_file_error() -> Error {
    Error::Invalid("invalid team credential file".into())
}

fn map_team_error(error: TeamError) -> Error {
    match error {
        TeamError::Unauthorized => Error::Invalid("team authentication required".into()),
        TeamError::Denied => Error::Invalid("team permission denied".into()),
        TeamError::Conflict(_) => Error::Conflict("team operation conflict".into()),
        TeamError::Expired => Error::Conflict("team operation expired".into()),
        TeamError::Indeterminate => Error::Conflict("team outcome indeterminate".into()),
        TeamError::Unavailable | TeamError::Database(_) => {
            Error::Invalid("team service unavailable".into())
        }
        TeamError::Storage(Error::Conflict(_)) => Error::Conflict("team operation conflict".into()),
        TeamError::Storage(Error::NotFound(_)) => Error::NotFound("team resource".into()),
        TeamError::Storage(Error::MissingObject(_)) => Error::NotFound("team source object".into()),
        TeamError::Storage(Error::SourceDenied) => Error::Invalid("team permission denied".into()),
        TeamError::Storage(Error::SourceIndeterminate) => {
            Error::Conflict("team source indeterminate".into())
        }
        TeamError::Storage(Error::Limit(_)) => Error::Limit("team operation limit exceeded".into()),
        TeamError::Storage(_) => Error::Invalid("team operation failed".into()),
    }
}

fn encode_json(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|_| Error::Invalid("team output unavailable".into()))
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", encode_json(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_source_failures_never_echo_object_ids() {
        for (error, expected) in [
            (
                Error::MissingObject("do-not-disclose-object".into()),
                "not found: team source object",
            ),
            (Error::SourceDenied, "invalid: team permission denied"),
            (
                Error::SourceIndeterminate,
                "conflict: team source indeterminate",
            ),
        ] {
            assert_eq!(
                map_team_error(TeamError::Storage(error)).to_string(),
                expected
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_is_private_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("operator.credential");
        let credential = "a".repeat(CREDENTIAL_BYTES);

        assert!(create_new_credential(&path, &credential).unwrap());
        assert!(!create_new_credential(&path, &credential).unwrap());
        assert_eq!(read_credential(&path).unwrap(), credential);
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_rejects_public_permissions_and_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let credential = directory.path().join("operator.credential");
        fs::write(&credential, "b".repeat(CREDENTIAL_BYTES)).unwrap();
        fs::set_permissions(&credential, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_credential(&credential).is_err());

        fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.path().join("operator.link");
        symlink(&credential, &link).unwrap();
        assert!(read_credential(&link).is_err());
    }

    #[test]
    fn team_json_errors_do_not_include_input() {
        let invalid = br#"{"credential":"do-not-repeat"}"#;
        let error = read_json_from::<RepositoryScope>(&invalid[..]).unwrap_err();
        assert_eq!(error.to_string(), "invalid: invalid team input");

        let oversized = vec![b'x'; MAX_TEAM_INPUT_BYTES as usize + 1];
        let error = read_json_from::<RepositoryScope>(&oversized[..]).unwrap_err();
        assert_eq!(error.to_string(), "limit: team input exceeds 8192 bytes");
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_rejects_oversize_and_malformed_secrets_generically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("operator.credential");
        fs::write(&path, "c".repeat(CREDENTIAL_BYTES + 2)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_credential(&path).unwrap_err().to_string(),
            "invalid: invalid team credential file"
        );

        fs::write(&path, format!("{}g", "c".repeat(CREDENTIAL_BYTES - 1))).unwrap();
        assert_eq!(
            read_credential(&path).unwrap_err().to_string(),
            "invalid: invalid team credential file"
        );
    }
}
