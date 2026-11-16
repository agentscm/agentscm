//! Every place the engine treats coding agents differently by name is listed in [`REGISTER`],
//! so a feature cannot be wired for one or two agents without anyone noticing.
//!
//! The test reads every Rust file under `src/` and finds the agent-name gates in it: a string
//! compared with `==` or `!=` to an agent name (`"claude"`) or a hook route (`"hook:gemini"`),
//! agent names inside `matches!`, an agent variant compared or matched (`agent == Agent::Claude`,
//! `matches!(agent, Agent::Cursor | Agent::Copilot)`), a `match` whose arms name some agents
//! (strings or variants) and fall through a wildcard or binding arm, and an enum whose agent
//! variants are fewer than the wired agents. Comments, test modules and a match that lists every
//! wired agent (a parser or a display name) are not gates; each match is judged on its own. A
//! match with no wildcard is left to the compiler: adding a variant breaks it already.
//!
//! An entry names the file and the enclosing function (or `enum Name`), never a line, so
//! unrelated edits do not break it. Several gates in one function make one entry naming every
//! agent they single out.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// The agents `client setup` wires, read from the engine's own list so this cannot go stale.
fn wired() -> impl Iterator<Item = &'static str> {
    agentscm::clients::names()
}

/// `text` when it is a wired agent's name.
fn wired_named(text: &str) -> Option<&'static str> {
    agentscm::clients::find(text).map(|client| client.name)
}

/// Agent variants spelled other than the agent's name with a capital.
const ALIASES: [(&str, &str); 3] = [
    ("ClaudeCode", "claude"),
    ("GeminiCli", "gemini"),
    ("Generic", "generic"),
];

/// The agent an enum variant means (`Codex`, `ClaudeCode`), if any.
fn variant_agent(variant: &str) -> Option<&'static str> {
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == variant)
        .map(|(_, agent)| *agent)
        .or_else(|| wired_named(&variant.to_lowercase()))
}

/// Files whose job is to write or detect each host's own configuration, so every branch in them
/// is the host's file format rather than a feature: setup, install, onboarding and detection.
const SETUP_FILES: [&str; 9] = [
    "src/agent_stop_install.rs",
    "src/client_plugin.rs",
    "src/client_setup.rs",
    "src/hub/onboarding.rs",
    "src/init_cli.rs",
    "src/init_cli/",
    "src/quickstart/",
    "src/workspace_client/hooks.rs",
    "src/workspace_client/install.rs",
];

/// Enums with an agent-named variant that are not about an agent's hooks: the reviewer's model
/// transport runs Claude Code as a model, whichever agent the person uses.
const NOT_AGENTS: [&str; 1] = ["TransportBasis"];

/// Items that name agents without singling any out: `cursor` is a page cursor in
/// `native_query`; `agent_name` falls back to every agent's label in src/clients.rs; and
/// `native_client` admits every agent there through `generic_native`.
const NOT_GATES: [(&str, &str); 3] = [
    ("src/team/http.rs", "native_query"),
    ("src/hub/advice.rs", "agent_name"),
    ("src/hub/hook.rs", "native_client"),
];

struct Gate {
    file: &'static str,
    /// The enclosing function, or `enum Name` for an enum whose variants are the gate.
    item: &'static str,
    /// The agents the gate names, sorted and space-separated.
    agents: &'static str,
    /// `host limit: ...` (which limit), `not wired yet`, or `unknown`.
    reason: &'static str,
}

