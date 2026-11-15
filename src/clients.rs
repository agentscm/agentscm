//! The coding agents `client setup` wires natively, in one list. Setup's `--agent` values, the
//! saved connection's agent names, onboarding, the hook client and the Hub's generic hook route
//! all come from this list, or are checked against it by a test.
//!
//! Claude Code, Codex, Cursor, GitHub Copilot and Gemini CLI have their own setup and payload
//! code. The agents with [`CommandHooks`] are wired from their table entry alone: their hooks run
//! a command with JSON on stdin, and the hook client turns that into the Hub's generic shape.
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// One natively wired coding agent.
#[derive(Debug)]
pub struct Client {
    /// The name `client setup --agent` and `agentscm hook` take.
    pub name: &'static str,
    /// The product name people know it by.
    pub label: &'static str,
    /// Its hooks reach the Hub's generic endpoint as `hook:<name>`, translated by the hook client.
    pub generic: bool,
    /// How it is wired, for an agent set up from this table.
    pub hooks: Option<&'static CommandHooks>,
}

/// A command-hook agent: where its user configuration is, and what its hooks send and read.
#[derive(Debug)]
pub struct CommandHooks {
    /// Programs on `PATH` that show it is installed.
    pub programs: &'static [&'static str],
    /// Its user configuration folder under `HOME`.
    pub home: &'static str,
    /// Variables that move that folder, in the order the agent reads them, each with the part
    /// added to its value.
    pub home_env: &'static [(&'static str, &'static str)],
    /// The user MCP file in that folder.
    pub mcp_file: &'static str,
    /// The key that maps server names to servers in it.
    pub mcp_key: &'static str,
    /// The `type` its stdio entries carry, when it documents one.
    pub mcp_type: Option<&'static str>,
    /// Whether its entry takes `trust: true` to run the server's tools without asking.
    pub mcp_trust: bool,
    /// The user hooks file in that folder; the hooks are under its `hooks` key.
    pub hooks_file: &'static str,
    /// The hooks file is the event map itself, with no `hooks` key.
    pub event_map_file: bool,
    /// Hooks listed straight under each event (`[{command}]`), not in groups of `hooks`.
    pub flat: bool,
    /// A file in that folder whose `hooks` the agent ignores once the hooks file exists.
    pub shadows: Option<&'static str>,
    /// Its user skills folder in that folder, when it documents one.
    pub skills: Option<&'static str>,
    /// Each native event setup registers: its name, what it is, its timeout in seconds.
    pub events: &'static [(&'static str, Event, u32)],
    /// Its shell tools, whose start and end the Hub counts so a long call stays live.
    pub shell_tools: &'static [&'static str],
    /// Its edit tools that name their file as `tool_input.file_path`.
    pub edit_tools: &'static [&'static str],
    /// Its patch tools, each with the `tool_input` key that holds the patch text.
    pub patch_tools: &'static [(&'static str, &'static str)],
    /// How it reads a refused edit.
    pub refusal: Refusal,
    /// Where a note for the model goes before a tool call.
    pub note: Note,
    /// Where the session start context goes.
    pub start: Note,
    /// Its hooks name the session and folder only at a session start; later hooks take the
    /// session the start hook recorded for the agent's process, and the hook's own folder.
    pub session_at_start_only: bool,
}

/// What a native hook event is to the Hub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Start,
    /// Before a tool call: an edit check when it names files, else activity.
    PreTool,
    /// After a tool call: activity.
    PostTool,
    /// A notification: a wait when its `notification_type` says for what.
    Notification,
    /// The end of a turn: the session waits for its person.
    Stop,
    End,
}

/// How an agent reads a refused tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny",...}}`
    PermissionDecision,
    /// `{"decision":"deny","reason":...}`
    Decision,
}

/// Where an agent takes text for its model from a hook's reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {
    /// It documents none for this event.
    None,
    /// `hookSpecificOutput.additionalContext`, with the event's name.
    HookSpecific,
    /// A top-level `additionalContext`.
    AdditionalContext,
    /// A top-level `context`.
    Context,
}

impl CommandHooks {
    /// The user configuration folder, given `HOME` and a way to read variables.
    pub fn folder(&self, home: &Path, var: impl Fn(&str) -> Option<OsString>) -> PathBuf {
        self.home_env
            .iter()
            .find_map(|(name, part)| {
                var(name)
                    .filter(|value| !value.is_empty())
                    .map(|value| PathBuf::from(value).join(part))
            })
            .unwrap_or_else(|| home.join(self.home))
    }

