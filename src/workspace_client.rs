//! Local, checkout-independent adapters for workspace snapshot timelines.
//!
//! The stdio server and completion hooks receive their filesystem authority at process startup.
//! Tool calls can select retained objects, but cannot select a repository, workspace, command,
//! executable, model, or data directory.

mod hooks;
mod install;
mod protocol;

use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};
use agentscm::workspace_timeline_service::WorkspaceTimelineService;

use hooks::run_capture_hook;
use install::{InstallRequest, install};
use protocol::run_mcp;

const MAX_ID_BYTES: usize = 128;
const MAX_CURSOR_BYTES: usize = 4 * 1024;
pub const MAX_PAGE_ITEMS: u16 = 100;

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// Native client integrations with a documented completion event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ClientAgent {
    Claude,
    Codex,
    Cursor,
    Copilot,
    Gemini,
}

impl ClientAgent {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Copilot => "copilot",
            Self::Gemini => "gemini",
        }
    }
}

/// Filesystem capabilities fixed before any MCP request or hook payload is read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedWorkspace {
    pub data_dir: PathBuf,
    pub workspace_root: PathBuf,
    pub fork_root: Option<PathBuf>,
}

impl PinnedWorkspace {
    pub fn open(data_dir: &Path, workspace_root: &Path, fork_root: Option<&Path>) -> Result<Self> {
        let data_dir = canonical_directory(data_dir, "data directory")?;
        let workspace_root = canonical_directory(workspace_root, "workspace root")?;
        let fork_root = fork_root
            .map(|path| canonical_directory(path, "fork root"))
            .transpose()?;
        if data_dir == workspace_root {
            return Err(invalid("data directory and workspace root must differ"));
        }
        if fork_root.as_ref().is_some_and(|root| {
            root == &data_dir || root == &workspace_root || root.starts_with(&workspace_root)
        }) {
            return Err(invalid(
                "fork root must be separate from the data directory and workspace root",
            ));
        }
        Ok(Self {
            data_dir,
            workspace_root,
            fork_root,
        })
    }

    pub fn contains_cwd(&self, cwd: &Path) -> bool {
        fs::canonicalize(cwd)
            .is_ok_and(|cwd| cwd == self.workspace_root || cwd.starts_with(&self.workspace_root))
    }
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let path = fs::canonicalize(path)
        .map_err(|_| invalid(format!("{label} does not name an existing directory")))?;
    if !fs::metadata(&path)?.is_dir() {
        return Err(invalid(format!("{label} is not a directory")));
    }
    if path
        .components()
        .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(invalid(format!("{label} is not canonical")));
    }
    if path.to_str().is_none() {
        return Err(invalid(format!("{label} is not valid UTF-8")));
    }
    Ok(path)
}

#[derive(Clone, Debug, Args)]
pub struct PinnedArgs {
    /// Existing native workspace root captured by this process.
    #[arg(long)]
    pub workspace: PathBuf,
    /// Existing operator-selected parent for new fork workspaces.
    #[arg(long)]
    pub fork_root: Option<PathBuf>,
}

impl PinnedArgs {
    fn open(&self, data_dir: &Path) -> Result<PinnedWorkspace> {
        PinnedWorkspace::open(data_dir, &self.workspace, self.fork_root.as_deref())
    }
}

#[derive(Clone, Debug, Args)]
pub struct HookArgs {
    #[command(flatten)]
    pub pinned: PinnedArgs,
    /// Native client hook payload shape.
    #[arg(long, value_enum)]
    pub agent: ClientAgent,
}

#[derive(Clone, Debug, Args)]
pub struct InstallArgs {
    #[command(flatten)]
    pub pinned: PinnedArgs,
    /// Native client whose completion hook and MCP entry should be configured.
    #[arg(long, value_enum)]
    pub agent: ClientAgent,
    /// Operator-local native hook settings file.
    #[arg(long)]
    pub hook_settings: PathBuf,
    /// Operator-local MCP settings file. May equal hook settings when the client combines them.
    #[arg(long)]
    pub mcp_settings: PathBuf,
    /// Apply the previewed settings update.
    #[arg(long)]
    pub apply: bool,
}

