# Codex capture fixtures

Hook payloads have the fields of the hook input schemas of Codex CLI 0.167.1
(`codex-rs/hooks/schema/generated`) and the value shapes a session of that version sent: a shell
call is `Bash` with a `command`, its `tool_response` is the output as a string, and a call id is
`exec-` and an id. `rollout.jsonl` has the record shapes of a session file that version wrote.
Every value is made up; nothing here was copied from a real session.