const REGISTER: &[Gate] = &[
    Gate {
        file: "src/agent_stop.rs",
        item: "enum StopAgent",
        agents: "claude codex",
        reason: "not wired yet: docs/agents.md lists no Stop adapter for Copilot, whose vendor \
                 documents a blocking agentStop; Cursor's stop hook cannot block (host limit); \
                 Gemini CLI and the src/clients.rs table agents are not in that matrix",
    },
    Gate {
        file: "src/change_capture/claude_code.rs",
        item: "capture_native_hook_inner",
        agents: "cursor",
        reason: "host limit: Cursor may reuse a call ID for Read then Write, so its observations \
                 cannot claim one managed action",
    },
    Gate {
        file: "src/change_capture/claude_code.rs",
        item: "capture_transcript",
        agents: "claude",
        reason: "host limit: this reader validates the Claude Code transcript schema; Codex uses its \
                 own reader",
    },
    Gate {
        file: "src/change_capture/claude_code.rs",
        item: "capture_transcript_page",
        agents: "claude",
        reason: "host limit: this page parser validates Claude Code session identity and transcript \
                 records",
    },
    Gate {
        file: "src/change_capture/claude_code.rs",
        item: "verify_history_start",
        agents: "claude",
        reason: "host limit: this proof parser requires a native Claude Code start record; Codex has \
                 a separate parser",
    },
    Gate {
        file: "src/change_capture/native_hooks.rs",
        item: "event_for",
        agents: "copilot cursor gemini",
        reason: "host limit: Cursor, Copilot and Gemini CLI name their hook events differently",
    },
    Gate {
        file: "src/change_capture/native_hooks.rs",
        item: "model",
        agents: "cursor gemini",
        reason: "host limit: Cursor and Gemini CLI expose model identifiers in their own payload \
                 fields",
    },
    Gate {
        file: "src/change_capture/native_hooks.rs",
        item: "normalize",
        agents: "copilot cursor gemini",
        reason: "not wired yet: these three native hook adapters are implemented; Claude Code and \
                 Codex use their existing readers",
    },
    Gate {
        file: "src/change_capture/native_hooks.rs",
        item: "tool_event",
        agents: "copilot cursor gemini",
        reason: "host limit: tool fields differ by host; only Cursor exposes a native tool call ID in \
                 these adapters",
    },
    Gate {
        file: "src/change_capture_cli.rs",
        item: "capture_native",
        agents: "claude codex",
        reason: "host limit: Claude Code carries native transcript paths; Codex model events and \
                 usage use its separate event schema",
    },
    Gate {
        file: "src/change_capture_cli.rs",
        item: "captured_harness",
        agents: "claude codex copilot cursor gemini",
        reason: "not wired yet: capture supports Claude Code, Codex, Cursor, Copilot and Gemini CLI; \
                 other setup clients have no capture adapter",
    },
    Gate {
        file: "src/change_capture_cli.rs",
        item: "native_start_proven",
        agents: "claude codex",
        reason: "not wired yet: source-authenticated history start proofs exist only for Claude Code \
                 and Codex",
    },
    Gate {
        file: "src/change_capture_cli.rs",
        item: "plugin_capture_inner",
        agents: "claude",
        reason: "host limit: legacy bindings install their own Claude Code capture hook; other \
                 clients use installed plugin hooks",
    },
    Gate {
        file: "src/change_capture_cli.rs",
        item: "watch_session",
        agents: "codex",
        reason: "not wired yet: structured asynchronous question snapshots are read from Codex \
                 history only",
    },
    Gate {
        file: "src/change_record.rs",
        item: "enum AgentHarnessV1",
        agents: "claude codex copilot cursor gemini",
        reason: "not wired yet: named capture harnesses cover these five clients",
    },
    Gate {
        file: "src/client_session.rs",
        item: "hook_agent",
        agents: "claude",
        reason: "host limit: other hosts inherit Claude settings, so configured Claude hooks require \
                 host detection first",
    },
    Gate {
        file: "src/client_session.rs",
        item: "inherited_hook_agent",
        agents: "claude copilot cursor",
        reason: "host limit: observable process and payload markers identify Claude Code, Cursor or \
                 Copilot; unsupported or conflicting markers skip safely",
    },
    Gate {
        file: "src/client_session.rs",
        item: "native_host",
        agents: "claude codex",
        reason: "not wired yet: native process qualification recognizes Claude Code and Codex CLI \
                 executables; other hosts retain ordinary hook and MCP delivery",
    },
    Gate {
        file: "src/continuation.rs",
        item: "enum NativeClient",
        agents: "claude codex",
        reason: "unknown",
    },
    Gate {
        file: "src/continuation.rs",
        item: "observed_session",
        agents: "claude codex",
        reason: "host limit: each client's output names its session in its own form \
                 (`session_id`; Codex's `thread.started` `thread_id`)",
    },
    Gate {
        file: "src/continuation.rs",
        item: "warning_count",
        agents: "codex",
        reason: "host limit: it counts Codex's own `item.completed` error events in its output",
    },
    Gate {
        file: "src/demo_cli.rs",
        item: "hook",
        agents: "codex",
        reason: "host limit: Codex edits arrive as `apply_patch` (docs/agents.md, Edit check)",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "activity_key",
        agents: "copilot cursor gemini",
        reason: "host limit: each host's own tool event names; Cursor's edit check is \
                 postToolUse, just after the write (docs/agents.md, Edit check)",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "branch_advisory_at",
        agents: "copilot",
        reason: "host limit: Copilot CLI's preToolUse cannot carry advisory text (comment)",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "call_hub",
        agents: "codex gemini",
        reason: "host limit: each host's hook payload and reply format (Codex payloads are \
                 sanitized, Gemini CLI replies are reshaped)",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "capture",
        agents: "claude codex copilot cursor gemini",
        reason: "not wired yet: the five capture adapters are implemented; retrospective history \
                 fanout is limited to Claude Code and Codex",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "capture_only",
        agents: "codex copilot cursor gemini",
        reason: "host limit: each native adapter has its own events needed only for capture rather \
                 than coordination",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "context_reply_with",
        agents: "claude codex copilot cursor gemini",
        reason: "host limit: each host takes added context in its own reply field and events; \
                 the table agents are answered from src/clients.rs before this match",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "hook",
        agents: "claude cursor",
        reason: "host limit: Cursor separates reply and turn end, and may inherit Claude hooks beside \
                 its native registration",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "is_session_start",
        agents: "copilot",
        reason: "host limit: Copilot CLI's camelCase payload names no event (comment)",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "observes_shell",
        agents: "claude codex",
        reason: "unknown: docs/agents.md says shell changes are not observed for Cursor, \
                 Copilot and Gemini CLI, not why",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "sanitized_client",
        agents: "claude codex copilot cursor gemini",
        reason: "host limit: each of the five agents with its own payload code sends its own \
                 shape; the table agents are read from src/clients.rs before this match",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "tool_call",
        agents: "copilot",
        reason: "host limit: Copilot CLI's camelCase payload (`toolArgs`, no event name)",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "waits",
        agents: "copilot cursor",
        reason: "host limit: each host's own wait events (docs/agents.md, waiting table)",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "with_local_note",
        agents: "copilot cursor gemini",
        reason: "host limit: each host reads added context from its own reply field",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "with_note",
        agents: "cursor",
        reason: "host limit: Cursor reads added context from `additional_context`",
    },
    Gate {
        file: "src/hook_cli.rs",
        item: "with_post_context",
        agents: "claude codex",
        reason: "host limit: Claude Code and Codex accept context on a shell call's end event \
                 (comment)",
    },
    Gate {
        file: "src/hook_cli/permission.rs",
        item: "applies_with",
        agents: "claude codex",
        reason: "unknown: docs/agents.md documents Hub answers for Claude Code and Codex \
                 prompts only, not why others are left out",
    },
    Gate {
        file: "src/hook_cli/permission.rs",
        item: "holds_prompt",
        agents: "codex",
        reason: "host limit: Codex shows its prompt only after the hook returns (comment)",
    },
    Gate {
        file: "src/hub/agent_messages.rs",
        item: "enum Waker",
        agents: "claude codex copilot",
        reason: "not wired yet: idle wake delivery supports Claude Code, Codex and Copilot \
                 notification hooks only",
    },
    Gate {
        file: "src/hub/hook.rs",
        item: "enum Agent",
        agents: "claude codex copilot cursor",
        reason: "unknown: Gemini CLI and the table agents reach the generic endpoint as \
                 `hook:<name>` (src/clients.rs); why these four have their own routes is not \
                 said",
    },
    Gate {
        file: "src/hub/hook.rs",
        item: "from_name",
        agents: "claude codex copilot cursor",
        reason: "unknown: Gemini CLI and the table agents reach the generic endpoint as \
                 `hook:<name>` (src/clients.rs); why these four have their own routes is not \
                 said",
    },
    Gate {
        file: "src/hub/hook.rs",
        item: "pascal",
        agents: "codex",
        reason: "not wired yet: durable structured question snapshots and native request_user_input \
                 detection are implemented for Codex only",
    },
    Gate {
        file: "src/hub/hook.rs",
        item: "render",
        agents: "claude copilot",
        reason: "host limit: Claude Code reads the rules in the MCP instructions; Copilot's \
                 reply shape (comments)",
    },
    Gate {
        file: "src/hub/hook.rs",
        item: "run",
        agents: "claude",
        reason: "host limit: Claude Code with a read-only token gets its one rule here, as it \
                 reads the others in the MCP instructions (comment)",
    },
    Gate {
        file: "src/hub/work_lifecycle.rs",
        item: "of",
        agents: "cursor",
        reason: "host limit: Cursor reports edits only after the call (docs/agents.md, Edit \
                 check)",
    },
    Gate {
        file: "src/intent_cli.rs",
        item: "enum Hook",
        agents: "claude",
        reason: "unknown",
    },
    Gate {
        file: "src/message_wait.rs",
        item: "can_wait",
        agents: "copilot",
        reason: "host limit: Copilot can start this waiter only from a notification hook with a \
                 notification type",
    },
    Gate {
        file: "src/message_wait.rs",
        item: "wait_body",
        agents: "claude",
        reason: "host limit: preserve the legacy Claude waiter body; other waiters name their \
                 delivery mechanism",
    },
    Gate {
        file: "src/message_wait.rs",
        item: "request",
        agents: "codex",
        reason: "host limit: Codex queue dispatch requires a current incarnation and qualified live \
                 native host; Claude and Copilot can use their legacy wait contract",
    },
    Gate {
        file: "src/message_wait.rs",
        item: "run",
        agents: "codex",
        reason: "host limit: Codex asynchronous hooks do not wake a turn by returning output, so \
                 the waiter uses the native queue command",
    },
    Gate {
        file: "src/message_wait.rs",
        item: "valid",
        agents: "codex",
        reason: "host limit: a legacy wait response cannot authorize Codex queue dispatch",
    },
    Gate {
        file: "src/message_wait.rs",
        item: "waited",
        agents: "codex",
        reason: "host limit: Codex delivery requires a runnable codex queue command before a notice \
                 is consumed",
    },
    Gate {
        file: "src/message_wait.rs",
        item: "waiter",
        agents: "claude codex copilot",
        reason: "not wired yet: idle wake adapters exist only for Claude Code, Codex and Copilot",
    },
    Gate {
        file: "src/skill_cli.rs",
        item: "enum AgentArg",
        agents: "claude codex",
        reason: "not wired yet: the agents with a Stop adapter (see enum StopAgent)",
    },
    Gate {
        file: "src/workspace_client.rs",
        item: "enum ClientAgent",
        agents: "claude codex copilot cursor gemini",
        reason: "not wired yet: workspace-client install has adapters for these five only \
                 (docs/workspace-timeline.md); the src/clients.rs table agents have none",
    },
];

