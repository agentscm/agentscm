"""Native hook shapes; this translation is not proof of a host or parent identity."""
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys

CLIENTS = ("claude", "codex", "cursor", "copilot", "gemini")
SESSION = re.compile(r"[A-Za-z0-9._:@-]{1,128}\Z")
CHILD_KEYS = ("agent_id", "agentId", "subagent_id", "parent_session_id", "parent_thread_id",
              "parentAgentId", "parent_agent_id", "parent_conversation_id", "parent_tool_call_id")
START_EVENTS = {"claude": "SessionStart", "codex": "SessionStart", "cursor": "sessionStart",
                "copilot": "sessionStart", "gemini": "SessionStart"}
TOOL_EVENTS = {"claude": "PreToolUse", "codex": "PreToolUse", "cursor": "postToolUse",
               "copilot": "postToolUse", "gemini": "AfterTool"}


def safe_path(value):
    return (isinstance(value, str) and os.path.isabs(value) and len(value) <= 4096
            and not re.search(r"[\x00-\x1f\x7f-\x9f\u202a-\u202e\u2066-\u2069]", value))


def normalize(client, raw, moment):
    if client not in CLIENTS or not isinstance(raw, dict) or any(key in raw for key in CHILD_KEYS):
        return None
    event = raw.get("hook_event_name")
    expected = START_EVENTS[client] if moment == "start" else TOOL_EVENTS[client]
    # Copilot's camelCase payload relies on the event's registered command; VS Code also names it.
    if client == "copilot" and event is None and isinstance(raw.get("sessionId"), str):
        event = expected
    if event != expected and not (client == "copilot" and event == expected[0].upper() + expected[1:]):
        return None
    root = raw.get("cwd")
    if root in (None, "") and client == "cursor":
        roots = raw.get("workspace_roots")
        if isinstance(roots, list) and len(roots) == 1:
            root = roots[0]
    session = next((raw.get(key) for key in ("session_id", "conversation_id", "sessionId")
                    if isinstance(raw.get(key), str) and raw[key]), None)
    if not safe_path(root) or not isinstance(session, str) or not SESSION.fullmatch(session):
        return None
    source = raw.get("source")
    if moment == "start":
        if source is None and client == "cursor":
            source = "startup"
        if client == "copilot" and source == "new":
            source = "startup"
        if source not in ("startup", "resume") and not (client == "claude" and source in ("clear", "compact")):
            return None
    tool = raw.get("tool_name", raw.get("toolName", ""))
    value = raw.get("tool_input", raw.get("toolArgs", {}))
    if not isinstance(tool, str):
        return None
    tail = re.split(r"__|\.|:", tool)[-1]
    if client == "gemini" and tool.startswith("mcp_agentscm_"):
        tail = tool.removeprefix("mcp_agentscm_")
    if tail in ("Bash", "bash", "Shell", "exec_command", "run_shell_command", "run_in_terminal"):
        if isinstance(value, str):
            try:
                value = json.loads(value)
            except ValueError:
                return None
        if not isinstance(value, dict):
            return None
        tool = "Bash"
        value = {"command": value.get("command", value.get("cmd", ""))}
    elif tail in ("Edit", "Write", "edit", "create", "apply_patch", "write_file", "replace"):
        tool, value = "Edit", {}
    elif tail in ("agentscm_start", "agentscm_work_preflight"):
        tool, value = tail, {}
    elif moment != "start":
        return None
    else:
        value = {}
    return {"cwd": root, "session_id": session, "source": source, "tool_name": tool,
            "tool_input": value, "native_event": event, "client": client,
            "hook_event_name": "SessionStart" if moment == "start" else "PreToolUse"}


def response(hook, text):
    client, event = hook["client"], hook["native_event"]
    if client == "cursor":
        return {"additional_context": text}
    if client == "copilot":
        return {"additionalContext": text}
    return {"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}}


def bounded(program, argv, seconds, max_output):
    """The deadline also covers stdin, Git and DNS; no hook failure reaches its host."""
    command = [sys.executable, "-B", str(Path(program).resolve()), "_worker", *argv]
    native = os.environ.get("AGENTSCM_FEEDBACK_EXECUTABLE")
    if native:
        if not safe_path(native):
            return
        command = [native, "--no-telemetry", "feedback", "_worker", *argv]
    with subprocess.Popen(command,
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True) as child:
        try:
            output, _ = child.communicate(timeout=seconds)
            if child.returncode == 0 and len(output) <= max_output:
                sys.stdout.buffer.write(output)
        finally:
            if child.returncode is None:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.communicate()
