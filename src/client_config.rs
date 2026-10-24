//! Private, checkout-bound Hub connections used by repository-neutral client plugins.
//! A connection names one canonical Git root, one Hub and one repository token file.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use agentscm::hub::forge_http::check_base_url;
use agentscm::{Error, Result};

use crate::hub_cli::{config_dir, map_hub_error, write_private};
use crate::intent_cli::{repo_root, valid_repo};

const VERSION: u8 = 1;

/// Opaque root-and-token binding, shared by the native hook and MCP proxies.
/// Not a credential or remote attestation. A replaced token or root gets different proof.
pub(crate) fn onboarding_client(root: &Path, token: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"agentscm-onboarding-client-v1\0");
    digest.update((token.len() as u64).to_be_bytes());
    digest.update(token.as_bytes());
    digest.update(root.as_os_str().as_encoded_bytes());
    format!("{:x}", digest.finalize())
}

pub(crate) fn with_completion_reporting(
    request: reqwest::RequestBuilder,
    enabled: bool,
) -> reqwest::RequestBuilder {
    if enabled {
        request.header(agentscm::hub::insights::COMPLETION_HEADER, "true")
    } else {
        request
    }
}
const MAX_CONNECTIONS: usize = 128;
const FILE_BYTES: u64 = 64 * 1024;
const LOCK_WAIT: Duration = Duration::from_secs(2);
const ORIGIN_BYTES: usize = 64 * 1024;
const MAX_ORIGIN_URLS: usize = 32;
const ORIGIN_WAIT: Duration = Duration::from_secs(2);

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Connection {
    pub(crate) root: PathBuf,
    /// Lowercase forge host from the checkout's origin. Together with `repo`, this prevents the
    /// same owner/name on another forge from selecting this credential.
    pub(crate) origin_host: String,
    pub(crate) hub: String,
    pub(crate) repo: String,
    pub(crate) token_file: PathBuf,
    #[serde(default)]
    pub(crate) report_completion: bool,
    /// Present only for tokens minted and owned by this client connection flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owned_token_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owned_token_expires_at: Option<i64>,
    /// Agents `client setup` configured in this checkout, which `client remove` takes out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) agents: Vec<String>,
}

/// The names `client setup --agent` accepts.
pub(crate) const AGENTS: [&str; 5] = ["claude", "codex", "cursor", "copilot", "gemini"];

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Connections {
    version: u8,
    connections: Vec<Connection>,
}

fn path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agentscm").join("connections.json"))
}

fn lock_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agentscm").join("connections.lock"))
}