#[test]
fn every_agent_name_gate_is_in_the_register() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = BTreeMap::new();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    for path in files {
        let file = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if is_test_file(&file) || SETUP_FILES.iter().any(|setup| file.starts_with(setup)) {
            continue;
        }
        for (item, agents) in gates(&fs::read_to_string(&path).unwrap()) {
            if !NOT_GATES.contains(&(file.as_str(), item.as_str())) {
                found.insert((file.clone(), item), agents);
            }
        }
    }

    let registered: BTreeMap<(String, String), String> = REGISTER
        .iter()
        .map(|gate| {
            (
                (gate.file.to_owned(), gate.item.to_owned()),
                gate.agents.to_owned(),
            )
        })
        .collect();
    assert_eq!(
        registered.len(),
        REGISTER.len(),
        "an entry is in the register twice"
    );
    for gate in REGISTER {
        assert!(
            gate.reason.starts_with("host limit: ")
                || gate.reason.starts_with("not wired yet")
                || gate.reason.starts_with("unknown"),
            "{} {}: the reason must start `host limit: <which>`, `not wired yet` or `unknown`",
            gate.file,
            gate.item
        );
    }

    let mut problems = String::new();
    for ((file, item), agents) in &found {
        match registered.get(&(file.clone(), item.clone())) {
            Some(listed) if listed == agents => {}
            Some(listed) => writeln!(
                problems,
                "  changed: {file} {item} now names `{agents}`, the register says `{listed}`"
            )
            .unwrap(),
            None => writeln!(
                problems,
                "  new: Gate {{ file: \"{file}\", item: \"{item}\", agents: \"{agents}\", \
                 reason: \"...\" }},"
            )
            .unwrap(),
        }
    }
    for (file, item) in registered.keys() {
        if !found.contains_key(&(file.clone(), item.clone())) {
            writeln!(
                problems,
                "  gone: {file} {item} no longer has an agent-name gate"
            )
            .unwrap();
        }
    }
    assert!(
        problems.is_empty(),
        "Agent-name gates in src/ differ from the register in tests/integration/agent_gates.rs:\n\
         {problems}\
         A gate decides that only some coding agents get a feature. Update REGISTER: add or change \
         the entry with the agents it names and the reason, `host limit: <what the host cannot \
         do>`, `not wired yet` or `unknown`, and remove entries that are gone. Then update the \
         capability table in docs/agents.md so it says which agents get the feature."
    );
}