#[derive(Clone, Debug, Subcommand)]
pub enum WorkspaceClientCommand {
    /// Serve the pinned native workspace over bounded local stdio MCP.
    Mcp(PinnedArgs),
    /// Capture at one supported native completion-hook event without blocking the client.
    Hook(HookArgs),
    /// Preview or apply operator-local completion-hook and MCP settings.
    Install(InstallArgs),
}

/// Synchronous CLI driver. MCP and hook processes construct exactly one startup-pinned service.
pub fn run(data_dir: &Path, command: WorkspaceClientCommand) -> Result<()> {
    match command {
        WorkspaceClientCommand::Mcp(args) => {
            let pinned = args.open(data_dir)?;
            let service = WorkspaceTimelineService::new(
                &pinned.data_dir,
                &pinned.workspace_root,
                pinned.fork_root.as_deref(),
            )?;
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            run_mcp(&pinned, &service, &mut stdin.lock(), &mut stdout.lock())
        }
        WorkspaceClientCommand::Hook(args) => {
            let mut stderr = std::io::stderr();
            let output = (|| -> Result<serde_json::Value> {
                let pinned = args.pinned.open(data_dir)?;
                let service = WorkspaceTimelineService::new(
                    &pinned.data_dir,
                    &pinned.workspace_root,
                    pinned.fork_root.as_deref(),
                )?;
                let mut input = Vec::new();
                std::io::stdin()
                    .take((hooks::MAX_HOOK_INPUT_BYTES + 1) as u64)
                    .read_to_end(&mut input)?;
                let (_, output) =
                    run_capture_hook(args.agent, &pinned, &input, &service, &mut stderr);
                Ok(output)
            })()
            .unwrap_or_else(|_| {
                let _ = writeln!(
                    stderr,
                    "AgentSCM capture failed: inspect the configured local workspace"
                );
                serde_json::json!({})
            });
            serde_json::to_writer(std::io::stdout(), &output)?;
            std::io::stdout().write_all(b"\n")?;
            Ok(())
        }
        WorkspaceClientCommand::Install(args) => {
            let request = InstallRequest {
                agent: args.agent,
                hook_settings: args.hook_settings,
                mcp_settings: args.mcp_settings,
                executable: std::env::current_exe()?,
                pinned: args.pinned.open(data_dir)?,
                apply: args.apply,
            };
            serde_json::to_writer_pretty(std::io::stdout(), &install(&request)?)?;
            std::io::stdout().write_all(b"\n")?;
            Ok(())
        }
    }
}

fn checked_id(value: &str, label: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte))
    {
        return Err(invalid(format!("{label} is invalid")));
    }
    Ok(value.to_owned())
}

fn checked_cursor(value: Option<String>) -> Result<Option<String>> {
    match value {
        Some(value)
            if value.is_empty()
                || value.len() > MAX_CURSOR_BYTES
                || value.chars().any(char::is_control) =>
        {
            Err(invalid("cursor is invalid"))
        }
        value => Ok(value),
    }
}

fn checked_limit(value: Option<u16>) -> Result<u16> {
    match value.unwrap_or(50) {
        1..=MAX_PAGE_ITEMS => Ok(value.unwrap_or(50)),
        _ => Err(invalid("limit is out of range")),
    }
}

fn checked_child_basename(value: &str) -> Result<String> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || path.is_absolute()
        || path.components().count() != 1
        || matches!(value, "." | "..")
        || value.chars().any(char::is_control)
        || value.contains(['/', '\\'])
    {
        return Err(invalid("child_basename is invalid"));
    }
    Ok(value.to_owned())
}
