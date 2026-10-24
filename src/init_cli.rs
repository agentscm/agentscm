//! `agentscm init-repo`: write the agent config (MCP server, hooks, instructions) that wires
//! the current Git repository to a hub. It never writes a token; each developer sets
//! `AGENTSCM_HUB_TOKEN` themselves.
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

use agentscm::hub::forge_http::check_base_url;
use agentscm::{Error, Result};

use crate::hub_cli::{config_dir, map_hub_error};
use crate::intent_cli::{parse_hub_file, remote_urls, valid_repo};
use crate::prompt::Ui;

mod merge;
mod targets;

pub(crate) use targets::{TOKEN_VAR, claude_hook_groups};

const NEEDS_TERMINAL: &str = "agentscm init-repo asks before it writes, and there is no terminal here. \
To run it without one, pass --yes and choose with flags: --hub-url URL, --agents claude,codex,cursor,copilot,gemini";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Agent {
    Claude,
    Codex,
    Cursor,
    Copilot,
    /// Gemini CLI reads GEMINI.md only.
    Gemini,
}

const ALL: [Agent; 5] = [
    Agent::Claude,
    Agent::Codex,
    Agent::Cursor,
    Agent::Copilot,
    Agent::Gemini,
];

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Agents to configure, comma separated (default: all).
    #[arg(long, value_delimiter = ',', value_enum)]
    agents: Option<Vec<Agent>>,
    /// The hub (default: AGENTSCM_HUB_URL, then hub.json).
    #[arg(long)]
    hub_url: Option<String>,
    /// The repository as owner/name (default: from the `origin` remote). The generated URLs
    /// carry it, so a token for another repository is not used here.
    #[arg(long)]
    repo: Option<String>,
    /// Write without asking.
    #[arg(long)]
    yes: bool,
}

/// What `run_in` did, as paths relative to the repository root.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    pub(crate) written: Vec<PathBuf>,
    pub(crate) unchanged: Vec<PathBuf>,
    pub(crate) skipped: Vec<(PathBuf, String)>,
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

pub fn run(args: InitArgs) -> Result<()> {
    if !args.yes && !std::io::stdin().is_terminal() {
        return Err(invalid(NEEDS_TERMINAL));
    }
    let root = repo_root(&std::env::current_dir()?)
        .ok_or_else(|| invalid("this is not inside a Git repository (no .git found above here)"))?;
    let repo = match args.repo {
        Some(repo) if valid_repo(&repo) => Some(repo),
        Some(_) => return Err(invalid("--repo must look like owner/name")),
        None => origin_repo(&root),
    };
    match &repo {
        Some(repo) => println!("Repository guard: {repo}"),
        None => {
            println!("No repository guard (no usable origin remote; --repo owner/name sets one).")
        }
    }
    let ui = Ui { yes: args.yes };
    let agents = args.agents.as_deref();
    run_in(&root, args.hub_url.as_deref(), repo.as_deref(), agents, &ui)?;
    print_next_steps(
        agents.unwrap_or(&ALL),
        args.hub_url.as_deref(),
        repo.as_deref(),
    );
    Ok(())
}

