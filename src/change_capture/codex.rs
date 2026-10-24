//! Codex hook capability description for the shared capture contract.
//!
//! The current AgentSCM Codex integration observes session boundaries and edit starts, but does
//! not observe user/assistant messages or every tool completion. It therefore cannot claim full
//! conversation or execution capture. The source-bound observed-command path is shared with the
//! Claude Code adapter through [`super::run_observed_command`].

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub user_prompts: bool,
    pub assistant_messages: bool,
    pub tool_starts: bool,
    pub tool_finishes: bool,
    pub session_end: bool,
    pub retrospective_visible_history: bool,
}

pub const CAPABILITIES: Capabilities = Capabilities {
    user_prompts: false,
    assistant_messages: false,
    tool_starts: true,
    tool_finishes: false,
    session_end: true,
    retrospective_visible_history: false,
};
