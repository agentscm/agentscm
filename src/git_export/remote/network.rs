//! Explicit client credentials and host trust for standard Git transports.
//! Only endpoint/trust descriptions are persisted; access material is per call.
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{invalid, path_text, shell_quote};
use crate::git_export::git::{Git, PushExit};
use crate::{Error, Result};

const MAX_SECRET: u64 = 16 * 1024;
const MAX_TRUST: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum Endpoint {
    Https {
        url: String,
        trust: HttpsTrust,
    },
    Ssh {
        url: String,
        known_hosts_sha256: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HttpsTrust {
    System,
    CaFile { sha256: String },
}

/// Explicit paths, never secret bytes, supplied by the trusted operator per call.
#[derive(Default)]
pub struct Access {
    pub credential_file: Option<PathBuf>,
    pub credential_helper: Option<PathBuf>,
    pub ca_file: Option<PathBuf>,
    pub ssh: Option<SshAccess>,
}

pub struct SshAccess {
    pub executable: PathBuf,
    pub identity_file: PathBuf,
    pub known_hosts_file: PathBuf,
}

impl SshAccess {
    /// Refuse filenames OpenSSH could reinterpret after shell parsing. This is
    /// also checked before CLI binding reads its selected trust description.
    pub fn validate_paths(&self) -> Result<()> {
        for path in [&self.identity_file, &self.known_hosts_file] {
            if !path.is_absolute()
                || path_text(path)?.contains(['%', '$'])
                || path_text(path)?.chars().any(char::is_control)
            {
                return Err(invalid(
                    "SSH input paths must be absolute without expansion syntax or controls",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpCredential {
    username: String,
    password: String,
}

impl Endpoint {
    pub fn url(&self) -> &str {
        match self {
            Self::Https { url, .. } | Self::Ssh { url, .. } => url,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let parsed = checked_url(self.url())?;
        match self {
            Self::Https { trust, .. } => {
                if parsed.scheme() != "https" || !parsed.username().is_empty() {
                    return Err(invalid("HTTPS endpoint must not contain credentials"));
                }
                if let HttpsTrust::CaFile { sha256 } = trust {
                    validate_digest(sha256)?;
                }
            }
            Self::Ssh {
                known_hosts_sha256, ..
            } => {
                if parsed.scheme() != "ssh"
                    || parsed.username().is_empty()
                    || parsed.username().starts_with('-')
                    || !parsed
                        .username()
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                {
                    return Err(invalid(
                        "SSH endpoint requires an explicit simple user name",
                    ));
                }
                validate_digest(known_hosts_sha256)?;
            }
        }
        Ok(())
    }

    pub(super) fn validate_access(&self, access: &Access) -> Result<()> {
        self.validate()?;
        super::reject_command_policy()?;
        match self {
            Self::Https { trust, .. } => {
                if access.ssh.is_some() {
                    return Err(invalid("SSH access cannot be used for HTTPS"));
                }
                let credential = access
                    .credential_file
                    .as_deref()
                    .ok_or_else(|| invalid("HTTPS requires an explicit credential file"))?;
                read_credential(credential)?;
                checked_executable(access.credential_helper.as_deref().ok_or_else(|| {
                    invalid("HTTPS requires a trusted credential helper executable")
                })?)?;
                match (trust, access.ca_file.as_deref()) {
                    (HttpsTrust::System, None) => {}
                    (HttpsTrust::CaFile { sha256 }, Some(path))
                        if &trust_digest(path)? == sha256 => {}
                    _ => return Err(invalid("TLS trust input differs from the bound profile")),
                }
            }
            Self::Ssh {
                known_hosts_sha256, ..
            } => {
                if access.credential_file.is_some() || access.ca_file.is_some() {
                    return Err(invalid("HTTPS access cannot be used for SSH"));
                }
                let ssh = access.ssh.as_ref().ok_or_else(|| {
                    invalid("SSH requires an explicit identity and host trust file")
                })?;
                ssh.validate_paths()?;
                checked_executable(&ssh.executable)?;
                checked_file(&ssh.identity_file, true, 64 * 1024)?;
                if &trust_digest(&ssh.known_hosts_file)? != known_hosts_sha256 {
                    return Err(invalid("SSH host trust input differs from its binding"));
                }
            }
        }
        Ok(())
    }

    pub(super) fn run(
        &self,
        git: &Git,
        access: &Access,
        args: &[&str],
        budget: u64,
    ) -> Result<Vec<u8>> {
        let exit = self.run_exit(git, access, args, budget)?;
        if !exit.finished || exit.code != Some(0) {
            return Err(Error::SourceIndeterminate);
        }
        Ok(exit.stdout)
    }

    pub(super) fn run_exit(
        &self,
        git: &Git,
        access: &Access,
        args: &[&str],
        budget: u64,
    ) -> Result<PushExit> {
        self.validate_access(access)?;
        // Git/OpenSSH reopen trust paths. Keep the exact digest-verified bytes
        // under our own temporary name so concurrent operator rotation cannot
        // change the trust used after validation. No secret material is copied.
        let trust_snapshot = match self {
            Self::Https {
                trust: HttpsTrust::CaFile { sha256 },
                ..
            } => Some(TrustSnapshot::new(
                access
                    .ca_file
                    .as_deref()
                    .ok_or_else(|| invalid("CA file is absent"))?,
                sha256,
            )?),
            Self::Ssh {
                known_hosts_sha256, ..
            } => Some(TrustSnapshot::new(
                &access
                    .ssh
                    .as_ref()
                    .ok_or_else(|| invalid("SSH access is absent"))?
                    .known_hosts_file,
                known_hosts_sha256,
            )?),
            Self::Https {
                trust: HttpsTrust::System,
                ..
            } => None,
        };
        let mut config: Vec<(String, String)> = [
            ("protocol.allow", "never"),
            ("credential.helper", ""),
            ("credential.interactive", "false"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let mut environment = vec![
            ("GIT_ASKPASS".into(), "/usr/bin/false".into()),
            ("SSH_ASKPASS".into(), "/usr/bin/false".into()),
            ("GIT_PROTOCOL_FROM_USER".into(), "0".into()),
        ];
        match self {
            Self::Https { trust, .. } => {
                config.extend([
                    ("protocol.https.allow".into(), "always".into()),
                    ("http.followRedirects".into(), "false".into()),
                    ("http.sslVerify".into(), "true".into()),
                    ("http.proxy".into(), String::new()),
                    ("credential.useHttpPath".into(), "true".into()),
                ]);
                let helper = access
                    .credential_helper
                    .as_deref()
                    .ok_or_else(|| invalid("credential helper is absent"))?;
                config.push((
                    "credential.helper".into(),
                    format!("!{} git-remote-credential", shell_quote(path_text(helper)?)),
                ));
                if let HttpsTrust::CaFile { .. } = trust {
                    config.push((
                        "http.sslCAInfo".into(),
                        path_text(
                            &trust_snapshot
                                .as_ref()
                                .ok_or_else(|| invalid("CA snapshot is absent"))?
                                .path,
                        )?
                        .into(),
                    ));
                }
                environment.push((
                    "AGENTSCM_REMOTE_CREDENTIAL_FILE".into(),
                    path_text(
                        access
                            .credential_file
                            .as_deref()
                            .ok_or_else(|| invalid("credential file is absent"))?,
                    )?
                    .into(),
                ));
                environment.push(("AGENTSCM_REMOTE_CREDENTIAL_URL".into(), self.url().into()));
            }
            Self::Ssh { .. } => {
                let ssh = access
                    .ssh
                    .as_ref()
                    .ok_or_else(|| invalid("SSH access is absent"))?;
                config.push(("protocol.ssh.allow".into(), "always".into()));
                environment.push(("GIT_SSH_VARIANT".into(), "ssh".into()));
                environment.push((
                    "GIT_SSH_COMMAND".into(),
                    ssh_command(
                        ssh,
                        &trust_snapshot
                            .as_ref()
                            .ok_or_else(|| invalid("SSH trust snapshot is absent"))?
                            .path,
                    )?,
                ));
            }
        }
        git.transport(args, &config, &environment, budget)
    }
}

pub fn trust_digest(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(read_trust(path)?)))
}

fn read_trust(path: &Path) -> Result<Vec<u8>> {
    let file = checked_file(path, false, MAX_TRUST)?;
    let mut bytes = Vec::new();
    file.take(MAX_TRUST + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_TRUST {
        return Err(invalid("host trust file exceeds its bounds"));
    }
    Ok(bytes)
}

struct TrustSnapshot {
    directory: PathBuf,
    path: PathBuf,
}

impl TrustSnapshot {
    fn new(source: &Path, expected: &str) -> Result<Self> {
        let bytes = read_trust(source)?;
        if format!("{:x}", Sha256::digest(&bytes)) != expected {
            return Err(invalid("host trust changed before transport"));
        }
        let parent = std::env::temp_dir().canonicalize()?;
        let metadata = fs::metadata(&parent)?;
        // SAFETY: geteuid has no arguments or mutable process effects.
        let owner = unsafe { libc::geteuid() };
        if !metadata.is_dir()
            || (metadata.uid() != owner && metadata.uid() != 0)
            || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
        {
            return Err(invalid(
                "unsupported transport temporary directory ownership",
            ));
        }
        // The generated filename is literal, including when passed to SSH.
        let text = path_text(&parent)?;
        if text.contains(['%', '$']) || text.chars().any(char::is_control) {
            return Err(invalid("unsupported transport temporary directory"));
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| invalid("transport random source unavailable"))?;
        let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let directory = parent.join(format!("agentscm-remote-trust-{name}"));
        DirBuilder::new().mode(0o700).create(&directory)?;
        let snapshot = Self {
            path: directory.join("trust"),
            directory,
        };
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&snapshot.path)?
            .write_all(&bytes)?;
        Ok(snapshot)
    }
}

impl Drop for TrustSnapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

fn checked_url(value: &str) -> Result<Url> {
    let parsed = Url::parse(value).map_err(|_| invalid("unsupported Git endpoint URL"))?;
    if value.len() > 2048
        || parsed.as_str() != value
        || !matches!(parsed.scheme(), "https" | "ssh")
        || parsed.host_str().is_none()
        || parsed.port() == Some(0)
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() == "/"
        || parsed.path().ends_with('/')
        || parsed
            .path()
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !parsed
            .path()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
    {
        return Err(invalid(
            "Git endpoint needs a canonical HTTPS/SSH URL and simple repository path",
        ));
    }
    Ok(parsed)
}

fn checked_executable(path: &Path) -> Result<()> {
    if !path.is_absolute() || !fs::metadata(path)?.is_file() {
        return Err(invalid(
            "transport executable must be an explicit absolute file",
        ));
    }
    Ok(())
}

fn checked_file(path: &Path, private: bool, limit: u64) -> Result<File> {
    if !path.is_absolute() {
        return Err(invalid("transport input files must use absolute paths"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid takes no pointers and only reads the process identity.
    let owner = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.len() > limit
        || metadata.mode() & if private { 0o077 } else { 0o022 } != 0
        || (metadata.uid() != owner && (!private && metadata.uid() != 0))
        || (private && metadata.uid() != owner)
    {
        return Err(invalid(
            "transport input file ownership, permissions or size is unsupported",
        ));
    }
    Ok(file)
}

fn read_credential(path: &Path) -> Result<HttpCredential> {
    let mut bytes = Vec::new();
    checked_file(path, true, MAX_SECRET)?
        .take(MAX_SECRET + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SECRET {
        return Err(invalid("credential file exceeds its bound"));
    }
    let credential: HttpCredential =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid Git credential file"))?;
    if credential.username.contains(':')
        || credential.username.is_empty()
        || credential.username.len() > 256
        || credential.password.is_empty()
        || credential.password.len() > 8192
        || credential
            .username
            .chars()
            .chain(credential.password.chars())
            .any(char::is_control)
    {
        return Err(invalid("unsupported Git credential fields"));
    }
    Ok(credential)
}

/// Internal credential-helper protocol. Secret fields only leave through Git's
/// private helper stdout; no diagnostics or persistent credential store is used.
pub fn credential_reply(operation: &str, input: &mut dyn Read) -> Result<Option<String>> {
    if !matches!(operation, "get" | "store" | "erase") {
        return Err(invalid("unsupported credential helper operation"));
    }
    let mut bytes = Vec::new();
    input.take(MAX_SECRET + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SECRET {
        return Err(invalid("credential helper request exceeds its bound"));
    }
    if operation != "get" {
        return Ok(None);
    }
    let expected = std::env::var("AGENTSCM_REMOTE_CREDENTIAL_URL")
        .map_err(|_| invalid("credential scope is absent"))?;
    let url = checked_url(&expected)?;
    if url.scheme() != "https" || !url.username().is_empty() {
        return Err(invalid("unsupported credential scope"));
    }
    let request = std::str::from_utf8(&bytes).map_err(|_| invalid("invalid credential request"))?;
    let mut fields = std::collections::BTreeMap::new();
    for line in request.lines().filter(|line| !line.is_empty()) {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| invalid("invalid credential request"))?;
        if matches!(key, "protocol" | "host" | "path" | "username")
            && fields.insert(key, value).is_some()
        {
            return Err(invalid("duplicate credential scope field"));
        }
    }
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or("")),
        None => url.host_str().unwrap_or("").into(),
    };
    if fields.get("protocol") != Some(&"https")
        || fields.get("host").copied() != Some(host.as_str())
        || fields.get("path").copied() != Some(url.path().trim_start_matches('/'))
    {
        return Err(invalid(
            "credential request differs from its bound endpoint",
        ));
    }
    let file = std::env::var_os("AGENTSCM_REMOTE_CREDENTIAL_FILE")
        .ok_or_else(|| invalid("credential file is absent"))?;
    let credential = read_credential(Path::new(&file))?;
    if fields
        .get("username")
        .is_some_and(|username| *username != credential.username)
    {
        return Err(invalid("credential username differs from its input"));
    }
    Ok(Some(format!(
        "username={}\npassword={}\n\n",
        credential.username, credential.password
    )))
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return Err(invalid("invalid host trust digest"));
    }
    Ok(())
}

fn ssh_command(access: &SshAccess, known_hosts: &Path) -> Result<String> {
    let known = path_text(known_hosts)?
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let mut parts = vec![
        path_text(&access.executable)?.to_owned(),
        "-F".into(),
        "/dev/null".into(),
        "-i".into(),
        path_text(&access.identity_file)?.into(),
    ];
    for option in [
        "BatchMode=yes",
        "NumberOfPasswordPrompts=0",
        "ConnectionAttempts=1",
        "ConnectTimeout=10",
        "RequestTTY=no",
        "IdentitiesOnly=yes",
        "IdentityAgent=none",
        "CertificateFile=/dev/null",
        "PKCS11Provider=none",
        "SecurityKeyProvider=none",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
        "PreferredAuthentications=publickey",
        "StrictHostKeyChecking=yes",
        "UpdateHostKeys=no",
        "VerifyHostKeyDNS=no",
        "GlobalKnownHostsFile=/dev/null",
        "AddKeysToAgent=no",
        "ControlMaster=no",
        "ControlPath=none",
        "ProxyCommand=none",
        "ProxyJump=none",
        "PermitLocalCommand=no",
        "LogLevel=ERROR",
    ] {
        parts.extend(["-o".into(), option.into()]);
    }
    parts.extend(["-o".into(), format!("UserKnownHostsFile=\"{known}\"")]);
    Ok(parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    #[test]
    fn trust_snapshot_pins_verified_bytes_and_cleans_up_on_return() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("trust.pem");
        fs::write(&source, b"selected trust").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        let digest = trust_digest(&source).unwrap();
        let snapshot = TrustSnapshot::new(&source, &digest).unwrap();
        let path = snapshot.path.clone();
        let directory = snapshot.directory.clone();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        let replacement = root.path().join("replacement");
        fs::write(&replacement, b"rotated trust").unwrap();
        fs::rename(&replacement, &source).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"selected trust");
        assert!(TrustSnapshot::new(&source, &digest).is_err());
        drop(snapshot);
        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn actual_ssh_parser_keeps_strict_identity_and_host_configuration() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("identity 'with space");
        let known = root.path().join("host trust 'with space");
        fs::write(&key, "").unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&known, "").unwrap();
        let access = SshAccess {
            executable: PathBuf::from("/usr/bin/ssh"),
            identity_file: key,
            known_hosts_file: known,
        };
        let command = ssh_command(&access, &access.known_hosts_file).unwrap();
        // -G prints effective configuration and exits; it performs no network
        // authentication and reads no machine identities/config (-F /dev/null).
        let output = Command::new("/bin/sh")
            .args([
                "-c",
                &format!("{command} -G -p 2222 synthetic@synthetic.invalid"),
            ])
            .env_clear()
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        for line in [
            "batchmode yes",
            "identitiesonly yes",
            "identityagent none",
            "certificatefile /dev/null",
            "stricthostkeychecking true",
            "passwordauthentication no",
            "updatehostkeys false",
        ] {
            assert!(
                text.lines().any(|value| value == line),
                "strict SSH option missing"
            );
        }
        // OpenSSH omits disabled provider fields on some versions instead of
        // printing "none". An enabled provider must never survive parsing.
        for field in ["pkcs11provider ", "securitykeyprovider "] {
            assert!(
                text.lines()
                    .filter(|line| line.starts_with(field))
                    .all(|line| line == format!("{field}none"))
            );
        }
        assert!(text.contains(access.known_hosts_file.to_str().unwrap()));
        assert!(text.contains(access.identity_file.to_str().unwrap()));
    }
}