fn repo_root(from: &Path) -> Option<PathBuf> {
    from.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// `owner/name` from the `origin` remote, only when its path has exactly those two segments
/// (a GitLab subgroup gives none).
fn origin_repo(root: &Path) -> Option<String> {
    let (_, url) = remote_urls(root)
        .into_iter()
        .find(|(remote, _)| remote == "origin")?;
    let path = match url.split_once("://") {
        Some(("ssh" | "https", rest)) => rest.split_once('/')?.1,
        Some(_) => return None,
        None => {
            url.split_once(':')
                .filter(|(host, _)| !host.contains('/'))?
                .1
        }
    };
    let path = path.trim_end_matches('/');
    let repo = path.strip_suffix(".git").unwrap_or(path);
    (repo.split('/').count() == 2 && valid_repo(repo)).then(|| repo.to_owned())
}

/// The hub named by hub.json, if there is a readable one.
fn saved_hub_url() -> Option<String> {
    let path = config_dir().ok()?.join("agentscm").join("hub.json");
    let file = parse_hub_file(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(file.url)
}

/// The hub URL without a trailing slash. It goes into committed files and, for command
/// hooks, into a shell command, so only plain URL characters are accepted.
fn hub_base(given: Option<&str>, ui: &Ui) -> Result<String> {
    let env = std::env::var("AGENTSCM_HUB_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let url = match given.map(str::to_owned).or(env).or_else(saved_hub_url) {
        Some(url) => url,
        None if ui.yes => return Err(invalid("no hub URL: pass --hub-url URL")),
        None => ui.ask("Hub URL (https://...)", ""),
    };
    let url = check_base_url(url.trim()).map_err(map_hub_error)?;
    if !url
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ":/.-_[]%~".contains(c))
    {
        return Err(invalid(
            "the hub URL has characters that cannot go in a committed hook command",
        ));
    }
    Ok(url)
}

enum Status {
    Create,
    Update,
    Unchanged,
    Skip(String),
}

struct Planned {
    path: &'static str,
    old: String,
    new: String,
    status: Status,
}

fn plan(root: &Path, hub: &str, repo: Option<&str>, agents: &[Agent]) -> Vec<Planned> {
    targets::targets(hub, repo, agents)
        .into_iter()
        .map(|target| {
            let path = root.join(target.path);
            if !stays_inside(root, &path) {
                return Planned {
                    path: target.path,
                    old: String::new(),
                    new: String::new(),
                    status: Status::Skip("it resolves to a path outside the repository".into()),
                };
            }
            let existing = match std::fs::read_to_string(&path) {
                Ok(text) => Some(text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => {
                    return Planned {
                        path: target.path,
                        old: String::new(),
                        new: String::new(),
                        status: Status::Skip("it cannot be read as text".into()),
                    };
                }
            };
            let old = existing.clone().unwrap_or_default();
            let (new, status) = match target.build(existing.as_deref()) {
                Err(why) => (String::new(), Status::Skip(why)),
                Ok(new) if existing.is_none() => (new, Status::Create),
                Ok(new) if new == old => (new, Status::Unchanged),
                Ok(new) => (new, Status::Update),
            };
            Planned {
                path: target.path,
                old,
                new,
                status,
            }
        })
        .collect()
}

/// Whether `path` (following symlinks, for the part that exists) is inside `root`: a cloned
/// repository must not be able to point a config file at the developer's other files.
fn stays_inside(root: &Path, path: &Path) -> bool {
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    let mut existing = path;
    while std::fs::symlink_metadata(existing).is_err() {
        match existing.parent() {
            Some(parent) => existing = parent,
            None => return false,
        }
    }
    std::fs::canonicalize(existing).is_ok_and(|real| real.starts_with(root))
}

/// Writes through a symlink instead of replacing it, via a temp file beside the target.
fn write_file(root: &Path, relative: &str, text: &str) -> Result<()> {
    let path = root.join(relative);
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut temp = path.clone().into_os_string();
    temp.push(".agentscm-tmp");
    let temp = PathBuf::from(temp);
    // Never write through a pre-planted temp symlink.
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    drop(file);
    // A new file gets the process's default mode; an existing one keeps its own.
    if let Ok(meta) = std::fs::metadata(&path) {
        std::fs::set_permissions(&temp, meta.permissions())?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644))?;
        }
    }
    std::fs::rename(&temp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    Ok(())
}

fn show_diff(item: &Planned) {
    let mut options = diffy::DiffOptions::new();
    options
        .set_original_filename(format!("a/{}", item.path))
        .set_modified_filename(format!("b/{}", item.path));
    println!("{}", options.create_patch(&item.old, &item.new));
}

/// Configures the repository at `root`. `repo` (already valid) guards every URL; `agents` of
/// `None` means all of them.
pub(crate) fn run_in(
    root: &Path,
    hub_url: Option<&str>,
    repo: Option<&str>,
    agents: Option<&[Agent]>,
    ui: &Ui,
) -> Result<Outcome> {
    let agents = agents.unwrap_or(&ALL);
    if agents.is_empty() {
        return Err(invalid("--agents is empty"));
    }
    let hub = hub_base(hub_url, ui)?;
    let planned = plan(root, &hub, repo, agents);
    let mut outcome = Outcome::default();
    for item in &planned {
        let (label, note) = match &item.status {
            Status::Create => ("create   ", String::new()),
            Status::Update => ("update   ", String::new()),
            Status::Unchanged => ("unchanged", String::new()),
            Status::Skip(why) => ("skipped  ", format!(" (left alone: {why})")),
        };
        println!("  {label} {}{note}", item.path);
    }
    let changed: Vec<&Planned> = planned
        .iter()
        .filter(|i| matches!(i.status, Status::Create | Status::Update))
        .collect();
    for item in &planned {
        let path = PathBuf::from(item.path);
        match &item.status {
            Status::Unchanged => outcome.unchanged.push(path),
            Status::Skip(why) => outcome.skipped.push((path, why.clone())),
            _ => {}
        }
    }
    if changed.is_empty() {
        println!("Nothing to write.");
        return Ok(outcome);
    }
    println!();
    for item in &changed {
        show_diff(item);
    }
    let question = format!(
        "Write {} file{}?",
        changed.len(),
        if changed.len() == 1 { "" } else { "s" }
    );
    if !ui.confirm(&question, true) {
        println!("Nothing written.");
        return Ok(outcome);
    }
    for item in changed {
        write_file(root, item.path, &item.new)?;
        outcome.written.push(PathBuf::from(item.path));
    }
    println!("Wrote {} file(s).", outcome.written.len());
    Ok(outcome)
}

