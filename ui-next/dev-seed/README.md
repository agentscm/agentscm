# Dev seed hub

Dev-only. Starts a fresh, throwaway hub built from this checkout on `127.0.0.1:7332` and fills it
with synthetic agents through the real hook, MCP and stand-up HTTP endpoints.

```sh
ui-next/dev-seed/start.sh
```

- `mint/` mints the tokens for synthetic users straight into the database. A real hub needs a forge sign-in to do this.
  Repository: `counterbranch/agentscm`.
- `.hub.env` (written on start) holds `AGENTSCM_HUB` and the read-only `READ_TOKEN` for the UIs.
- `seed.py --live` posts a hook or checkpoint about every 20 s until Ctrl-C; the hub stops with it.
- Every start wipes `.data/`. All names, logins and work are synthetic.

What it fills: whoami and health (2 denials), intents, sessions, and activity events, including a
granted and a declined handoff. It also fills five current work items (active, blocked, handover,
paused) plus finished ones, and coordination requests in pending, responded and resolved states,
with the briefing history.

Not covered: pull requests, which need a forge connection; skill workflows, where the hub answers
`unavailable: native_workflow_not_configured`; the workspace, which needs a forge sign-in; and
detected overlaps (the hub refuses overlapping starts, so the list is an all-clear).