fn read_bounded(path: &Path) -> Result<Option<String>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > FILE_BYTES {
        return Err(invalid("connections.json is too large"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| invalid("connections.json is not valid UTF-8"))
}

fn checked(mut connection: Connection) -> Result<Connection> {
    connection.hub = check_base_url(&connection.hub).map_err(map_hub_error)?;
    if !valid_repo(&connection.repo)
        || !connection.root.is_absolute()
        || !connection.token_file.is_absolute()
        || connection.origin_host.is_empty()
        || connection.origin_host.chars().any(char::is_control)
        || connection.owned_token_id.is_some() != connection.owned_token_expires_at.is_some()
        || connection.owned_token_id.as_deref().is_some_and(|id| {
            !id.starts_with("tok_")
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
        || connection
            .owned_token_expires_at
            .is_some_and(|expiry| expiry <= 0)
        || connection.agents.iter().enumerate().any(|(index, agent)| {
            !AGENTS.contains(&agent.as_str()) || connection.agents[..index].contains(agent)
        })
    {
        return Err(invalid("connections.json has an invalid connection"));
    }
    if connection
        .root
        .components()
        .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(invalid("a configured repository root is not canonical"));
    }
    connection.origin_host.make_ascii_lowercase();
    if let Some(id) = connection.owned_token_id.as_deref()
        && owned_token_path(&connection, id)? != connection.token_file
    {
        return Err(invalid(
            "connections.json has an invalid managed token path",
        ));
    }
    Ok(connection)
}

pub(crate) fn owned_token_path(connection: &Connection, token_id: &str) -> Result<PathBuf> {
    let mut digest = Sha256::new();
    digest.update(connection.hub.as_bytes());
    digest.update([0]);
    digest.update(connection.root.as_os_str().as_encoded_bytes());
    digest.update([0]);
    digest.update(connection.origin_host.as_bytes());
    digest.update([0]);
    digest.update(connection.repo.as_bytes());
    digest.update([0]);
    digest.update(token_id.as_bytes());
    let suffix: String = digest.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(config_dir()?
        .join("agentscm")
        .join("tokens")
        .join(format!("client-{suffix}.token")))
}

fn parse(text: &str) -> Result<Vec<Connection>> {
    let parsed: Connections =
        serde_json::from_str(text).map_err(|_| invalid("connections.json is not valid"))?;
    if parsed.version != VERSION || parsed.connections.len() > MAX_CONNECTIONS {
        return Err(invalid("connections.json is not valid"));
    }
    let mut out = Vec::with_capacity(parsed.connections.len());
    for connection in parsed.connections {
        let connection = checked(connection)?;
        if out
            .iter()
            .any(|existing: &Connection| existing.root == connection.root)
        {
            return Err(invalid("connections.json repeats a repository root"));
        }
        out.push(connection);
    }
    out.sort_by(|left, right| left.root.cmp(&right.root));
    Ok(out)
}

pub(crate) fn load() -> Result<Vec<Connection>> {
    load_from(&path()?)
}

fn load_from(path: &Path) -> Result<Vec<Connection>> {
    match read_bounded(path)? {
        Some(text) => parse(&text),
        None => Ok(Vec::new()),
    }
}

fn save_to(path: &Path, connections: &[Connection]) -> Result<()> {
    if connections.len() > MAX_CONNECTIONS {
        return Err(invalid("too many client connections"));
    }
    let mut connections = connections.to_vec();
    connections.sort_by(|left, right| left.root.cmp(&right.root));
    let text = serde_json::to_string_pretty(&Connections {
        version: VERSION,
        connections,
    })? + "\n";
    if text.len() as u64 > FILE_BYTES {
        return Err(invalid("connections.json is too large"));
    }
    write_private(path, &text)
}

pub(crate) fn acquire_lock(path: &Path) -> Result<File> {
    let dir = path
        .parent()
        .ok_or_else(|| invalid("bad client connection lock path"))?;
    std::fs::create_dir_all(dir)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(invalid("client connection lock must be a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let lock = options.open(path)?;
    let metadata = lock.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(invalid("client connection lock must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no arguments or memory access preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(invalid("client connection lock has another owner"));
        }
        lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(invalid("client connections are busy; retry"));
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

/// Compare and replace one root while retaining unrelated entries written concurrently.
pub(crate) fn update(
    root: &Path,
    expected: Option<&Connection>,
    replacement: Option<Connection>,
) -> Result<()> {
    update_at(&path()?, &lock_path()?, root, expected, replacement)
}

fn update_at(
    config_path: &Path,
    lock_path: &Path,
    root: &Path,
    expected: Option<&Connection>,
    replacement: Option<Connection>,
) -> Result<()> {
    let _lock = acquire_lock(lock_path)?;
    let mut connections = load_from(config_path)?;
    replace(&mut connections, root, expected, replacement)?;
    save_to(config_path, &connections)
}

fn replace(
    connections: &mut Vec<Connection>,
    root: &Path,
    expected: Option<&Connection>,
    replacement: Option<Connection>,
) -> Result<()> {
    let current = connections
        .iter()
        .find(|connection| connection.root == root);
    if current != expected {
        return Err(invalid("client connection changed concurrently; retry"));
    }
    let replacement = match replacement {
        Some(connection) => {
            let connection = checked(connection)?;
            if connection.root != root {
                return Err(invalid("replacement client connection has another root"));
            }
            crate::client_cli::read_token(&connection.token_file)?;
            Some(connection)
        }
        None => None,
    };
    connections.retain(|connection| connection.root != root);
    if let Some(connection) = replacement {
        connections.push(connection);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetiredFile {
    Referenced,
    Removed,
}

pub(crate) fn remove_unreferenced_owned_file(connection: &Connection) -> Result<RetiredFile> {
    let id = connection
        .owned_token_id
        .as_deref()
        .ok_or_else(|| invalid("client token is not managed"))?;
    if owned_token_path(connection, id)? != connection.token_file {
        return Err(invalid("managed token path does not match its identity"));
    }
    remove_unreferenced_at(&path()?, &lock_path()?, &connection.token_file)
}

fn remove_unreferenced_at(
    config_path: &Path,
    lock_path: &Path,
    token_file: &Path,
) -> Result<RetiredFile> {
    let _lock = acquire_lock(lock_path)?;
    let connections = load_from(config_path)?;
    if connections
        .iter()
        .any(|connection| connection.token_file == token_file)
    {
        return Ok(RetiredFile::Referenced);
    }
    match std::fs::remove_file(token_file) {
        Ok(()) => Ok(RetiredFile::Removed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RetiredFile::Removed),
        Err(error) => Err(error.into()),
    }
}

/// Canonical Git root for an explicit path or a path inside the checkout.
pub(crate) fn canonical_root(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .map_err(|_| invalid("repository path does not exist"))?;
    repo_root(&canonical).ok_or_else(|| invalid("path is not inside a Git repository"))
}

fn remote_identity(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let (host, path) = if url.contains("://") {
        let parsed = reqwest::Url::parse(url).ok()?;
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return None;
        }
        (
            parsed
                .host_str()?
                .trim_matches(['[', ']'])
                .to_ascii_lowercase(),
            parsed.path().trim_matches('/').to_owned(),
        )
    } else {
        let (left, path) = url.split_once(':')?;
        if left.contains('/') || path.contains(['?', '#']) {
            return None;
        }
        (
            left.rsplit('@').next()?.to_ascii_lowercase(),
            path.trim_matches('/').to_owned(),
        )
    };
    let path = path.strip_suffix(".git").unwrap_or(&path).to_owned();
    (!host.is_empty() && valid_repo(&path)).then_some((host, path))
}

struct GitOutput {
    bytes: Vec<u8>,
    too_large: bool,
}

fn collect_git_output(mut stdout: std::process::ChildStdout) -> std::io::Result<GitOutput> {
    let mut bytes = Vec::with_capacity(ORIGIN_BYTES.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut too_large = false;
    loop {
        let read = stdout.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = ORIGIN_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        too_large |= read > remaining;
    }
    Ok(GitOutput { bytes, too_large })
}

/// Ask Git for the effective fetch URLs. This honors Git's config includes, worktree layout and
/// URL rewriting while remaining a local config-only operation.
fn effective_origin_urls(root: &Path) -> Result<Vec<String>> {
    let mut child = Command::new("git")
        .args(["remote", "get-url", "--all", "origin"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        // The explicit checkout, rather than inherited repository-routing variables, owns this
        // identity check. Other Git config remains effective, including includes and insteadOf.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .spawn()
        .map_err(|_| invalid("cannot inspect the checkout origin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| invalid("cannot inspect the checkout origin"))?;
    let reader = std::thread::spawn(move || collect_git_output(stdout));
    let deadline = Instant::now() + ORIGIN_WAIT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(invalid("checkout origin inspection timed out"));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(invalid("cannot inspect the checkout origin"));
            }
        }
    };
    let output = reader
        .join()
        .map_err(|_| invalid("cannot inspect the checkout origin"))?
        .map_err(|_| invalid("cannot inspect the checkout origin"))?;
    if !status.success() {
        return Err(invalid(
            "checkout origin is not a supported forge repository URL",
        ));
    }
    if output.too_large {
        return Err(invalid("checkout origin output is too large"));
    }
    let text = String::from_utf8(output.bytes)
        .map_err(|_| invalid("checkout origin output is not valid UTF-8"))?;
    let urls = text.lines().map(str::to_owned).collect::<Vec<_>>();
    if urls.len() > MAX_ORIGIN_URLS {
        return Err(invalid("checkout origin has too many URLs"));
    }
    Ok(urls)
}

/// The checkout's unique `origin` forge host and repository name.
pub(crate) fn origin_identity(root: &Path) -> Result<(String, String)> {
    let mut identities = effective_origin_urls(root)?
        .into_iter()
        .map(|url| {
            if url.is_empty() || url.chars().any(char::is_control) {
                return Err(invalid("checkout origin has an unsupported URL"));
            }
            remote_identity(&url).ok_or_else(|| invalid("checkout origin has an unsupported URL"))
        })
        .collect::<Result<Vec<_>>>()?;
    identities.sort_by(|left, right| {
        left.0.cmp(&right.0).then_with(|| {
            left.1
                .to_ascii_lowercase()
                .cmp(&right.1.to_ascii_lowercase())
        })
    });
    identities.dedup_by(|left, right| left.0 == right.0 && left.1.eq_ignore_ascii_case(&right.1));
    match identities.as_slice() {
        [identity] => Ok(identity.clone()),
        [] => Err(invalid(
            "checkout origin is not a supported forge repository URL",
        )),
        _ => Err(invalid("checkout origin names several forge repositories")),
    }
}

/// Whether the checkout still exists at its canonical path and has the bound origin identity.
pub(crate) fn is_current(connection: &Connection) -> bool {
    connection
        .root
        .canonicalize()
        .is_ok_and(|root| root == connection.root)
        && repo_root(&connection.root).as_deref() == Some(connection.root.as_path())
        && same_origin(connection, &connection.root)
}

/// [`is_current`], and a linked worktree `checkout` still has the bound origin identity, which
/// worktree-specific Git configuration could otherwise change.
pub(crate) fn is_current_in(connection: &Connection, checkout: &Path) -> bool {
    is_current(connection) && (checkout == connection.root || same_origin(connection, checkout))
}

fn same_origin(connection: &Connection, checkout: &Path) -> bool {
    origin_identity(checkout).is_ok_and(|(host, repo)| {
        host.eq_ignore_ascii_case(&connection.origin_host)
            && repo.eq_ignore_ascii_case(&connection.repo)
    })
}

/// Resolve an existing prefix, then normalize a possibly-not-yet-created tail.
pub(crate) fn resolved(path: &Path) -> Option<PathBuf> {
    let parts: Vec<Component<'_>> = path.components().collect();
    let existing = (1..=parts.len())
        .rev()
        .find(|count| parts[..*count].iter().collect::<PathBuf>().exists())?;
    let mut out = parts[..existing]
        .iter()
        .collect::<PathBuf>()
        .canonicalize()
        .ok()?;
    for part in &parts[existing..] {
        match part {
            Component::Normal(name) => out.push(name),
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// Longest registered root containing the path. Nested repositories route to the inner root.
pub(crate) fn connection_for_path<'a>(
    connections: &'a [Connection],
    path: &Path,
) -> Option<&'a Connection> {
    let path = resolved(path)?;
    connections
        .iter()
        .filter(|connection| path.starts_with(&connection.root))
        .max_by_key(|connection| connection.root.components().count())
}

/// The connection for the Git checkout containing `path`, and that checkout's root. A linked
/// worktree of a registered checkout shares its Git directory and routes to it, so a session an
/// editor starts in a new worktree needs no setup. A nested or separate clone never matches.
pub(crate) fn connection_for_checkout<'a>(
    connections: &'a [Connection],
    path: &Path,
) -> Option<(&'a Connection, PathBuf)> {
    let resolved = resolved(path)?;
    let checkout = resolved
        .ancestors()
        .find(|path| path.exists())
        .and_then(repo_root)?;
    if let Some(connection) = connections.iter().find(|c| c.root == checkout) {
        return Some((connection, checkout));
    }
    let common = common_dir(&checkout)?;
    connections
        .iter()
        .filter(|connection| common_dir(&connection.root).as_deref() == Some(common.as_path()))
        // Several checkouts of one repository may be registered; prefer the main worktree.
        .min_by_key(|connection| connection.root.join(".git") != common)
        .map(|connection| (connection, checkout))
}

/// The main worktree of the checkout at `root`: its own root unless it is a verified linked
/// worktree of a non-bare repository, whose shared Git directory is `<main>/.git`.
pub(crate) fn main_checkout(root: &Path) -> PathBuf {
    common_dir(root)
        .filter(|common| common.file_name().is_some_and(|name| name == ".git"))
        .and_then(|common| common.parent().map(Path::to_owned))
        .filter(|main| repo_root(main).as_deref() == Some(main.as_path()))
        .unwrap_or_else(|| root.to_owned())
}

/// The Git directory every worktree of the checkout at `root` shares. A linked worktree's `.git`
/// file names its own directory under the shared `worktrees/`, which Git points back at the
/// checkout; a copied, forged or submodule pointer fails these checks.
fn common_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return dot_git.canonicalize().ok();
    }
    let read = |path: &Path| std::fs::read_to_string(path).ok();
    let own = root
        .join(read(&dot_git)?.strip_prefix("gitdir:")?.trim())
        .canonicalize()
        .ok()?;
    let back = own
        .join(read(&own.join("gitdir"))?.trim())
        .canonicalize()
        .ok()?;
    let common = own
        .join(read(&own.join("commondir"))?.trim())
        .canonicalize()
        .ok()?;
    (back == dot_git.canonicalize().ok()? && own.parent()? == common.join("worktrees"))
        .then_some(common)
}

/// A safe repo-relative path under `root`, resolving symlinked existing prefixes.
pub(crate) fn relative(root: &Path, path: &Path) -> Option<String> {
    let path = resolved(path)?;
    let parts: Vec<&str> = path
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<_>>()?;
    let joined = parts.join("/");
    let bad = joined.is_empty()
        || joined.len() > 1024
        || joined.contains(['*', '\\'])
        || joined.chars().any(char::is_control);
    (!bad).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn git(root: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn connection(root: PathBuf, repo: &str) -> Connection {
        Connection {
            root,
            origin_host: "example.test".into(),
            hub: "https://hub.example.test".into(),
            repo: repo.into(),
            token_file: std::env::temp_dir().join("token"),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: Vec::new(),
        }
    }

    #[test]
    fn the_longest_registered_root_wins() {
        let parent = tempfile::tempdir().unwrap();
        let nested = parent.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let roots = vec![
            connection(parent.path().canonicalize().unwrap(), "a/parent"),
            connection(nested.canonicalize().unwrap(), "a/nested"),
        ];
        let found = connection_for_path(&roots, &nested.join("new.rs")).unwrap();
        assert_eq!(found.repo, "a/nested");
    }

    #[test]
    fn managed_token_paths_are_unique_per_server_token() {
        let root = tempfile::tempdir().unwrap();
        let saved = connection(root.path().canonicalize().unwrap(), "acme/widgets");
        assert_ne!(
            owned_token_path(&saved, "tok_first").unwrap(),
            owned_token_path(&saved, "tok_second").unwrap()
        );
    }

    #[test]
    fn compare_and_replace_retains_unrelated_connections() {
        let parent = tempfile::tempdir().unwrap();
        let mut one = connection(parent.path().join("one"), "acme/one");
        one.token_file = parent.path().join("one.token");
        write_private(&one.token_file, "one-token\n").unwrap();
        let two = connection(parent.path().join("two"), "acme/two");
        let mut connections = vec![one.clone(), two.clone()];
        let mut replacement = one.clone();
        replacement.hub = "https://new.example.test".into();
        replace(
            &mut connections,
            &one.root,
            Some(&one),
            Some(replacement.clone()),
        )
        .unwrap();
        assert!(connections.contains(&two));
        assert!(connections.contains(&replacement));
        assert!(replace(&mut connections, &one.root, Some(&one), None).is_err());
    }

    #[test]
    fn reporting_toggle_is_a_compare_and_replace_update() {
        let parent = tempfile::tempdir().unwrap();
        let config_path = parent.path().join("connections.json");
        let lock_path = parent.path().join("connections.lock");
        let token_file = parent.path().join("token");
        write_private(&token_file, "synthetic-token\n").unwrap();
        let mut original = connection(parent.path().join("checkout"), "acme/widgets");
        original.token_file = token_file;
        save_to(&config_path, &[original.clone()]).unwrap();

        let mut enabled = original.clone();
        enabled.report_completion = true;
        update_at(
            &config_path,
            &lock_path,
            &original.root,
            Some(&original),
            Some(enabled.clone()),
        )
        .unwrap();
        assert_eq!(load_from(&config_path).unwrap(), vec![enabled.clone()]);
        assert!(
            update_at(
                &config_path,
                &lock_path,
                &original.root,
                Some(&original),
                Some(enabled),
            )
            .is_err()
        );
    }

    #[test]
    fn managed_token_file_is_preserved_while_another_binding_references_it() {
        let parent = tempfile::tempdir().unwrap();
        let config_path = parent.path().join("connections.json");
        let lock_path = parent.path().join("connections.lock");
        let token_file = parent.path().join("managed.token");
        write_private(&token_file, "shared-token\n").unwrap();
        let mut reference = connection(parent.path().join("checkout"), "acme/widgets");
        reference.token_file = token_file.clone();
        save_to(&config_path, &[reference.clone()]).unwrap();

        assert_eq!(
            remove_unreferenced_at(&config_path, &lock_path, &token_file).unwrap(),
            RetiredFile::Referenced
        );
        assert_eq!(
            std::fs::read_to_string(&token_file).unwrap(),
            "shared-token\n"
        );

        update_at(
            &config_path,
            &lock_path,
            &reference.root,
            Some(&reference),
            None,
        )
        .unwrap();
        assert_eq!(
            remove_unreferenced_at(&config_path, &lock_path, &token_file).unwrap(),
            RetiredFile::Removed
        );
        assert!(!token_file.exists());
    }

    #[test]
    fn failed_publication_preserves_the_old_binding_and_token_file() {
        let parent = tempfile::tempdir().unwrap();
        let config_path = parent.path().join("connections.json");
        let lock_path = parent.path().join("connections.lock");
        let old_token = parent.path().join("old.token");
        write_private(&old_token, "old-token\n").unwrap();
        let mut old = connection(parent.path().join("checkout"), "acme/widgets");
        old.token_file = old_token.clone();
        save_to(&config_path, &[old.clone()]).unwrap();

        let mut replacement = old.clone();
        replacement.hub = "https://new.example.test".into();
        replacement.token_file = parent.path().join("missing.token");
        assert!(
            update_at(
                &config_path,
                &lock_path,
                &old.root,
                Some(&old),
                Some(replacement),
            )
            .is_err()
        );

        assert_eq!(load_from(&config_path).unwrap(), vec![old]);
        assert_eq!(std::fs::read_to_string(old_token).unwrap(), "old-token\n");
    }

    #[cfg(unix)]
    #[test]
    fn connection_lock_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("target.lock");
        let lock_path = parent.path().join("connections.lock");
        write_private(&target, "").unwrap();
        symlink(&target, &lock_path).unwrap();

        assert!(acquire_lock(&lock_path).is_err());
    }

    #[test]
    fn version_one_connections_without_ownership_metadata_still_load() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let text = json!({
            "version": 1,
            "connections": [{
                "root": root,
                "origin_host": "example.test",
                "hub": "https://hub.example.test",
                "repo": "acme/widgets",
                "token_file": std::env::temp_dir().join("supplied-token")
            }]
        })
        .to_string();
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].owned_token_id.is_none());
        assert!(parsed[0].owned_token_expires_at.is_none());
        assert!(!parsed[0].report_completion);
    }

    #[test]
    fn relative_paths_resolve_existing_symlinks_and_new_tails() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        assert_eq!(
            relative(&root, &root.join("src/../src/new.rs")).as_deref(),
            Some("src/new.rs")
        );
        assert!(relative(&root, &root.join("../outside.rs")).is_none());
    }

    #[test]
    fn remote_fingerprints_keep_the_host_and_drop_transport_details() {
        for (url, want) in [
            (
                "git@github.com:Acme/widgets.git",
                Some(("github.com", "Acme/widgets")),
            ),
            (
                "ssh://git@gitlab.example.test:2222/group/widgets.git",
                Some(("gitlab.example.test", "group/widgets")),
            ),
            (
                "https://token@github.com/acme/widgets.git",
                Some(("github.com", "acme/widgets")),
            ),
            ("/srv/git/acme/widgets", None),
        ] {
            let got = remote_identity(url);
            assert_eq!(
                got.as_ref()
                    .map(|(host, repo)| (host.as_str(), repo.as_str())),
                want,
                "{url}"
            );
        }
    }

    #[test]
    fn origin_identity_requires_one_unique_forge_and_repository() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        let base = std::fs::read_to_string(dir.path().join(".git/config")).unwrap();
        std::fs::write(
            dir.path().join(".git/config"),
            format!("{base}[remote \"origin\"]\n  URL = git@github.com:acme/widgets.git\n"),
        )
        .unwrap();
        assert_eq!(
            origin_identity(dir.path()).unwrap(),
            ("github.com".into(), "acme/widgets".into())
        );
        let saved = Connection {
            root: dir.path().canonicalize().unwrap(),
            origin_host: "github.com".into(),
            hub: "https://hub.example.test".into(),
            repo: "acme/widgets".into(),
            token_file: std::env::temp_dir().join("token"),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: Vec::new(),
        };
        assert!(is_current(&saved));
        std::fs::write(
            dir.path().join(".git/config"),
            format!(
                "{base}[remote \"origin\"]\n  URL = git@github.com:acme/widgets.git\n  URL = git@gitlab.com:acme/other.git\n"
            ),
        )
        .unwrap();
        assert!(!is_current(&saved));
        for unsupported in ["/srv/mirror/other.git", "file:///srv/mirror/other.git"] {
            std::fs::write(
                dir.path().join(".git/config"),
                format!("{base}[remote \"origin\"]\n  url = {unsupported}\n  url = https://github.com/acme/widgets.git\n"),
            ).unwrap();
            assert!(origin_identity(dir.path()).is_err());
            assert!(!is_current(&saved));
        }

        let many = format!(
            "{base}[remote \"origin\"]\n{}",
            "  URL = https://github.com/acme/widgets.git\n".repeat(MAX_ORIGIN_URLS + 1)
        );
        std::fs::write(dir.path().join(".git/config"), many).unwrap();
        assert!(origin_identity(dir.path()).is_err());
    }

    #[test]
    fn origin_identity_honors_includes_and_git_url_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        let base = std::fs::read_to_string(dir.path().join(".git/config")).unwrap();
        std::fs::write(
            dir.path().join(".git/config"),
            format!(
                "{base}[include]\n  path = origin.inc\n[url \"ssh://git@github.com/\"]\n  insteadOf = gh:\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".git/origin.inc"),
            "[remote \"origin\"]\n  URL = gh:Acme/widgets.git\n",
        )
        .unwrap();

        assert_eq!(
            origin_identity(dir.path()).unwrap(),
            ("github.com".into(), "Acme/widgets".into())
        );
    }

    #[test]
    fn origin_identity_uses_the_common_config_from_a_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        let linked = dir.path().join("linked");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "--quiet"]);
        git(&main, &["config", "user.name", "AgentSCM Test"]);
        git(&main, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(main.join("tracked"), "test\n").unwrap();
        git(&main, &["add", "tracked"]);
        git(&main, &["commit", "--quiet", "-m", "test"]);
        git(
            &main,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );

        assert_eq!(
            origin_identity(&linked).unwrap(),
            ("github.com".into(), "acme/widgets".into())
        );
    }

    #[test]
    fn linked_worktrees_route_to_their_registered_checkout_and_other_clones_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "--quiet"]);
        git(&main, &["config", "user.name", "AgentSCM Test"]);
        git(&main, &["config", "user.email", "test@example.invalid"]);
        git(&main, &["commit", "--quiet", "--allow-empty", "-m", "test"]);
        let main = main.canonicalize().unwrap();
        // Editors create session worktrees both inside the checkout and beside it.
        let nested = main.join(".claude/worktrees/task");
        let beside = dir.path().join("beside");
        let third = dir.path().join("third");
        for linked in [&nested, &beside, &third] {
            let linked = linked.to_str().unwrap();
            git(&main, &["worktree", "add", "--quiet", "--detach", linked]);
        }
        let foreign = main.join("vendor/other");
        std::fs::create_dir_all(&foreign).unwrap();
        git(&foreign, &["init", "--quiet"]);
        let connections = [connection(main.clone(), "acme/widgets")];

        assert_eq!(
            connection_for_checkout(&connections, &main.join("src/new.rs")),
            Some((&connections[0], main.clone()))
        );
        for linked in [&nested, &beside] {
            let linked = linked.canonicalize().unwrap();
            assert_eq!(
                connection_for_checkout(&connections, &linked.join("src/new.rs")),
                Some((&connections[0], linked))
            );
        }
        assert_eq!(
            connection_for_checkout(&connections, &foreign.join("a.rs")),
            None
        );

        // A copied `.git` pointer is not a worktree Git created for this checkout.
        let copied = dir.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::copy(beside.join(".git"), copied.join(".git")).unwrap();
        assert_eq!(connection_for_checkout(&connections, &copied), None);
        for checkout in [
            &main,
            &nested.canonicalize().unwrap(),
            &beside.canonicalize().unwrap(),
        ] {
            assert_eq!(main_checkout(checkout), main);
        }
        assert_eq!(main_checkout(&copied), copied);
        assert_eq!(main_checkout(&foreign), foreign);

        let both = [
            connection(beside.canonicalize().unwrap(), "acme/widgets"),
            connection(main.clone(), "acme/widgets"),
        ];
        assert_eq!(
            connection_for_checkout(&both, &third).map(|(connection, _)| &connection.root),
            Some(&main)
        );
    }
}