pub(crate) fn print_next_steps(agents: &[Agent], hub: Option<&str>, repo: Option<&str>) {
    println!();
    println!("Each developer sets one environment variable; no token is stored in the repository.");
    println!("Create an agent token (the default lifetime is 8h; --ttl allows up to 30d):");
    println!(
        "  agentscm hub login --hub {} --provider github   (or gitlab)",
        hub.unwrap_or("<hub url>")
    );
    println!(
        "  agentscm hub token create --name <token name> --repo {} --ttl 30d",
        repo.unwrap_or("OWNER/NAME")
    );
    println!("  export AGENTSCM_HUB_TOKEN=<agent token>");
    println!(
        "  PowerShell: $env:AGENTSCM_HUB_TOKEN = \"<agent token>\"   (persist with: setx AGENTSCM_HUB_TOKEN \"<agent token>\")"
    );
    println!();
    println!(
        "In a repository set up with `agentscm init-repo`, MCP works through AGENTSCM_HUB_TOKEN. The hub's Connect agents page cannot verify these checkouts."
    );
    println!();
    let has = |agent| agents.contains(&agent);
    if has(Agent::Codex) || has(Agent::Cursor) || has(Agent::Copilot) {
        println!(
            "- The `agentscm` binary must be on PATH for the Codex, Cursor and Copilot hooks."
        );
    }
    if has(Agent::Codex) {
        println!("- Codex: trust the project, and review the new hooks in /hooks.");
    }
    if has(Agent::Claude) {
        println!("- Claude Code asks you to approve the project MCP server the first time.");
    }
    if has(Agent::Copilot) {
        println!("- Copilot in VS Code prompts once for the token.");
    }
    if has(Agent::Cursor) {
        println!("- Cursor: start it from a shell where AGENTSCM_HUB_TOKEN is set.");
    }
    if has(Agent::Gemini) {
        println!(
            "- Gemini: only GEMINI.md was written, with no MCP or hooks, so AgentSCM tools are not available to it yet; run `agentscm client setup --agent gemini --hub <hub url>` to add them."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const HUB: &str = "https://hub.example.test";

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_repository_is_left_alone() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim");
        std::fs::write(&target, "mine").unwrap();
        let dir = repo();
        std::os::unix::fs::symlink(&target, dir.path().join("AGENTS.md")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".cursor")).unwrap();
        let outcome = run_all(dir.path(), HUB, None);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine");
        assert!(outside.path().read_dir().unwrap().count() == 1);
        assert!(
            outcome
                .skipped
                .iter()
                .any(|(p, _)| p.ends_with("AGENTS.md"))
        );
        assert!(
            outcome
                .skipped
                .iter()
                .any(|(p, _)| p.starts_with(".cursor"))
        );
    }

    fn run_all(root: &Path, hub: &str, agents: Option<&[Agent]>) -> Outcome {
        run_in(root, Some(hub), None, agents, &Ui { yes: true }).unwrap()
    }

    fn run_guarded(root: &Path, repo: &str, agents: Option<&[Agent]>) -> Outcome {
        run_in(root, Some(HUB), Some(repo), agents, &Ui { yes: true }).unwrap()
    }

    fn origin_of(url: &str) -> Option<String> {
        let dir = repo();
        let config = format!("[remote \"origin\"]\n\turl = {url}\n");
        std::fs::write(dir.path().join(".git/config"), config).unwrap();
        origin_repo(dir.path())
    }

    #[test]
    fn the_repository_comes_from_origin_when_it_has_two_segments() {
        for (url, want) in [
            ("git@github.com:acme/widgets.git", Some("acme/widgets")),
            ("git@github.com:acme/widgets", Some("acme/widgets")),
            (
                "ssh://git@github.com/acme/widgets.git",
                Some("acme/widgets"),
            ),
            ("ssh://git@host:2222/acme/widgets", Some("acme/widgets")),
            ("https://github.com/acme/widgets.git", Some("acme/widgets")),
            ("https://github.com/acme/widgets/", Some("acme/widgets")),
            ("git@gitlab.com:group/sub/widgets.git", None),
            ("https://gitlab.com/group/sub/widgets", None),
            ("https://github.com/widgets", None),
            ("git@github.com:widgets.git", None),
            ("https://github.com/a%20b/c", None),
            ("git://github.com/acme/widgets", None),
            ("/srv/git/acme/widgets", None),
            ("../widgets", None),
        ] {
            assert_eq!(origin_of(url).as_deref(), want, "{url}");
        }
        // Only origin counts.
        let dir = repo();
        let config = "[remote \"fork\"]\n\turl = git@github.com:me/widgets.git\n";
        std::fs::write(dir.path().join(".git/config"), config).unwrap();
        assert_eq!(origin_repo(dir.path()), None);
    }

    fn urls(text: &str) -> Vec<&str> {
        text.split('"')
            .filter(|s| s.starts_with("https://"))
            .collect()
    }

    #[test]
    fn a_known_repository_guards_every_url_and_command() {
        let dir = repo();
        let root = dir.path();
        run_guarded(root, "acme/widgets", None);
        let guard = "?repo=acme/widgets";
        let settings = read(root, ".claude/settings.json");
        assert_eq!(urls(&settings).len(), 2);
        assert!(
            urls(&settings)
                .iter()
                .all(|u| u.ends_with(&format!("claude{guard}")))
        );
        for path in [
            ".mcp.json",
            ".cursor/mcp.json",
            ".vscode/mcp.json",
            ".codex/config.toml",
        ] {
            let text = read(root, path);
            assert!(text.contains(&format!("{HUB}/mcp{guard}")), "{path}");
        }
        for path in [
            ".codex/hooks.json",
            ".cursor/hooks.json",
            ".github/hooks/agentscm.json",
        ] {
            let text = read(root, path);
            let commands = text.matches("agentscm hook ").count();
            assert!(commands > 0, "{path}");
            assert_eq!(
                text.matches(&format!("--hub-url {HUB} --repo acme/widgets\""))
                    .count(),
                commands,
                "{path}"
            );
        }
        // Without a repository nothing mentions one.
        let bare = repo();
        run_all(bare.path(), HUB, None);
        for path in [
            ".claude/settings.json",
            ".mcp.json",
            ".cursor/mcp.json",
            ".cursor/hooks.json",
            ".vscode/mcp.json",
            ".codex/config.toml",
            ".codex/hooks.json",
            ".github/hooks/agentscm.json",
        ] {
            let text = read(bare.path(), path);
            assert!(
                !text.contains("?repo=") && !text.contains("--repo"),
                "{path}"
            );
        }
        // Adding the guard later updates the files in place.
        run_guarded(bare.path(), "acme/widgets", None);
        assert_eq!(read(bare.path(), ".mcp.json"), read(root, ".mcp.json"));
        assert_eq!(
            read(bare.path(), ".claude/settings.json"),
            read(root, ".claude/settings.json")
        );
    }

    #[test]
    fn cursor_makes_no_pre_edit_call_and_an_earlier_one_is_removed() {
        let dir = repo();
        let root = dir.path();
        run_all(root, HUB, Some(&[Agent::Cursor]));
        let hooks = json_at(root, ".cursor/hooks.json");
        assert!(hooks["hooks"].get("preToolUse").is_none());
        assert!(hooks["hooks"]["postToolUse"].is_array());

        let old = "agentscm hook cursor --hub-url https://hub.example.test";
        std::fs::write(
            root.join(".cursor/hooks.json"),
            json!({"version": 1, "hooks": {
                "preToolUse": [
                    {"command": old, "matcher": "Write|Delete", "timeout": 5},
                    {"command": "mine", "timeout": 5}],
                "sessionStart": [{"command": old, "timeout": 5}]
            }})
            .to_string(),
        )
        .unwrap();
        run_all(root, HUB, Some(&[Agent::Cursor]));
        let hooks = json_at(root, ".cursor/hooks.json");
        assert_eq!(
            hooks["hooks"]["preToolUse"],
            json!([{"command": "mine", "timeout": 5}])
        );
        assert_eq!(hooks["hooks"]["sessionStart"].as_array().unwrap().len(), 1);

        // With only our entry there, the event goes away.
        std::fs::write(
            root.join(".cursor/hooks.json"),
            json!({"version": 1, "hooks": {"preToolUse": [{"command": old}]}}).to_string(),
        )
        .unwrap();
        run_all(root, HUB, Some(&[Agent::Cursor]));
        assert!(
            json_at(root, ".cursor/hooks.json")["hooks"]
                .get("preToolUse")
                .is_none()
        );
    }

    fn read(root: &Path, path: &str) -> String {
        std::fs::read_to_string(root.join(path)).unwrap()
    }

    fn json_at(root: &Path, path: &str) -> Value {
        serde_json::from_str(&read(root, path)).unwrap()
    }

    #[test]
    fn a_second_run_changes_nothing() {
        let dir = repo();
        let first = run_all(dir.path(), HUB, None);
        assert_eq!(first.written.len(), 13);
        let second = run_all(dir.path(), HUB, None);
        assert!(second.written.is_empty());
        assert_eq!(second.unchanged.len(), 13);
        assert!(second.skipped.is_empty());
    }

    #[test]
    fn claude_only_writes_claude_files_and_the_shared_instructions() {
        let dir = repo();
        let out = run_all(dir.path(), HUB, Some(&[Agent::Claude]));
        let mut written: Vec<String> = out
            .written
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        written.sort();
        assert_eq!(
            written,
            [
                ".claude/settings.json",
                ".mcp.json",
                "AGENTS.md",
                "CLAUDE.md"
            ]
        );
        assert!(!dir.path().join(".codex").exists());
    }

    #[test]
    fn user_settings_survive_and_a_new_hub_updates_in_place() {
        let dir = repo();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            root.join(".claude/settings.json"),
            r#"{"permissions":{"allow":["Bash(ls)"]},"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"mine"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"other":{"type":"stdio","command":"x"}}}"#,
        )
        .unwrap();
        run_all(root, HUB, Some(&[Agent::Claude]));
        run_all(root, "https://other.example.test", Some(&[Agent::Claude]));
        let settings = json_at(root, ".claude/settings.json");
        assert_eq!(settings["permissions"], json!({"allow": ["Bash(ls)"]}));
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["matcher"], "Bash");
        assert_eq!(
            pre[1]["hooks"][0]["url"],
            "https://other.example.test/v1/hub/hook/claude"
        );
        let mcp = json_at(root, ".mcp.json");
        assert_eq!(mcp["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            mcp["mcpServers"]["agentscm"]["url"],
            "https://other.example.test/mcp"
        );
        assert!(!read(root, ".mcp.json").contains(HUB));
    }

    #[test]
    fn unparsable_files_are_left_alone_and_the_rest_continue() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join(".mcp.json"), "{ // comment\n}").unwrap();
        let out = run_all(root, HUB, Some(&[Agent::Claude]));
        assert_eq!(read(root, ".mcp.json"), "{ // comment\n}");
        assert_eq!(out.skipped.len(), 1);
        assert!(root.join(".claude/settings.json").exists());
    }

    #[test]
    fn markers_are_replaced_and_a_foreign_codex_server_is_skipped() {
        let dir = repo();
        let root = dir.path();
        run_all(root, HUB, Some(&[Agent::Codex]));
        run_all(root, "https://other.example.test", Some(&[Agent::Codex]));
        let toml = read(root, ".codex/config.toml");
        assert_eq!(toml.matches("[mcp_servers.agentscm]").count(), 1);
        assert!(toml.contains("https://other.example.test/mcp"));
        assert_eq!(read(root, "AGENTS.md").matches("agentscm:begin").count(), 1);

        std::fs::write(
            root.join(".codex/config.toml"),
            "[mcp_servers.agentscm]\nurl = \"x\"\n",
        )
        .unwrap();
        let out = run_all(root, HUB, Some(&[Agent::Codex]));
        assert_eq!(
            read(root, ".codex/config.toml"),
            "[mcp_servers.agentscm]\nurl = \"x\"\n"
        );
        assert_eq!(out.skipped.len(), 1);
    }

    #[test]
    fn no_token_is_written() {
        let dir = repo();
        run_all(dir.path(), HUB, None);
        for path in [
            ".mcp.json",
            ".cursor/mcp.json",
            ".vscode/mcp.json",
            ".codex/config.toml",
        ] {
            let text = read(dir.path(), path);
            assert!(
                !text.contains("Bearer ey") && !text.contains("sk-"),
                "{path}"
            );
        }
    }

    #[test]
    fn hub_urls_are_checked() {
        let ui = Ui { yes: true };
        assert_eq!(
            hub_base(Some("https://h.example.test/"), &ui).unwrap(),
            "https://h.example.test"
        );
        assert!(hub_base(Some("http://h.example.test"), &ui).is_err());
        assert!(hub_base(Some("https://h.example.test/a'b"), &ui).is_err());
        assert!(hub_base(Some("https://h.example.test/a;b"), &ui).is_err());
    }

    #[test]
    fn the_repository_is_the_nearest_ancestor_with_git() {
        let dir = repo();
        let deep = dir.path().join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(repo_root(&deep).unwrap(), dir.path());
    }
}
