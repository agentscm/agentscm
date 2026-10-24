//! `agentscm init-repo`: write the agent instruction files (AGENTS.md and the files that
//! point each agent at it) for the current Git repository. It writes no MCP server, hook or
//! token: each developer connects their own checkout with `agentscm client setup`.
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

use agentscm::hub::forge_http::check_base_url;
use agentscm::{Error, Result};

use crate::hub_cli::{config_dir, map_hub_error};
use crate::intent_cli::parse_hub_file;
use crate::prompt::Ui;

mod merge;
mod targets;

const NEEDS_TERMINAL: &str = "agentscm init-repo asks before it writes, and there is no terminal here. \
To run it without one, pass --yes and choose with flags: --agents claude,codex,cursor,copilot,gemini (--hub-url URL fills in the setup command it prints)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Agent {
    Claude,
    Codex,
    Cursor,
    Copilot,
    Gemini,
}

impl Agent {
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Copilot => "copilot",
            Self::Gemini => "gemini",
        }
    }
}

pub(crate) const ALL: [Agent; 5] = [
    Agent::Claude,
    Agent::Codex,
    Agent::Cursor,
    Agent::Copilot,
    Agent::Gemini,
];

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Agents to write instructions for, comma separated (default: all).
    #[arg(long, value_delimiter = ',', value_enum)]
    agents: Option<Vec<Agent>>,
    /// The hub, for the setup command printed at the end (default: AGENTSCM_HUB_URL, then
    /// hub.json). It is not written to any file.
    #[arg(long)]
    hub_url: Option<String>,
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
    let hub = hub_base(args.hub_url.as_deref())?;
    let ui = Ui { yes: args.yes };
    let agents = args.agents.as_deref();
    let outcome = run_in(&root, agents, hub.as_deref(), &ui)?;
    print_next_steps(agents.unwrap_or(&ALL), hub.as_deref(), &outcome.written);
    Ok(())
}

fn repo_root(from: &Path) -> Option<PathBuf> {
    from.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The hub named by hub.json, if there is a readable one.
fn saved_hub_url() -> Option<String> {
    let path = config_dir().ok()?.join("agentscm").join("hub.json");
    let file = parse_hub_file(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(file.url)
}

/// The hub URL without a trailing slash, for the setup command in the instruction files and
/// the next steps. A bad `--hub-url` is an error; a bad AGENTSCM_HUB_URL or hub.json only
/// warns, because the command then just keeps its placeholder.
fn hub_base(given: Option<&str>) -> Result<Option<String>> {
    if let Some(url) = given {
        return checked_hub(url).map(Some);
    }
    let env = std::env::var("AGENTSCM_HUB_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let Some(url) = env.or_else(saved_hub_url) else {
        return Ok(None);
    };
    Ok(checked_hub(&url)
        .inspect_err(|error| {
            eprintln!(
                "Not using the saved hub URL ({error}); the setup command keeps {}.",
                targets::HUB_PLACEHOLDER
            )
        })
        .ok())
}

/// Only plain URL characters, since the URL goes into a shell command.
fn checked_hub(url: &str) -> Result<String> {
    let url = check_base_url(url.trim()).map_err(map_hub_error)?;
    if !url
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ":/.-_[]%~".contains(c))
    {
        return Err(invalid(
            "the hub URL has characters that cannot go in a shell command",
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

fn plan(root: &Path, agents: &[Agent], hub: Option<&str>) -> Vec<Planned> {
    targets::targets(agents, hub)
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

/// Writes the instruction files at `root`; `agents` of `None` means all of them.
pub(crate) fn run_in(
    root: &Path,
    agents: Option<&[Agent]>,
    hub: Option<&str>,
    ui: &Ui,
) -> Result<Outcome> {
    let agents = agents.unwrap_or(&ALL);
    if agents.is_empty() {
        return Err(invalid("--agents is empty"));
    }
    let planned = plan(root, agents, hub);
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

pub(crate) fn print_next_steps(agents: &[Agent], hub: Option<&str>, written: &[PathBuf]) {
    println!();
    if !written.is_empty() {
        println!(
            "Commit the {} file{} written above. They hold instructions only: no MCP server, hook or token.",
            written.len(),
            if written.len() == 1 { "" } else { "s" }
        );
    }
    println!("Each developer connects their own checkout once for each agent they use:");
    for agent in agents {
        println!("  {}", targets::setup_command(*agent, hub));
    }
    println!(
        "It signs in to the hub and adds that machine's MCP server and hooks; the hub's Connect agents page then verifies them."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        dir
    }

    fn run_all(root: &Path, agents: Option<&[Agent]>) -> Outcome {
        run_in(root, agents, None, &Ui { yes: true }).unwrap()
    }

    fn read(root: &Path, path: &str) -> String {
        std::fs::read_to_string(root.join(path)).unwrap()
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
        let outcome = run_all(dir.path(), None);
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

    #[test]
    fn only_instructions_are_written_and_a_second_run_changes_nothing() {
        let dir = repo();
        let root = dir.path();
        let first = run_all(root, None);
        assert_eq!(first.written.len(), 5);
        for path in [
            ".mcp.json",
            ".claude",
            ".codex",
            ".cursor/mcp.json",
            ".cursor/hooks.json",
            ".vscode",
            ".github/hooks",
        ] {
            assert!(!root.join(path).exists(), "{path}");
        }
        let second = run_all(root, None);
        assert!(second.written.is_empty());
        assert_eq!(second.unchanged.len(), 5);
        assert!(second.skipped.is_empty());
        assert_eq!(read(root, "AGENTS.md").matches("agentscm:begin").count(), 1);
    }

    #[test]
    fn claude_only_writes_its_import_and_the_shared_instructions() {
        let dir = repo();
        let out = run_all(dir.path(), Some(&[Agent::Claude]));
        let mut written: Vec<String> = out
            .written
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        written.sort();
        assert_eq!(written, ["AGENTS.md", "CLAUDE.md"]);
    }

    #[test]
    fn user_text_survives_and_unbalanced_markers_are_left_alone() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("CLAUDE.md"), "# Mine\n").unwrap();
        run_all(root, Some(&[Agent::Claude]));
        assert_eq!(read(root, "CLAUDE.md"), "# Mine\n@AGENTS.md\n");
        std::fs::write(root.join("AGENTS.md"), "<!-- agentscm:begin -->\n").unwrap();
        let out = run_all(root, Some(&[Agent::Claude]));
        assert_eq!(read(root, "AGENTS.md"), "<!-- agentscm:begin -->\n");
        assert_eq!(out.skipped.len(), 1);
    }

    #[test]
    fn hub_urls_are_checked() {
        assert_eq!(
            hub_base(Some("https://h.example.test/"))
                .unwrap()
                .as_deref(),
            Some("https://h.example.test")
        );
        assert!(hub_base(Some("http://h.example.test")).is_err());
        assert!(hub_base(Some("https://h.example.test/a'b")).is_err());
        assert!(hub_base(Some("https://h.example.test/a;b")).is_err());
    }

    #[test]
    fn the_repository_is_the_nearest_ancestor_with_git() {
        let dir = repo();
        let deep = dir.path().join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(repo_root(&deep).unwrap(), dir.path());
    }
}
