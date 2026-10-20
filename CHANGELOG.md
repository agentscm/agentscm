# Changelog

All notable changes are listed here, newest first. AgentSCM is alpha: any release may change the CLI, the HTTP API, storage and schemas without a migration.

## Unreleased

- Live CLI qualification of the local Stop hooks (2026-10-20, one macOS machine, synthetic fixture, local native store). Claude Code CLI 2.1.304 print mode: `claude-stop` blocked with the failing test's output, passed after the fix with a current receipt confirmed independently, and an always-failing requirement stopped after 8 blocks. Codex CLI 0.163.0 `codex exec`: `codex-stop` blocked once and then passed, but hooks fired only with `--dangerously-bypass-hook-trust` and a native workspace also needs `--skip-git-repo-check`; the bound was not live-tested on Codex. Interactive sessions, desktop/IDE surfaces, hosted `remote-*` stops and agent-owned `task-*` stops are not live-tested. The capability matrix is now version 2 and notes vendor documentation for Cursor (`stop` can only continue via `followup_message`), Copilot (`agentStop` blocking, documented, not verified) and VS Code agent hooks (preview). See the [capability matrix](docs/agents.md#workflow-lifecycle-capability-matrix).
- Thin instruction entrypoint: `init-repo`'s managed block gains a short "Workflow tasks" section (discover with `capabilities`, fetch one `reference` on demand, never run feedback as a command), `schema_mismatch` from `instructions` and `reference` adds `server_schema_versions` and a fixed `action`, and `install-stop` task modes print an `instructions` pointer. No background updater is added.
- `skill-workflow task-stop` and `task-codex-stop`: read-only Stop hooks for agent-owned tasks. They call the facade `qualification` operation (30 second limit) and pass only when it is current and qualified and the clean native workspace base is the qualified revision; otherwise, including on network, auth or malformed-response errors, they block with typed failures and `next` data and never claim, collect, upload, finalize or checkpoint. `install-stop` accepts both. Fixture-tested, not live-qualified. See [agent workflow tasks](docs/advanced/agent-workflow-tasks.md#stop-hook-agent-owned-mode).
- Agent-task `capabilities` (schema discovery) and read-only `qualification` (is this exact subject currently qualified, with typed `next` data) over the workspace HTTP and MCP facade. A `reference` request with an unsupported `expected_schema_version` now returns a typed `schema_mismatch` instead of a generic invalid-request error.
- Stop triggers for Codex CLI and a bounded, installable Stop loop. `skill-workflow codex-stop` and `remote-codex-stop` are Codex CLI Stop hooks with the pass/block behavior of `claude-stop` and `remote-stop`. All four share a handler that reads at most 64 KiB, requires a `Stop` event, tolerates vendor fields, and allows at most 8 consecutive automatic continuations per agent, session, workspace and change (a runaway-loop guard, not crash-durable; counted under `<data-dir>/agent-stop/`; `stop_hook_active` is required and an explicit `false` resets the count and a pass does not; once exhausted or unreadable it allows the stop and says nothing was qualified). A payload `cwd` disjoint from `--workspace` is a no-op, and Stop subcommand parse errors exit 1 so they do not block. The local Stop pass predicate is tightened (`workspace_changed_during_run` must be false) and local concurrent-checkpoint detection is fixed (generation, base revision and pending operation are compared between the state and status reads). `skill-workflow install-stop` previews by default and, with `--apply`, adds or removes only its own entry for the same workspace (`--remove [--workspace PATH]`) in an operator-local Claude Code or Codex hook file, refusing files inside the workspace and symbolic links. The hooks are tested with fixture payloads built from vendor documentation, not live-qualified; omitting or bypassing them does not bypass native landing policy. See [Stop hook guide](docs/advanced/claude-code-workflows.md#opt-in-claude-code-stop-hook) and the [capability matrix](docs/agents.md#workflow-lifecycle-capability-matrix).
- Workspace capture honors exclusion rules. Status, checkpoint and the other workspace scans leave out untracked paths matched by `.gitignore`, a root `.agentscmignore`, the private `.agentscm-workspace/exclude` and, with a root `.git`, Git's global and `info/exclude` rules; base-revision paths are always captured. A root `.git` is skipped as metadata but needs a working `git` that accepts the repository. `agentscm workspace preview` lists included and excluded paths with the deciding rule, and `workspace status` reports `excluded_total`. Undo, merge application and refresh block rather than overwrite an excluded local file. Local backup still copies ignored files. See [source selection](docs/advanced/workspace.md#source-selection).
- A hosted laptop check counts as changed if the workspace base moves during the run.

## 0.1.0-alpha.3 (2026-10-17)

- `agentscm demo` ends with the next step, `agentscm quickstart` (or `npx agentscm`), and the status page's demo banner says the same.

## 0.1.0-alpha.2 (2026-10-17)

- `npx agentscm --version` and `--help` reach the binary instead of the setup guide.

## 0.1.0-alpha.1 (2026-10-17)

First alpha. **Not for production.** No scale, high-availability or security audit yet.

### What is in it

- **Collision warnings.** `agentscm collisions` and `GET /v1/collisions` report which branches and worktrees touch the same files, with a `git merge-tree` conflict check; read-only. See [collisions](docs/collisions.md).
- **Autopsy.** `agentscm autopsy` counts, from your own Claude Code logs, how often two sessions edited the same file while one still held it; local and read-only. See [autopsy](docs/autopsy.md).
- **Merged branches skipped.** `agentscm collisions` skips branches the default branch already has through a squash or rebase merge; `--include-merged` restores the old scan.
- **Readable output.** On a terminal `agentscm collisions` prints a short summary; `--json` and `--text` choose the format, and `--git-dir` defaults to the current directory.
- **Coordination hub.** `agentscm hub serve` on SQLite or PostgreSQL with local or S3-compatible storage, GitHub or GitLab sign-in, expiring agent tokens and an append-only event log. See [hub](docs/hub.md).
- **Work intents.** Agents announce the files they are about to edit and get attributed notices when others overlap, through the hub API, `agentscm intent` and a Claude Code hook example. See [work intents](docs/intents.md).
- **Warn once.** On Claude Code, Codex and Copilot pre-edit hooks the hub refuses the first edit in a session to a file another session holds; a repeat passes and nothing is locked. See [warn once](docs/hub.md#warn-once).
- **MCP and hook endpoints.** `POST /mcp` (six tools) and `POST /v1/hub/hook/{claude,codex,cursor,copilot,generic}` give agents the same coordination with nothing installed; the hook endpoints always answer 200 and fail open. See [agent sessions](docs/hub.md#agent-sessions-mcp-and-hook-endpoints).
- **Handoff (experimental).** A refusal asks the holder to hand the file over, and the holder answers with `agentscm_handoff`. See [handoff](docs/hub.md#handoff).
- **Status page.** The hub serves a read-only page at `/ui/` (in the container image and release binaries) showing live agents, contested files, overlapping pull requests, recent activity and handoffs; it signs in only with a read-only token (`agentscm hub token create --read-only`). See [status page](docs/hub.md#status-page).
- **Demo.** `agentscm demo` (or `npx agentscm demo`) runs a throwaway hub with three simulated agents in any Git repository and opens the status page; the agents and the handoff grant are scripted. See [the demo](docs/hub.md#try-it-locally-agentscm-demo).
- **Forge sync.** The hub observes open pull and merge requests and recent branches on GitHub and GitLab, and can post an opt-in comment and commit status. Tested only against in-process fakes.
- **Publish to GitHub.** `agentscm hub publish` turns a native revision in an exported mirror into a branch and pull request, resumably and without forcing.
- **Agent wiring.** `agentscm init-repo` writes committed config for Claude Code, Codex, Cursor and Copilot in VS Code, plus `AGENTS.md` and Gemini's pointer file; `agentscm hook <agent>` is the thin client for command hooks and fails open. See [supported agents](docs/agents.md).
- **Guided setup.** `npx agentscm` / `agentscm quickstart` walks through a first install; `agentscm uninstall` undoes it.
- **Native ledger.** Immutable revisions, review contexts, approvals, conditional integration, workspaces, merges and offline exchange. Experimental; see [advanced](docs/advanced/README.md).
- **Isolated command checks.** Run a declared command against an exact revision in a locked-down container and bind the result to approval. Experimental; see [execution](docs/advanced/execution.md).
- **Git bridges.** Snapshot capture, reconcile and export between Git and the native ledger, for a local shadow workflow. Experimental; see [advanced](docs/advanced/README.md).
- **Compose.** A single-host trial profile and a PostgreSQL profile (`postgres`, service `hub-postgres`; a volume from the old profile name is not reused). See [hub](docs/hub.md#running-in-containers).
- **Windows x86_64 binary.** Checksummed and attested like the others, installed by `npx agentscm`, with a subset of commands. See [install](docs/install.md#windows).
- **Container image and docs.** The release workflow publishes `ghcr.io/counterbranch/agentscm:<version>`, and CI covers formatting, clippy, tests, a container smoke test and a two-client hub end-to-end test. See [deployment](docs/deployment.md).

### Known limits

- Nothing is locked. Intents and collision warnings are advice; the only refusal is the warn-once refusal of one edit, and a repeat passes. Notices are collected by polling, not pushed, and none arrives when an intent expires. Revoked or expired tokens do not release their intents.
- Overlap means the same path or a path under an announced `dir/**`. Semantic conflicts are not detected, and work that is not announced is invisible. Only edit and write tool calls are announced, not shell writes.
- Hub sessions live in memory: a restart signs everyone out.
- The hub does not terminate TLS; put it behind a proxy. Plain `http` is accepted only for loopback addresses. The PostgreSQL connection has no TLS.
- GitHub and GitLab sync and GitHub publishing have been tested only against in-process fakes, not live forges. GitLab merge requests cannot be created. Publishing handles one parent, at most 1,000 paths and 64 MiB.
- Command execution is not a sandbox for arbitrary hostile code. The container-boundary tests qualify only the runtime they run on; there is no unattended recovery after a controller crash, and records are unsigned. Linux and Unix hosts with Docker only.
- Git export and reconcile refuse executable and symlink entries. Snapshot capture is one commit at a time with no history import. The hub is not yet joined to the native ledger, so there is no multi-user landing admission and no cutover from Git.
- One writable authority per data directory; the hub uses one serialized database connection per process. Nothing is measured at tens or hundreds of agents, and overlap lookup was measured only on a synthetic 1,000 intents.
- The read-only `serve` API is unauthenticated and meant for loopback; `serve` refuses a non-loopback `--bind` without `--public`, and exposing it needs an authenticating proxy you operate.
- Claude Code is the reference integration; the Codex, Cursor and Copilot hooks follow their vendors' documented formats. Claude Code cannot use an HTTP hook for `SessionStart`, and Cursor's pre-edit hook cannot carry a warning, so Cursor is warned just after the edit. Windows is not yet supported by `quickstart`.
- Schemas and storage may change between alpha releases without migration, and the authority database cannot be downgraded.
- The Windows binary is not code-signed, and the macOS binaries are not notarized. The Windows binary supports a subset of commands (setup guide without the collision check, `hook`, `init-repo`, hub client and hub, intents; not `collisions`, the native ledger, Git bridge, command checks, team service or recovery commands) and is x86_64 only.
- No automated model review.
