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
To run it without one, pass --yes and choose with flags: --agents claude,codex,cursor,copilot,gemini (--hub-url URL fills in the setup command it prints, not the files)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Agent {
    Claude,
    Codex,
    Cursor,
    Copilot,
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
    /// Agents to write instructions for, comma separated (default: all).
    #[arg(long, value_delimiter = ',', value_enum)]
    agents: Option<Vec<Agent>>,
    /// The hub for the setup command printed at the end (default: AGENTSCM_HUB_URL, then
    /// hub.json). The instruction files never name a hub.
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
    let outcome = run_in(&root, agents, &ui)?;
    print_next_steps(hub.as_deref(), &outcome.written);
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

/// The hub URL without a trailing slash, for the printed setup command. A bad `--hub-url` is an
/// error; a bad AGENTSCM_HUB_URL or hub.json only warns, because the command then just omits it.
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
            eprintln!("Not using the saved hub URL ({error}); the setup command leaves it out.")
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
    /// An `@AGENTS.md` import line rather than instructions.
    import: bool,
    old: String,
    new: String,
    status: Status,
}

/// The Supported skills section from the repository's lock file; empty when it is absent or
/// does not parse (the instruction files then stay as they were).
fn skills_section(root: &Path) -> String {
    use agentscm::skill_workflow::supported::{LOCK_PATH, SkillLock, instruction_section};
    // Same containment rule as the files init-repo writes.
    let path = root.join(LOCK_PATH);
    if !stays_inside(root, &path) {
        return String::new();
    }
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| SkillLock::parse(&bytes).ok())
        .map(|lock| instruction_section(&lock))
        .unwrap_or_default()
}

