//! Isolated Git transport for local-validation reconciliation.
//!
//! The checkout is used only to discover one direct `origin` URL and its object directory. Every
//! fetch runs in a private temporary bare repository without checkout, user or system Git
//! configuration. Authenticated mode accepts only the canonical HTTPS URL for the policy's
//! GitHub repository and passes its header to the fetch child through Git's runtime environment.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tempfile::TempDir;

use super::run::{config_dir_tree, rev};
use crate::execution::Cancellation;
use crate::execution::process::capture_with_input;
use crate::{Error, Result};

const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const GIT_OUTPUT: u64 = 16 * 1024 * 1024;

struct Authentication<'a> {
    token: &'a str,
    canonical_url: String,
}

/// One isolated transport context reused for every fetch in a reconcile run.
pub struct FetchContext<'a> {
    repo: &'a Path,
    remote_url: String,
    object_dir: PathBuf,
    temporary: TempDir,
    authentication: Option<Authentication<'a>>,
}

impl<'a> FetchContext<'a> {
    /// Uses the checkout's direct `remote.origin.url` without any credential configuration.
    pub fn anonymous(repo: &'a Path) -> Result<Self> {
        Self::new(repo, None, None, None)
    }

    /// Reuses `token` only for a canonical HTTPS GitHub transport matching both repository
    /// identities. The caller must separately restrict the GitHub API to its production URL.
    pub fn authenticated(
        repo: &'a Path,
        github_repo: &str,
        policy_repository: &str,
        token: &'a str,
    ) -> Result<Self> {
        if !valid_github_repo(github_repo) {
            return Err(invalid("GitHub repository must be OWNER/NAME"));
        }
        let expected_policy = format!("github.com/{github_repo}");
        if policy_repository != expected_policy {
            return Err(invalid("GitHub repository differs from the policy"));
        }
        let canonical_url = format!("https://github.com/{github_repo}.git");
        Self::new(repo, Some(github_repo), Some(token), Some(canonical_url))
    }

