//! What the machine and the checkout already have, found without changing anything.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::intent_cli::valid_repo;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Forge {
    Github,
    Gitlab,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Remote {
    pub(crate) forge: Forge,
    pub(crate) host: String,
    /// `owner/name` (GitLab: the full group path).
    pub(crate) repo: String,
}

impl Remote {
    pub(crate) fn provider(&self) -> Option<&'static str> {
        match self.forge {
            Forge::Github => Some("github"),
            Forge::Gitlab => Some("gitlab"),
            Forge::Other => None,
        }
    }
}

/// Understands `git@host:owner/name.git`, `https://host/owner/name(.git)` and
/// `ssh://git@host[:port]/owner/name(.git)`.
pub(crate) fn parse_remote(url: &str) -> Option<Remote> {
    let url = url.trim();
    let (host, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => url.split_once(':')?,
    };
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next()?.to_ascii_lowercase();
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let forge = match host.as_str() {
        "github.com" => Forge::Github,
        "gitlab.com" => Forge::Gitlab,
        _ => Forge::Other,
    };
    let segments = path.split('/').count();
    if host.is_empty() || !valid_repo(path) || (forge != Forge::Gitlab && segments != 2) {
        return None;
    }
    Some(Remote {
        forge,
        host,
        repo: path.to_owned(),
    })
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
}

/// The first executable called `name` on `PATH`.
pub(crate) fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .find(|path| path.metadata().is_ok_and(|m| is_executable(&m)))
}

/// Standard output of a command that succeeds within `limit`; `None` otherwise.
pub(crate) fn capture(
    program: &Path,
    args: &[&str],
    dir: Option<&Path>,
    limit: Duration,
) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return status.success().then(|| out.trim().to_owned());
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

const QUICK: Duration = Duration::from_secs(10);

pub(crate) struct Detected {
    pub(crate) git: Option<(PathBuf, String)>,
    pub(crate) repo_root: Option<PathBuf>,
    /// The origin URL as Git reports it, parsed.
    pub(crate) remote: Option<Remote>,
    /// More than one registered Git worktree (including the current checkout).
    pub(crate) worktrees: bool,
    pub(crate) docker: bool,
    pub(crate) claude: bool,
}

pub(crate) fn detect(cwd: &Path, home: &Path) -> Detected {
    let git = which("git").and_then(|path| {
        let version = capture(&path, &["--version"], None, QUICK)?;
        Some((path, version))
    });
    let repo_root = git.as_ref().and_then(|(git, _)| {
        capture(git, &["rev-parse", "--show-toplevel"], Some(cwd), QUICK).map(PathBuf::from)
    });
    let remote = git
        .as_ref()
        .zip(repo_root.as_ref())
        .and_then(|((git, _), root)| {
            let url = capture(git, &["remote", "get-url", "origin"], Some(root), QUICK)?;
            parse_remote(&url)
        });
    let worktrees = git
        .as_ref()
        .zip(repo_root.as_ref())
        .is_some_and(|((git, _), root)| {
            capture(git, &["worktree", "list", "--porcelain"], Some(root), QUICK).is_some_and(
                |text| {
                    text.lines()
                        .filter(|line| line.starts_with("worktree "))
                        .count()
                        > 1
                },
            )
        });
    let docker = which("docker").is_some_and(|docker| {
        capture(&docker, &["version"], None, QUICK).is_some()
            && capture(&docker, &["compose", "version"], None, QUICK).is_some()
    });
    let claude = home.join(".claude").is_dir() || which("claude").is_some();
    Detected {
        git,
        repo_root,
        remote,
        worktrees,
        docker,
        claude,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(url: &str) -> Option<(Forge, String, String)> {
        parse_remote(url).map(|r| (r.forge, r.host, r.repo))
    }

    #[test]
    fn parses_scp_https_and_ssh_remotes() {
        let github = |repo: &str| Some((Forge::Github, "github.com".to_owned(), repo.to_owned()));
        assert_eq!(
            remote("git@github.com:acme/widgets.git"),
            github("acme/widgets")
        );
        assert_eq!(
            remote("https://github.com/acme/widgets"),
            github("acme/widgets")
        );
        assert_eq!(
            remote("https://github.com/acme/widgets.git/"),
            github("acme/widgets")
        );
        assert_eq!(
            remote("https://user:tok@github.com/acme/widgets.git"),
            github("acme/widgets")
        );
        assert_eq!(
            remote("ssh://git@github.com:22/acme/widgets.git"),
            github("acme/widgets")
        );
        assert_eq!(
            remote("git@gitlab.com:group/sub/proj.git"),
            Some((Forge::Gitlab, "gitlab.com".into(), "group/sub/proj".into()))
        );
        assert_eq!(
            remote("https://git.example.org/team/app.git"),
            Some((Forge::Other, "git.example.org".into(), "team/app".into()))
        );
        for bad in [
            "",
            "/srv/git/repo.git",
            "https://github.com/solo",
            "https://github.com/a/b/c",
            "github.com",
        ] {
            assert_eq!(remote(bad), None, "{bad}");
        }
    }
}
