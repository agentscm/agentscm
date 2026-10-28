//! What the machine and the checkout already have, found without changing anything.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::intent_cli::valid_repo;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DockerDaemon {
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) endpoint: String,
}

fn docker_arch(value: &str) -> Option<&'static str> {
    match value {
        "amd64" | "x86_64" => Some("amd64"),
        "arm64" | "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// The daemon platform and endpoint used by the active Docker context. A local Hub publishes a
/// loopback port on this machine, so remote SSH/TCP contexts are deliberately not accepted.
pub(crate) fn docker_daemon() -> Result<DockerDaemon, String> {
    let docker = which("docker").ok_or("Docker was not found on PATH.")?;
    let platform = capture(
        &docker,
        &["info", "--format", "{{.OSType}}/{{.Architecture}}"],
        None,
        QUICK,
    )
    .ok_or("Docker did not report its daemon platform.")?;
    let (os, arch) = platform
        .split_once('/')
        .ok_or("Docker reported an invalid daemon platform.")?;
    // Docker gives DOCKER_CONTEXT precedence over DOCKER_HOST. Inspect that context's
    // endpoint rather than accepting a local-looking host override for a remote daemon.
    let context_override =
        std::env::var_os("DOCKER_CONTEXT").is_some_and(|value| !value.is_empty());
    let endpoint = (!context_override)
        .then(|| {
            std::env::var("DOCKER_HOST")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .flatten()
        .or_else(|| {
            capture(
                &docker,
                &[
                    "context",
                    "inspect",
                    "--format",
                    "{{(index .Endpoints \"docker\").Host}}",
                ],
                None,
                QUICK,
            )
        })
        .ok_or("Docker did not report its active context endpoint.")?;
    if !(endpoint.starts_with("unix://") || endpoint.starts_with("npipe://")) {
        return Err(format!(
            "The active Docker context uses {endpoint}; a local Hub requires a local Docker context so 127.0.0.1 reaches this machine."
        ));
    }
    Ok(DockerDaemon {
        os: os.to_owned(),
        arch: docker_arch(arch)
            .ok_or_else(|| format!("Docker reported unsupported architecture {arch}."))?
            .to_owned(),
        endpoint,
    })
}

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
    command.args(args);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = super::process::run(&mut command, limit).ok()?;
    (output.success && !output.truncated && !output.incomplete && output.stdout_valid_utf8)
        .then_some(output.stdout)
}

const QUICK: Duration = Duration::from_secs(10);

pub(crate) struct Detected {
    pub(crate) repo_root: Option<PathBuf>,
    /// The origin URL as Git reports it, parsed.
    pub(crate) remote: Option<Remote>,
    pub(crate) docker: bool,
    /// Coding agents installed on this computer, in setup order.
    #[cfg(unix)]
    pub(crate) agents: Vec<crate::client_setup::Agent>,
}

pub(crate) fn detect(cwd: &Path) -> Detected {
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
    let docker = which("docker").is_some_and(|docker| {
        capture(&docker, &["version"], None, QUICK).is_some()
            && capture(&docker, &["compose", "version"], None, QUICK).is_some()
    });
    Detected {
        repo_root,
        remote,
        docker,
        #[cfg(unix)]
        agents: installed_agents(&|name| which(name).is_some(), &|path| path.is_dir()),
    }
}

/// Each agent found by its program on PATH or its user configuration directory. Claude Code and
/// Codex are set up through their own CLI's plugin commands, so only the program counts for them.
#[cfg(unix)]
fn installed_agents(
    on_path: &dyn Fn(&str) -> bool,
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<crate::client_setup::Agent> {
    use crate::client_setup::Agent;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = |variable: Option<&str>, default: &str| {
        variable
            .and_then(std::env::var_os)
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(default)))
            .is_some_and(|path| exists(&path))
    };
    [
        (Agent::Claude, on_path("claude")),
        (Agent::Codex, on_path("codex")),
        (
            Agent::Cursor,
            on_path("cursor") || on_path("cursor-agent") || dir(None, ".cursor"),
        ),
        (
            Agent::Copilot,
            on_path("copilot") || dir(Some("COPILOT_HOME"), ".copilot"),
        ),
        (Agent::Gemini, on_path("gemini") || dir(None, ".gemini")),
    ]
    .into_iter()
    .filter_map(|(agent, found)| found.then_some(agent))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn agents_are_found_by_program_or_config_directory() {
        use crate::client_setup::Agent;
        assert!(installed_agents(&|_| false, &|_| false).is_empty());
        // A config directory alone is not enough for agents set up through their CLI.
        let dirs = installed_agents(&|_| false, &|_| true);
        assert_eq!(dirs, [Agent::Cursor, Agent::Copilot, Agent::Gemini]);
        let programs = installed_agents(&|name| matches!(name, "claude" | "codex"), &|_| false);
        assert_eq!(programs, [Agent::Claude, Agent::Codex]);
    }

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

    #[test]
    fn normalizes_supported_docker_architectures() {
        assert_eq!(docker_arch("aarch64"), Some("arm64"));
        assert_eq!(docker_arch("arm64"), Some("arm64"));
        assert_eq!(docker_arch("x86_64"), Some("amd64"));
        assert_eq!(docker_arch("amd64"), Some("amd64"));
        assert_eq!(docker_arch("armv7"), None);
    }

    #[cfg(unix)]
    #[test]
    fn capture_drains_output_larger_than_a_pipe_while_the_command_runs() {
        let output = capture(
            Path::new("/bin/sh"),
            &["-c", "head -c 131072 /dev/zero | tr '\\000' x"],
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.len(), 131072);
        assert!(output.bytes().all(|byte| byte == b'x'));
    }

    #[cfg(unix)]
    #[test]
    fn capture_cleans_up_an_inherited_pipe_after_leader_exit() {
        let started = std::time::Instant::now();
        let output = capture(
            Path::new("/bin/sh"),
            &["-c", "sleep 60 & printf complete; exit 0"],
            None,
            Duration::from_secs(1),
        );
        assert_eq!(output.as_deref(), Some("complete"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn capture_rejects_truncation_without_using_a_text_marker() {
        assert_eq!(
            capture(
                Path::new("/bin/sh"),
                &["-c", "head -c 524288 /dev/zero"],
                None,
                Duration::from_secs(5),
            ),
            None
        );
        assert_eq!(
            capture(
                Path::new("/bin/sh"),
                &["-c", "printf '[output truncated]'"],
                None,
                Duration::from_secs(5),
            )
            .as_deref(),
            Some("[output truncated]")
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_preserves_rejection_of_invalid_utf8() {
        assert_eq!(
            capture(
                Path::new("/bin/sh"),
                &["-c", "printf '\\377'"],
                None,
                Duration::from_secs(5),
            ),
            None
        );
    }
}