fn plan(root: &Path, agents: &[Agent]) -> Vec<Planned> {
    targets::targets(agents, &skills_section(root))
        .into_iter()
        .map(|target| {
            let import = matches!(target.content, targets::Content::Import(_));
            let path = root.join(target.path);
            if !stays_inside(root, &path) {
                return Planned {
                    path: target.path,
                    import,
                    old: String::new(),
                    new: String::new(),
                    status: Status::Skip("it resolves to a path outside the repository".into()),
                };
            }
            // A CLAUDE.md that is a link to AGENTS.md would make AGENTS.md import itself.
            if import && same_file(&path, &root.join("AGENTS.md")) {
                return Planned {
                    path: target.path,
                    import,
                    old: String::new(),
                    new: String::new(),
                    status: Status::Skip(
                        "it is the same file as AGENTS.md, which holds the instructions".into(),
                    ),
                };
            }
            let existing = match std::fs::read_to_string(&path) {
                Ok(text) => Some(text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => {
                    return Planned {
                        path: target.path,
                        import,
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
                import,
                old,
                new,
                status,
            }
        })
        .collect()
}

fn same_file(left: &Path, right: &Path) -> bool {
    matches!(
        (std::fs::canonicalize(left), std::fs::canonicalize(right)),
        (Ok(left), Ok(right)) if left == right
    )
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
pub(crate) fn run_in(root: &Path, agents: Option<&[Agent]>, ui: &Ui) -> Result<Outcome> {
    run_asking(root, agents, ui, true)
}

/// `run_in`, with what an empty answer to its question means.
fn run_asking(
    root: &Path,
    agents: Option<&[Agent]>,
    ui: &Ui,
    default_yes: bool,
) -> Result<Outcome> {
    let agents = agents.unwrap_or(&ALL);
    if agents.is_empty() {
        return Err(invalid("--agents is empty"));
    }
    let planned = plan(root, agents);
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
    if !ui.confirm(&question, default_yes) {
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

/// The instruction-file agents for the coding agents a checkout was set up for (their
/// `client setup` names). Codex reads `AGENTS.md` itself, so it stands for `AGENTS.md` alone.
fn instruction_agents(names: &[String]) -> Vec<Agent> {
    let mut agents: Vec<Agent> = names
        .iter()
        .filter_map(|name| Agent::from_str(name, true).ok())
        .collect();
    if agents.is_empty() {
        agents.push(Agent::Codex);
    }
    agents
}

/// What is missing or outdated in the instruction files `init-repo` would write for the
/// checkout's agents, one line per file; empty when every file is current.
pub(crate) fn instructions_state(root: &Path, names: &[String]) -> Vec<String> {
    plan(root, &instruction_agents(names))
        .iter()
        .filter_map(|item| {
            let why = match (&item.status, item.import) {
                (Status::Unchanged, _) => return None,
                // A CLAUDE.md linked to AGENTS.md already reads the instructions.
                (Status::Skip(_), true)
                    if same_file(&root.join(item.path), &root.join("AGENTS.md")) =>
                {
                    return None;
                }
                (Status::Create, _) => "is missing".to_owned(),
                (Status::Update, true) => "does not import AGENTS.md".to_owned(),
                (Status::Update, false) if item.old.contains(merge::BEGIN_PREFIX) => {
                    "has an outdated AgentSCM block".to_owned()
                }
                (Status::Update, false) => "has no current AgentSCM block".to_owned(),
                (Status::Skip(why), _) => format!("cannot be updated ({why})"),
            };
            Some(format!("{} {why}", item.path))
        })
        .collect()
}

/// After `client setup` or `client connect`: the agent instructions are part of onboarding, but
/// they change committed files, so they are previewed and written only on a yes in a terminal.
pub(crate) fn offer_after_setup(root: &Path, names: &[String]) {
    let ui = Ui { yes: false };
    offer(root, names, std::io::stdin().is_terminal().then_some(&ui));
}

/// `offer_after_setup` with the terminal to ask in, if any. Without one, after a no, or when
/// writing fails, setup still succeeds and this says so; it returns what was written.
fn offer(root: &Path, names: &[String], ui: Option<&Ui>) -> Vec<PathBuf> {
    let state = instructions_state(root, names);
    if state.is_empty() {
        return Vec::new();
    }
    println!();
    println!("This checkout's agent instructions are not current:");
    for line in &state {
        println!("  {line}");
    }
    let later = "Run `agentscm init-repo` here to preview and write them.";
    let Some(ui) = ui else {
        println!("There is no terminal to ask in, so nothing was changed. {later}");
        return Vec::new();
    };
    println!("They tell every agent how to coordinate. Review the change; it is a committed file:");
    // Asked in the middle of another command, so only an explicit yes writes.
    match run_asking(root, Some(&instruction_agents(names)), ui, false) {
        Ok(outcome) if !outcome.written.is_empty() => {
            println!(
                "Commit the instruction file{} written above.",
                if outcome.written.len() == 1 { "" } else { "s" }
            );
            outcome.written
        }
        Ok(_) => {
            println!("Setup is done without them. {later}");
            Vec::new()
        }
        Err(error) => {
            println!("Couldn't write them all ({error}); setup is done without them. {later}");
            Vec::new()
        }
    }
}

pub(crate) fn print_next_steps(hub: Option<&str>, written: &[PathBuf]) {
    println!();
    if !written.is_empty() {
        println!(
            "Commit the {} file{} written above. They hold instructions only: no MCP server, hook or token.",
            written.len(),
            if written.len() == 1 { "" } else { "s" }
        );
    }
    println!("Each developer sets up their own machine once:");
    match hub {
        Some(hub) => println!("  agentscm quickstart --hub {hub}"),
        None => println!("  agentscm quickstart"),
    }
    println!(
        "It signs in to their Hub and sets up their agents for every checkout; it writes nothing to the repository."
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
        run_in(root, agents, &Ui { yes: true }).unwrap()
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

    #[cfg(unix)]
    #[test]
    fn a_claude_md_linked_to_agents_md_does_not_import_itself() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("AGENTS.md"), "# Team\n").unwrap();
        std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
        let outcome = run_all(root, None);
        assert!(!read(root, "AGENTS.md").contains("@AGENTS.md"));
        assert!(read(root, "AGENTS.md").contains("agentscm:begin"));
        assert!(outcome.skipped.iter().any(|(p, _)| p == "CLAUDE.md"));
        assert!(
            std::fs::symlink_metadata(root.join("CLAUDE.md"))
                .unwrap()
                .file_type()
                .is_symlink()
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

    fn names(agents: &[&str]) -> Vec<String> {
        agents.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn setup_offers_the_block_and_keeps_the_rest_of_agents_md() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("AGENTS.md"), "# Team\n\nOur rules.\n").unwrap();
        let claude = names(&["claude"]);
        assert_eq!(
            instructions_state(root, &claude),
            [
                "CLAUDE.md is missing",
                "AGENTS.md has no current AgentSCM block"
            ]
        );
        // No terminal and a declined prompt both change nothing.
        assert!(offer(root, &claude, None).is_empty());
        assert!(offer(root, &claude, Some(&Ui { yes: false })).is_empty());
        assert_eq!(read(root, "AGENTS.md"), "# Team\n\nOur rules.\n");
        assert!(!root.join("CLAUDE.md").exists());
        let written = offer(root, &claude, Some(&Ui { yes: true }));
        assert_eq!(written.len(), 2, "{written:?}");
        let agents = read(root, "AGENTS.md");
        assert!(agents.starts_with("# Team\n\nOur rules.\n\n<!-- agentscm:begin -->\n"));
        assert!(agentscm::hub::agent_rules::carries_all(&agents));
        assert_eq!(read(root, "CLAUDE.md"), "@AGENTS.md\n");
        // A repeat finds everything current and writes nothing.
        assert!(instructions_state(root, &claude).is_empty());
        assert!(offer(root, &claude, Some(&Ui { yes: true })).is_empty());
        assert_eq!(read(root, "AGENTS.md"), agents);
    }

    #[test]
    fn an_outdated_block_is_replaced_and_the_text_around_it_kept() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(
            root.join("AGENTS.md"),
            "# Mine\n\n<!-- agentscm:begin -->\nold rules\n<!-- agentscm:end -->\n\nafter\n",
        )
        .unwrap();
        // A checkout set up for no named agent still gets AGENTS.md itself.
        assert_eq!(
            instructions_state(root, &[]),
            ["AGENTS.md has an outdated AgentSCM block"]
        );
        offer(root, &[], Some(&Ui { yes: true }));
        let text = read(root, "AGENTS.md");
        assert!(
            text.starts_with("# Mine\n\n<!-- agentscm:begin -->\n"),
            "{text}"
        );
        assert!(text.ends_with("<!-- agentscm:end -->\n\nafter\n"), "{text}");
        assert!(!text.contains("old rules"));
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_md_linked_to_agents_md_counts_as_current() {
        let dir = repo();
        let root = dir.path();
        std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "# Team\n").unwrap();
        let claude = names(&["claude"]);
        assert_eq!(
            instructions_state(root, &claude),
            ["AGENTS.md has no current AgentSCM block"]
        );
        offer(root, &claude, Some(&Ui { yes: true }));
        assert!(instructions_state(root, &claude).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_checkout_is_reported_and_setup_goes_on() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo();
        let root = dir.path();
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o555)).unwrap();
        // A superuser writes anyway, so there is nothing to show there.
        if std::fs::write(root.join("probe"), "").is_ok() {
            return;
        }
        let written = offer(root, &[], Some(&Ui { yes: true }));
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(written.is_empty());
        assert!(!root.join("AGENTS.md").exists());
        assert_eq!(instructions_state(root, &[]), ["AGENTS.md is missing"]);
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