/// Variants are mapped to agents by name: this fails when setup's `Agent` enum spells an agent
/// in a way `variant_agent` does not read, which would hide that agent's enum gates.
#[test]
fn every_setup_agent_variant_reads_as_a_wired_agent() {
    let source = fs::read_to_string(format!(
        "{}/src/client_setup.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let m = mask(&source);
    let open = source
        .find("enum Agent {")
        .expect("client_setup.rs has no `enum Agent`")
        + 11;
    let variants = enum_variants(&m, open);
    let read: BTreeSet<&str> = variants
        .iter()
        .map(|variant| variant_agent(variant).unwrap_or(variant))
        .collect();
    assert_eq!(
        read,
        wired().collect::<BTreeSet<_>>(),
        "src/client_setup.rs `Agent` variants do not all read as agents in src/clients.rs: add \
         the variant's spelling to ALIASES in tests/integration/agent_gates.rs"
    );
}

fn rust_files(directory: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
    out.sort();
}

/// Files that only `#[cfg(test)]` modules include.
fn is_test_file(file: &str) -> bool {
    file.ends_with("/tests.rs") || file.contains("/tests/")
}

fn ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `source` with comments blanked, and every byte inside a string or char literal other than
/// letters, digits, `_`, `-` and `:` blanked, so braces count and agent names stay. Same length.
fn mask(source: &str) -> Vec<u8> {
    let s = source.as_bytes();
    let mut out = s.to_vec();
    let blank = |out: &mut Vec<u8>, range: std::ops::Range<usize>, keep_words: bool| {
        for byte in &mut out[range] {
            let word = byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b':');
            if *byte != b'\n' && !(keep_words && word) {
                *byte = b' ';
            }
        }
    };
    let mut i = 0;
    while i < s.len() {
        let previous_ident = i > 0 && ident(s[i - 1]);
        if s[i..].starts_with(b"//") {
            let end = s[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(s.len(), |n| i + n);
            blank(&mut out, i..end, false);
            i = end;
        } else if s[i..].starts_with(b"/*") {
            let (mut j, mut depth) = (i + 2, 1);
            while j < s.len() && depth > 0 {
                if s[j..].starts_with(b"/*") {
                    depth += 1;
                    j += 2;
                } else if s[j..].starts_with(b"*/") {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i..j, false);
            i = j;
        } else if !previous_ident && (s[i] == b'r' || s[i..].starts_with(b"br")) && {
            let start = if s[i] == b'b' { i + 2 } else { i + 1 };
            let hashes = s[start..].iter().take_while(|&&b| b == b'#').count();
            s.get(start + hashes) == Some(&b'"')
        } {
            let start = if s[i] == b'b' { i + 2 } else { i + 1 };
            let hashes = s[start..].iter().take_while(|&&b| b == b'#').count();
            let open = start + hashes;
            let mut close = vec![b'"'];
            close.extend(std::iter::repeat_n(b'#', hashes));
            let end = (open + 1..s.len())
                .find(|&j| s[j..].starts_with(&close))
                .unwrap_or(s.len());
            blank(&mut out, open + 1..end, true);
            i = end + close.len();
        } else if s[i] == b'"' {
            let mut j = i + 1;
            while j < s.len() && s[j] != b'"' {
                j += if s[j] == b'\\' { 2 } else { 1 };
            }
            blank(&mut out, i + 1..j.min(s.len()), true);
            i = j + 1;
        } else if s[i] == b'\'' {
            // A char literal ('x', '\n', '{'); anything else is a lifetime.
            let end = if s.get(i + 1) == Some(&b'\\') {
                (i + 2..s.len()).find(|&j| s[j] == b'\'')
            } else {
                source[i + 1..]
                    .chars()
                    .next()
                    .map(|c| i + 1 + c.len_utf8())
                    .filter(|&j| s.get(j) == Some(&b'\''))
            };
            match end {
                Some(end) => {
                    blank(&mut out, i + 1..end, false);
                    i = end + 1;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    out
}

fn skip_space(m: &[u8], mut i: usize) -> usize {
    while i < m.len() && m[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn read_ident(m: &[u8], i: usize) -> &str {
    let end = (i..m.len()).find(|&j| !ident(m[j])).unwrap_or(m.len());
    std::str::from_utf8(&m[i..end]).unwrap()
}

/// Whether the code just before `start` or just after `end` is `==` or `!=`.
fn compared(m: &[u8], start: usize, end: usize) -> bool {
    let before = m[..start]
        .iter()
        .rposition(|b| !b.is_ascii_whitespace() && *b != b'*' && *b != b'&')
        .map(|j| &m[j.saturating_sub(1)..=j]);
    let after = m
        .get(skip_space(m, end)..)
        .map(|rest| &rest[..rest.len().min(2)]);
    [before, after]
        .iter()
        .any(|side| matches!(side, Some(b"==") | Some(b"!=")))
}

/// The first index from `from` where `stop` holds at bracket depth 0 relative to `from`, or
/// where a bracket closes below that depth.
fn at_depth(m: &[u8], from: usize, stop: impl Fn(&[u8]) -> bool) -> usize {
    let mut depth = 0;
    for j in from..m.len() {
        if depth == 0 && stop(&m[j..]) {
            return j;
        }
        match m[j] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' if depth == 0 => return j,
            b')' | b']' | b'}' => depth -= 1,
            _ => {}
        }
    }
    m.len()
}

/// The agents named by the arms of the `match` keyword at `at`, when that match singles some
/// out: it names fewer than every wired agent and has a wildcard or binding arm, so a new agent
/// falls through it without the compiler noticing. Each match counts on its own.
fn match_gate(m: &[u8], at: usize) -> BTreeSet<&'static str> {
    let open = at_depth(m, at + 5, |rest| rest[0] == b'{');
    if open >= m.len() || m[open] != b'{' {
        return BTreeSet::new();
    }
    let close = at_depth(m, open + 1, |_| false);
    let (mut agents, mut fallback) = (BTreeSet::new(), false);
    let mut j = open + 1;
    while skip_space(m, j) < close {
        let arrow = at_depth(m, j, |rest| rest.starts_with(b"=>"));
        if arrow >= close {
            break;
        }
        let pattern = std::str::from_utf8(&m[j..arrow]).unwrap();
        let pattern = pattern.split(" if ").next().unwrap().trim();
        fallback |= pattern == "_"
            || pattern.contains(" @ ")
            || pattern.starts_with(|c: char| c.is_ascii_lowercase()) && pattern.bytes().all(ident);
        for (index, piece) in pattern.split('"').enumerate() {
            let agent = piece.strip_prefix("hook:").unwrap_or(piece);
            if index % 2 == 1
                && let Some(agent) = wired_named(agent)
            {
                agents.insert(agent);
            }
        }
        for path in pattern.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':')) {
            if let Some((ty, variant)) = path.split_once("::")
                && !NOT_AGENTS.contains(&ty)
                && let Some(agent) = variant_agent(variant)
            {
                agents.insert(agent);
            }
        }
        let body = skip_space(m, arrow + 2);
        j = if m.get(body) == Some(&b'{') {
            let end = at_depth(m, body + 1, |_| false) + 1;
            let after = skip_space(m, end);
            if m.get(after) == Some(&b',') {
                after + 1
            } else {
                end
            }
        } else {
            let end = at_depth(m, body, |rest| rest[0] == b',');
            if end < close { end + 1 } else { close }
        };
    }
    let covered = wired().all(|agent| agents.contains(agent));
    if fallback && !covered {
        agents
    } else {
        BTreeSet::new()
    }
}

/// The agent-name gates in one file: enclosing item to the agents it names.
fn gates(source: &str) -> BTreeMap<String, String> {
    let m = mask(source);
    // Per item: agents named by comparisons, `matches!` and partial matches.
    let mut named: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    let mut enums = BTreeMap::new();
    // Open braces: Some(item) for a function body, None for any other block.
    let mut stack: Vec<(Option<String>, bool)> = Vec::new();
    let (mut pending_fn, mut pending_test, mut brackets) = (None::<String>, false, 0i32);
    let mut matches_end = 0;
    let mut i = 0;
    while i < m.len() {
        let in_test = stack.iter().any(|(_, test)| *test);
        let starts_word = i == 0 || !ident(m[i - 1]);
        let item = stack
            .iter()
            .rev()
            .find_map(|(item, _)| item.clone())
            .unwrap_or_else(|| "(outside a function)".to_owned());
        match m[i] {
            b'{' => {
                stack.push((pending_fn.take(), pending_test));
                pending_test = false;
            }
            b'}' => {
                stack.pop();
            }
            b'[' => brackets += 1,
            b']' => brackets -= 1,
            b';' if brackets == 0 => {
                pending_fn = None;
                pending_test = false;
            }
            b'#' if m[i..].starts_with(b"#[cfg(test)]")
                || m[i..].starts_with(b"#[cfg(all(test") =>
            {
                pending_test = true;
            }
            b'f' if starts_word && m[i..].starts_with(b"fn ") => {
                pending_fn = Some(read_ident(&m, skip_space(&m, i + 3)).to_owned());
            }
            b'e' if starts_word && !in_test && m[i..].starts_with(b"enum ") => {
                let name_at = skip_space(&m, i + 5);
                let name = read_ident(&m, name_at).to_owned();
                let open = (name_at..m.len()).find(|&j| m[j] == b'{' || m[j] == b';');
                if let Some(open) = open.filter(|&j| m[j] == b'{') {
                    let agents = enum_agents(&m, open);
                    if !agents.is_empty() && !NOT_AGENTS.contains(&name.as_str()) {
                        enums.insert(format!("enum {name}"), agents);
                    }
                }
            }
            b'm' if starts_word && m[i..].starts_with(b"matches!(") => {
                let mut depth = 0;
                matches_end = (i + 8..m.len())
                    .find(|&j| {
                        depth += match m[j] {
                            b'(' => 1,
                            b')' => -1,
                            _ => 0,
                        };
                        depth == 0
                    })
                    .unwrap_or(m.len());
                // A `matches!` that lists every wired agent asks "is this a wired agent at all".
                let listed = std::str::from_utf8(&m[i..matches_end]).unwrap();
                if wired().all(|agent| listed.contains(&format!("\"{agent}\""))) {
                    i = matches_end;
                    continue;
                }
            }
            b'm' if starts_word
                && !in_test
                && m[i..].starts_with(b"match")
                && !m.get(i + 5).copied().is_some_and(ident) =>
            {
                let agents = match_gate(&m, i);
                if !agents.is_empty() {
                    named.entry(item).or_default().extend(agents);
                }
            }
            b'"' if !in_test => {
                let end = (i + 1..m.len()).find(|&j| m[j] == b'"').unwrap_or(m.len());
                let text = std::str::from_utf8(&m[i + 1..end]).unwrap();
                let agent = text.strip_prefix("hook:").unwrap_or(text);
                if let Some(agent) = wired_named(agent)
                    && (i < matches_end || compared(&m, i, end + 1))
                {
                    named.entry(item).or_default().insert(agent);
                }
                i = end + 1;
                continue;
            }
            byte if starts_word && !in_test && byte.is_ascii_uppercase() => {
                let path = read_ident(&m, i);
                let after = i + path.len();
                if m[after..].starts_with(b"::") && !NOT_AGENTS.contains(&path) {
                    let variant = read_ident(&m, after + 2);
                    let end = after + 2 + variant.len();
                    if let Some(agent) = variant_agent(variant)
                        && (i < matches_end || compared(&m, i, end))
                    {
                        named.entry(item).or_default().insert(agent);
                    }
                }
                i = after.max(i + 1);
                continue;
            }
            _ => {}
        }
        i += 1;
    }

    let wired: BTreeSet<&str> = wired().collect();
    let mut out = BTreeMap::new();
    for (item, gates) in named {
        out.insert(item, gates.into_iter().collect::<Vec<_>>().join(" "));
    }
    for (item, agents) in enums {
        if !agents.is_superset(&wired) {
            out.insert(item, agents.into_iter().collect::<Vec<_>>().join(" "));
        }
    }
    out
}

/// The agents an enum's variants name, for the enum body opening at `open`.
fn enum_agents(m: &[u8], open: usize) -> BTreeSet<&'static str> {
    let mut agents = BTreeSet::new();
    for name in enum_variants(m, open) {
        // `Generic` is the absence of an agent, so it alone does not make an agent enum.
        if let Some(agent) = variant_agent(&name)
            && agent != "generic"
        {
            agents.insert(agent);
        }
    }
    agents
}

/// The variant names of the enum body opening at `open`.
fn enum_variants(m: &[u8], open: usize) -> Vec<String> {
    let mut variants = Vec::new();
    let (mut depth, mut j, mut at_variant) = (0, open, true);
    while j < m.len() {
        match m[j] {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            b',' if depth == 1 => at_variant = true,
            byte if depth == 1 && at_variant && ident(byte) => {
                let name = read_ident(m, j);
                variants.push(name.to_owned());
                at_variant = false;
                j += name.len();
                continue;
            }
            _ => {}
        }
        j += 1;
    }
    variants
}

#[test]
fn the_scanner_finds_each_gate_form_and_skips_tests_comments_and_full_lists() {
    // The full lists follow the wired agents, so they stay full when an agent is added.
    let names: Vec<&str> = wired().collect();
    let arms = |names: &[&str]| {
        names
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let (first, second) = names.split_at(names.len() / 2);
    let variants: Vec<String> = names
        .iter()
        .map(|name| name[..1].to_uppercase() + &name[1..])
        .collect();
    let source = r##"
        // agent == "claude" in a comment
        fn shell(agent: &str) -> bool { matches!(agent, "claude" | "codex") }
        fn route(via: &str) -> bool { via == "hook:gemini" }
        fn variant(agent: Agent) -> bool { agent != Agent::Generic && "{" != "}" }
        fn harness(agent: &str) -> Option<u8> {
            match agent { "claude" if true => Some(1), "codex" => Some(2), _ => None }
        }
        fn parse(agent: &str) -> u8 { match agent { ALL => 1, _ => 0 } }
        fn partial(agent: Agent) -> bool { match agent { Agent::Claude => true, _ => false } }
        fn exhaustive(agent: Waker) -> u8 {
            match agent { Waker::Claude => { 1 } Waker::Codex(n) => n }
        }
        fn two(a: &str, b: &str) -> u8 {
            match a { FIRST => 1, other => 0 } + match b { SECOND => 1, _ => 0 }
        }
        enum Waker { Claude, #[serde(rename = "x")] Codex(u8) }
        enum Every { VARIANTS }
        #[cfg(test)]
        mod tests { fn t() { assert!(agent == "cursor"); } }
    "##
    .replace("ALL", &arms(&names))
    .replace("FIRST", &arms(first))
    .replace("SECOND", &arms(second))
    .replace("VARIANTS", &variants.join(", "));
    let found = gates(&source);
    let mut every = names.clone();
    every.sort();
    let every = every.join(" ");
    let expected: BTreeMap<String, String> = [
        ("shell", "claude codex"),
        ("route", "gemini"),
        ("variant", "generic"),
        ("partial", "claude"),
        ("two", every.as_str()),
        ("harness", "claude codex"),
        ("enum Waker", "claude codex"),
    ]
    .into_iter()
    .map(|(item, agents)| (item.to_owned(), agents.to_owned()))
    .collect();
    assert_eq!(found, expected);
}