    /// The registered event called `name`, matched as the agent may spell it.
    pub fn event(&self, name: &str) -> Option<(&'static str, Event)> {
        let plain = |text: &str| text.replace('_', "").to_ascii_lowercase();
        let name = plain(name);
        self.events
            .iter()
            .find(|(native, ..)| plain(native) == name)
            .map(|(native, event, _)| (*native, *event))
    }

    pub fn is_shell(&self, tool: &str) -> bool {
        self.shell_tools
            .iter()
            .any(|shell| shell.eq_ignore_ascii_case(tool))
    }
}

const fn own(name: &'static str, label: &'static str, generic: bool) -> Client {
    Client {
        name,
        label,
        generic,
        hooks: None,
    }
}

const fn table(name: &'static str, label: &'static str, hooks: &'static CommandHooks) -> Client {
    Client {
        name,
        label,
        generic: true,
        hooks: Some(hooks),
    }
}

/// Every natively wired agent, in setup order.
pub const CLIENTS: [Client; 9] = [
    own("claude", "Claude Code", false),
    own("codex", "Codex", false),
    own("cursor", "Cursor", false),
    own("copilot", "GitHub Copilot", false),
    own("gemini", "Gemini CLI", true),
    table("droid", "Factory Droid", &DROID),
    table("qwen", "Qwen Code", &QWEN),
    table("junie", "Junie CLI", &JUNIE),
    table("crush", "Crush", &CRUSH),
];

/// The agent called `name`.
pub fn find(name: &str) -> Option<&'static Client> {
    CLIENTS.iter().find(|client| client.name == name)
}

/// Every agent's name, in setup order.
pub fn names() -> impl Iterator<Item = &'static str> {
    CLIENTS.iter().map(|client| client.name)
}

/// The names joined with `separator`, for help and error text.
pub fn names_joined(separator: &str) -> String {
    names().collect::<Vec<_>>().join(separator)
}

/// Whether a hook call's `via` (`hook:<name>`) came from a natively wired agent through the
/// generic route, so it counts as that agent's own hook.
pub fn generic_native(via: &str) -> bool {
    via.strip_prefix("hook:")
        .and_then(find)
        .is_some_and(|client| client.generic)
}

const NESTED_EVENTS: [(&str, Event, u32); 6] = [
    ("SessionStart", Event::Start, 5),
    ("PreToolUse", Event::PreTool, 5),
    ("PostToolUse", Event::PostTool, 3),
    ("Notification", Event::Notification, 3),
    ("Stop", Event::Stop, 3),
    ("SessionEnd", Event::End, 3),
];

static DROID: CommandHooks = CommandHooks {
    programs: &["droid"],
    home: ".factory",
    home_env: &[],
    mcp_file: "mcp.json",
    mcp_key: "mcpServers",
    mcp_type: Some("stdio"),
    mcp_trust: false,
    hooks_file: "hooks.json",
    event_map_file: true,
    flat: false,
    shadows: Some("settings.json"),
    skills: Some("skills"),
    events: &NESTED_EVENTS,
    shell_tools: &["Execute"],
    edit_tools: &["Create", "Edit"],
    patch_tools: &[],
    refusal: Refusal::PermissionDecision,
    note: Note::None,
    start: Note::HookSpecific,
    session_at_start_only: false,
};

static QWEN: CommandHooks = CommandHooks {
    programs: &["qwen"],
    home: ".qwen",
    home_env: &[("QWEN_HOME", "")],
    mcp_file: "settings.json",
    mcp_key: "mcpServers",
    mcp_type: None,
    mcp_trust: true,
    hooks_file: "settings.json",
    event_map_file: false,
    flat: false,
    shadows: None,
    skills: Some("skills"),
    events: &NESTED_EVENTS,
    shell_tools: &["run_shell_command"],
    edit_tools: &["write_file", "edit"],
    patch_tools: &[],
    refusal: Refusal::PermissionDecision,
    note: Note::HookSpecific,
    start: Note::HookSpecific,
    session_at_start_only: false,
};