    fn new(
        repo: &'a Path,
        authenticated_repo: Option<&str>,
        token: Option<&'a str>,
        canonical_url: Option<String>,
    ) -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        for name in ["home", "xdg", "template"] {
            let path = temporary.path().join(name);
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
        let remote_url = direct_origin(repo, temporary.path())?;
        if let Some(github_repo) = authenticated_repo {
            let without_suffix = format!("https://github.com/{github_repo}");
            let with_suffix = format!("{without_suffix}.git");
            if remote_url != without_suffix && remote_url != with_suffix {
                return Err(invalid(
                    "authenticated fetch requires the canonical GitHub origin URL",
                ));
            }
        } else {
            validate_anonymous_remote(&remote_url)?;
        }
        let object_dir = repository_path(
            repo,
            temporary.path(),
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ],
            "object directory",
        )?;
        let git_dir = temporary.path().join("transport.git");
        let template = temporary.path().join("template");
        let mut init = isolated_command(temporary.path());
        init.args(["init", "--bare", "--quiet", "--template"])
            .arg(&template)
            .arg(&git_dir);
        run(&mut init, GIT_TIMEOUT, "init")?;
        record_command("init", &init, false);
        let authentication = match (token, canonical_url.as_ref()) {
            (Some(token), Some(canonical_url)) => Some(Authentication {
                token,
                canonical_url: canonical_url.clone(),
            }),
            (None, None) => None,
            _ => return Err(invalid("incomplete authenticated fetch configuration")),
        };
        Ok(Self {
            repo,
            remote_url: canonical_url.unwrap_or(remote_url),
            object_dir,
            temporary,
            authentication,
        })
    }

    fn git_dir(&self) -> PathBuf {
        self.temporary.path().join("transport.git")
    }

    fn fetch(&self, refspec: &str) -> Result<()> {
        let mut command = isolated_command(self.temporary.path());
        command
            .current_dir(self.repo)
            .env("GIT_OBJECT_DIRECTORY", &self.object_dir)
            .arg("--git-dir")
            .arg(self.git_dir())
            .args([
                "--no-replace-objects",
                "--no-lazy-fetch",
                "--no-optional-locks",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "credential.helper=",
                "-c",
                "fetch.recurseSubmodules=false",
                "-c",
                "submodule.recurse=false",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "-c",
                "http.followRedirects=false",
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "protocol.git.allow=never",
                "-c",
                "protocol.ssh.allow=never",
                "-c",
                "protocol.file.allow=always",
            ]);
        if let Some(authentication) = &self.authentication {
            let basic = STANDARD.encode(format!("x-access-token:{}", authentication.token));
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env(
                    "GIT_CONFIG_KEY_0",
                    format!("http.{}.extraHeader", authentication.canonical_url),
                )
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("Authorization: Basic {basic}"),
                );
        }
        let remote = test_remote_override(&self.remote_url, self.authentication.is_some());
        command.args([
            "fetch",
            "--no-tags",
            "--quiet",
            "--no-recurse-submodules",
            "--",
        ]);
        command.arg(remote).arg(refspec);
        record_command("fetch", &command, self.authentication.is_some());
        run(&mut command, FETCH_TIMEOUT, "fetch").map(drop)
    }

    fn temporary_rev(&self, name: &str) -> Result<String> {
        let mut command = isolated_command(self.temporary.path());
        command
            .env("GIT_OBJECT_DIRECTORY", &self.object_dir)
            .arg("--git-dir")
            .arg(self.git_dir())
            .args([
                "--no-replace-objects",
                "--no-lazy-fetch",
                "rev-parse",
                "--verify",
            ])
            .arg(format!("{name}^{{commit}}"));
        record_command("rev-parse", &command, false);
        let out = run(&mut command, GIT_TIMEOUT, "rev-parse")?;
        let value = String::from_utf8(out).map_err(|_| invalid("Git returned a non-UTF-8 oid"))?;
        let value = value.trim();
        if !is_oid(value) {
            return Err(invalid("Git returned an invalid oid"));
        }
        Ok(value.to_owned())
    }

    fn fetch_base(&self, base: &str) -> Result<()> {
        if !crate::hub::github::valid_ref_name(base) {
            return Err(invalid("base is not a branch name"));
        }
        self.fetch(&format!(
            "+refs/heads/{base}:refs/agentscm/local-validation-base"
        ))?;
        let commit = self.temporary_rev("refs/agentscm/local-validation-base")?;
        let reference = format!("refs/remotes/origin/{base}");
        let mut command = local_command(self.repo, self.temporary.path());
        command
            .args(["-c", "core.hooksPath=/dev/null", "update-ref"])
            .arg(reference)
            .arg(commit);
        record_command("update-ref", &command, false);
        run(&mut command, GIT_TIMEOUT, "update-ref").map(drop)
    }

    /// Fetches the base and pull-request refs as data and requires temporary `FETCH_HEAD` to be
    /// exactly the API-observed head.
    pub fn fetch_pull(&self, base: &str, number: i64, head: &str) -> Result<()> {
        if number < 1 || !is_oid(head) {
            return Err(invalid("invalid pull request ref"));
        }
        self.fetch_base(base)?;
        self.fetch(&format!("refs/pull/{number}/head"))?;
        if self.temporary_rev("FETCH_HEAD")? != head {
            return Err(invalid("pull request head moved while fetching"));
        }
        Ok(())
    }

    /// Re-fetches the base through the same isolated context and compares `dir` with checkout
    /// `HEAD`. A missing directory at the base means it changed.
    pub fn config_dir_unchanged(&self, base: &str, dir: &str) -> Result<bool> {
        self.fetch_base(base)?;
        let tip = format!("refs/remotes/origin/{base}");
        rev(self.repo, &tip, "^{commit}")?;
        let Ok(theirs) = rev(self.repo, &tip, &format!(":./{dir}")) else {
            return Ok(false);
        };
        Ok(config_dir_tree(self.repo, dir)? == theirs)
    }

    #[cfg(test)]
    fn temporary_path(&self) -> &Path {
        self.temporary.path()
    }
}