static JUNIE: CommandHooks = CommandHooks {
    programs: &["junie"],
    home: ".junie",
    home_env: &[],
    mcp_file: "mcp/mcp.json",
    mcp_key: "mcpServers",
    mcp_type: None,
    mcp_trust: false,
    hooks_file: "config.json",
    event_map_file: false,
    flat: false,
    shadows: None,
    skills: Some("skills"),
    // Its PermissionRequest hook approves the call when it answers nothing, so it is not used.
    events: &[
        ("SessionStart", Event::Start, 5),
        ("PreToolUse", Event::PreTool, 5),
        ("PostToolUse", Event::PostTool, 3),
        ("Stop", Event::Stop, 3),
        ("SessionEnd", Event::End, 3),
    ],
    shell_tools: &["Bash", "powershell"],
    edit_tools: &["Edit", "Write"],
    patch_tools: &[("apply_patch", "patch")],
    refusal: Refusal::Decision,
    note: Note::AdditionalContext,
    // Its session start shows a hook's message to the person only.
    start: Note::None,
    session_at_start_only: true,
};

static CRUSH: CommandHooks = CommandHooks {
    programs: &["crush"],
    home: ".config/crush",
    home_env: &[("CRUSH_GLOBAL_CONFIG", ""), ("XDG_CONFIG_HOME", "crush")],
    mcp_file: "crush.json",
    mcp_key: "mcp",
    mcp_type: Some("stdio"),
    mcp_trust: false,
    hooks_file: "crush.json",
    event_map_file: false,
    flat: true,
    shadows: None,
    skills: Some("skills"),
    events: &[
        ("SessionStart", Event::Start, 5),
        ("PreToolUse", Event::PreTool, 5),
        ("PostToolUse", Event::PostTool, 3),
        ("TurnEnd", Event::Stop, 3),
        ("SessionEnd", Event::End, 3),
    ],
    shell_tools: &["bash"],
    edit_tools: &["edit", "write", "multiedit"],
    patch_tools: &[],
    refusal: Refusal::Decision,
    note: Note::Context,
    // Its session start hook only observes.
    start: Note::None,
    session_at_start_only: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_lowercase_and_fit_the_generic_route() {
        for (index, client) in CLIENTS.iter().enumerate() {
            assert!(
                client
                    .name
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-')),
                "{}",
                client.name
            );
            assert!(!CLIENTS[..index].iter().any(|c| c.name == client.name));
            // A table agent always takes the generic route.
            assert!(client.hooks.is_none() || client.generic, "{}", client.name);
        }
        assert!(generic_native("hook:gemini") && generic_native("hook:droid"));
        assert!(!generic_native("hook:claude") && !generic_native("hook:other"));
        assert!(!generic_native("droid"));
    }

    #[test]
    fn every_table_agent_registers_each_kind_of_event_once_at_most() {
        for client in CLIENTS.iter().filter(|client| client.hooks.is_some()) {
            let hooks = client.hooks.unwrap();
            for event in [Event::Start, Event::PreTool, Event::PostTool, Event::End] {
                assert_eq!(
                    hooks.events.iter().filter(|(_, e, _)| *e == event).count(),
                    1,
                    "{} {event:?}",
                    client.name
                );
            }
            for (native, event, timeout) in hooks.events {
                assert_eq!(hooks.event(native), Some((*native, *event)));
                assert!((1..=10).contains(timeout));
            }
            assert!(!hooks.edit_tools.is_empty());
        }
        assert_eq!(
            CRUSH.event("pre_tool_use"),
            Some(("PreToolUse", Event::PreTool))
        );
        assert_eq!(CRUSH.event("Notification"), None);
    }

    #[test]
    fn the_folder_follows_the_variables_the_agent_reads() {
        let home = Path::new("/home/me");
        let none = |_: &str| None;
        assert_eq!(CRUSH.folder(home, none), home.join(".config/crush"));
        let xdg = |name: &str| (name == "XDG_CONFIG_HOME").then(|| OsString::from("/x"));
        assert_eq!(CRUSH.folder(home, xdg), Path::new("/x/crush"));
        let both = |name: &str| Some(OsString::from(format!("/{name}")));
        assert_eq!(CRUSH.folder(home, both), Path::new("/CRUSH_GLOBAL_CONFIG"));
        let empty = |_: &str| Some(OsString::new());
        assert_eq!(QWEN.folder(home, empty), home.join(".qwen"));
    }

    /// The Hub page lists the agents it can show setup for; a missing one is dropped there.
    #[test]
    fn the_hub_page_lists_every_agent() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/src/connectAgent.ts");
        let Ok(text) = std::fs::read_to_string(path) else {
            return; // A packaged crate has no UI sources.
        };
        let quoted: Vec<String> = names().map(|name| format!("'{name}'")).collect();
        let line = format!(
            "export const CLIENT_AGENTS = [{}] as const",
            quoted.join(", ")
        );
        assert!(text.contains(&line), "{line}");
    }
}