fn direct_origin(repo: &Path, private_root: &Path) -> Result<String> {
    let mut command = local_command(repo, private_root);
    command.args([
        "config",
        "--local",
        "--no-includes",
        "--null",
        "--get-all",
        "remote.origin.url",
    ]);
    record_command("config", &command, false);
    let out = run(&mut command, GIT_TIMEOUT, "config")?;
    let values = out
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .map(|value| {
            std::str::from_utf8(value)
                .map(str::to_owned)
                .map_err(|_| invalid("origin URL must be UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    if values.len() != 1 {
        return Err(invalid("exactly one direct origin URL is required"));
    }
    Ok(values.into_iter().next().unwrap_or_default())
}

fn repository_path(repo: &Path, private_root: &Path, args: &[&str], what: &str) -> Result<PathBuf> {
    let mut command = local_command(repo, private_root);
    command.args(args);
    record_command(what, &command, false);
    let out = run(&mut command, GIT_TIMEOUT, what)?;
    let value = String::from_utf8(out).map_err(|_| invalid("Git path must be UTF-8"))?;
    let path = PathBuf::from(value.trim());
    if !path.is_absolute() || !path.is_dir() {
        return Err(invalid("Git object directory is unavailable"));
    }
    Ok(path)
}

fn validate_anonymous_remote(remote: &str) -> Result<()> {
    if remote.is_empty() || remote.starts_with('-') || remote.contains('\0') {
        return Err(invalid("unsafe origin URL"));
    }
    if let Ok(url) = reqwest::Url::parse(remote) {
        if !matches!(url.scheme(), "http" | "https" | "file")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || (url.scheme() == "file" && url.host_str().is_some_and(|host| host != "localhost"))
        {
            return Err(invalid("unsafe origin URL"));
        }
        return Ok(());
    }
    let before_slash = remote.split('/').next().unwrap_or(remote);
    if remote.contains("::") || before_slash.contains(':') {
        return Err(invalid("unsafe origin URL"));
    }
    Ok(())
}

fn valid_github_repo(repository: &str) -> bool {
    let mut parts = repository.split('/');
    let valid = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(name), None) if valid(owner) && valid(name))
}

fn local_command(repo: &Path, private_root: &Path) -> Command {
    let mut command = isolated_command(private_root);
    command.arg("-C").arg(repo).args([
        "--no-replace-objects",
        "--no-lazy-fetch",
        "--no-optional-locks",
    ]);
    command
}

fn isolated_command(private_root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", private_root.join("home"))
        .env("XDG_CONFIG_HOME", private_root.join("xdg"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("LC_ALL", "C")
        .process_group(0);
    command
}

fn run(command: &mut Command, timeout: Duration, operation: &str) -> Result<Vec<u8>> {
    let output = capture_with_input(command, None, timeout, GIT_OUTPUT, &Cancellation::default())
        .map_err(|_| invalid(&format!("git {operation} failed")))?;
    if output.stop.is_some() || output.code != Some(0) {
        return Err(invalid(&format!("git {operation} failed")));
    }
    Ok(output.stdout)
}

fn is_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

#[cfg(not(test))]
fn test_remote_override(remote: &str, _authenticated: bool) -> String {
    remote.to_owned()
}

#[cfg(not(test))]
fn record_command(_kind: &str, _command: &Command, _authenticated: bool) {}

#[cfg(test)]
use sha2::Digest;
#[cfg(test)]
use std::cell::RefCell;

#[cfg(test)]
#[derive(Clone, Debug)]
struct RecordedCommand {
    kind: String,
    argv: Vec<String>,
    authenticated: bool,
    authentication_key: Option<String>,
    authentication_value_sha256: Option<String>,
}

#[cfg(test)]
#[derive(Default)]
struct TestState {
    remote_override: Option<String>,
    commands: Vec<RecordedCommand>,
}

#[cfg(test)]
thread_local! {
    static TEST_STATE: RefCell<Option<TestState>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn test_remote_override(remote: &str, authenticated: bool) -> String {
    if !authenticated {
        return remote.to_owned();
    }
    TEST_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|state| state.remote_override.clone())
            .unwrap_or_else(|| remote.to_owned())
    })
}

#[cfg(test)]
fn record_command(kind: &str, command: &Command, authenticated: bool) {
    TEST_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(state) = state.as_mut() else {
            return;
        };
        let environment = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<Vec<_>>();
        let authentication_key = environment
            .iter()
            .find(|(key, _)| key == "GIT_CONFIG_KEY_0")
            .and_then(|(_, value)| value.clone());
        let authentication_value_sha256 = environment
            .iter()
            .find(|(key, _)| key == "GIT_CONFIG_VALUE_0")
            .and_then(|(_, value)| value.as_ref())
            .map(|value| format!("{:x}", sha2::Sha256::digest(value.as_bytes())));
        state.commands.push(RecordedCommand {
            kind: kind.to_owned(),
            argv: command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            authenticated,
            authentication_key,
            authentication_value_sha256,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    struct Fixture {
        root: TempDir,
        checkout: PathBuf,
        origin: PathBuf,
        head: String,
    }

    impl Fixture {
        fn new(origin_url: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            let checkout = root.path().join("checkout");
            fs::create_dir(&checkout).unwrap();
            git(&checkout, &["init", "-q", "-b", "main"]);
            fs::write(checkout.join("base"), "base").unwrap();
            fs::create_dir(checkout.join("cfg")).unwrap();
            fs::write(checkout.join("cfg/policy.json"), "{}").unwrap();
            git(&checkout, &["add", "."]);
            git(&checkout, &["commit", "-q", "-m", "base"]);
            let base = git(&checkout, &["rev-parse", "HEAD"]);
            fs::write(checkout.join("head"), "head").unwrap();
            git(&checkout, &["add", "."]);
            git(&checkout, &["commit", "-q", "-m", "head"]);
            let head = git(&checkout, &["rev-parse", "HEAD"]);
            let origin = root.path().join("origin.git");
            git(
                root.path(),
                &[
                    "init",
                    "-q",
                    "--bare",
                    "-b",
                    "main",
                    origin.to_str().unwrap(),
                ],
            );
            git(
                &checkout,
                &[
                    "push",
                    "-q",
                    origin.to_str().unwrap(),
                    &format!("{base}:refs/heads/main"),
                    &format!("{head}:refs/pull/1/head"),
                ],
            );
            git(&checkout, &["remote", "add", "origin", origin_url]);
            Self {
                root,
                checkout,
                origin,
                head,
            }
        }
    }

    struct TestScope;

    impl TestScope {
        fn enter(remote: &Path) -> Self {
            TEST_STATE.with(|state| {
                assert!(
                    state
                        .replace(Some(TestState {
                            remote_override: Some(remote.to_string_lossy().into_owned()),
                            commands: Vec::new(),
                        }))
                        .is_none()
                );
            });
            Self
        }

        fn commands(&self) -> Vec<RecordedCommand> {
            TEST_STATE.with(|state| state.borrow().as_ref().unwrap().commands.clone())
        }
    }

    impl Drop for TestScope {
        fn drop(&mut self) {
            TEST_STATE.with(|state| {
                state.replace(None);
            });
        }
    }

    fn files_contain(root: &Path, needle: &str) -> bool {
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let mut bytes = Vec::new();
                fs::File::open(path)
                    .unwrap()
                    .read_to_end(&mut bytes)
                    .unwrap();
                if String::from_utf8_lossy(&bytes).contains(needle) {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn authenticated_fetches_reuse_one_isolated_context_without_persisting_secrets() {
        let fixture = Fixture::new("https://github.com/o/r");
        let scope = TestScope::enter(&fixture.origin);
        let token = "synthetic-auth-token";
        let encoded = STANDARD.encode(format!("x-access-token:{token}"));
        let fetch =
            FetchContext::authenticated(&fixture.checkout, "o/r", "github.com/o/r", token).unwrap();
        fetch.fetch_pull("main", 1, &fixture.head).unwrap();
        assert!(fetch.config_dir_unchanged("main", "cfg").unwrap());

        let commands = scope.commands();
        let fetches: Vec<_> = commands
            .iter()
            .filter(|command| command.kind == "fetch")
            .collect();
        assert_eq!(fetches.len(), 3);
        assert!(fetches.iter().all(|command| command.authenticated));
        let expected_header = format!("Authorization: Basic {encoded}");
        let expected_digest = format!("{:x}", sha2::Sha256::digest(expected_header.as_bytes()));
        assert!(fetches.iter().all(|command| {
            command.authentication_key.as_deref()
                == Some("http.https://github.com/o/r.git.extraHeader")
                && command.authentication_value_sha256.as_deref() == Some(&expected_digest)
        }));
        assert!(
            commands
                .iter()
                .filter(|command| command.kind != "fetch")
                .all(|command| !command.authenticated
                    && command.authentication_key.is_none()
                    && command.authentication_value_sha256.is_none())
        );
        assert!(commands.iter().all(|command| {
            let argv = command.argv.join("\n");
            !argv.contains(token) && !argv.contains(&encoded) && !argv.contains("Authorization")
        }));
        assert!(!files_contain(fetch.temporary_path(), token));
        assert!(!files_contain(fetch.temporary_path(), &encoded));
    }

    #[test]
    fn authenticated_failure_is_sanitized() {
        let fixture = Fixture::new("https://github.com/o/r.git");
        let missing = fixture.root.path().join("missing.git");
        let _scope = TestScope::enter(&missing);
        let token = "synthetic-token-that-must-not-escape";
        let fetch =
            FetchContext::authenticated(&fixture.checkout, "o/r", "github.com/o/r", token).unwrap();
        let error = format!(
            "{:?}",
            fetch.fetch_pull("main", 1, &fixture.head).unwrap_err()
        );
        assert!(!error.contains(token));
        assert!(!error.contains(&STANDARD.encode(format!("x-access-token:{token}"))));
        assert_eq!(error, "Invalid(\"git fetch failed\")");
    }

    #[test]
    fn authenticated_origin_is_exact_and_matches_the_policy() {
        for origin in [
            "https://user@github.com/o/r.git",
            "https://github.com:443/o/r.git",
            "https://github.com/o/r/extra",
            "https://github.com/o/%72.git",
            "https://github.com/o/r.git?x",
            "https://github.com.evil/o/r.git",
            "git@github.com:o/r.git",
            "/tmp/r.git",
        ] {
            let fixture = Fixture::new(origin);
            assert!(
                FetchContext::authenticated(&fixture.checkout, "o/r", "github.com/o/r", "token")
                    .is_err(),
                "accepted {origin}"
            );
        }
        let fixture = Fixture::new("https://github.com/o/r");
        assert!(
            FetchContext::authenticated(&fixture.checkout, "o/r", "github.com/x/r", "token")
                .is_err()
        );
        for repository in ["../r", "o/..", "o/r/extra", "o/%72"] {
            assert!(
                FetchContext::authenticated(
                    &fixture.checkout,
                    repository,
                    &format!("github.com/{repository}"),
                    "token"
                )
                .is_err(),
                "accepted {repository}"
            );
        }
    }

    #[test]
    fn anonymous_remote_rejects_credentials_and_executable_transports() {
        for origin in [
            "https://user:password@github.com/o/r.git",
            "ext::sh -c touch canary",
            "ssh://git@github.com/o/r.git",
            "git@github.com:o/r.git",
            "git://github.com/o/r.git",
        ] {
            let fixture = Fixture::new(origin);
            assert!(
                FetchContext::anonymous(&fixture.checkout).is_err(),
                "accepted {origin}"
            );
        }
    }
}
