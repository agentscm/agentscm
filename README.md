<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/agentscm-logo-dark.png">
    <img src="docs/assets/agentscm-logo.png" alt="AgentSCM" width="270">
  </picture>
</h1>

**Catch up on shared agent work. Review changes with their source and evidence.**

AgentSCM gives people and coding agents a shared place to see who is working on what,
what changed, what checks were recorded, and what needs attention next. Use it alongside
**your existing Git repositories, coding agents and GitHub or GitLab workflow**.

A self-hosted Hub holds coordination and shared records. Your computer can connect local
source and saved agent conversations to that view. Experimental native workflows add
revision-bound checks, review and explicit human approval; adopting coordination does
not require migrating your repository.

> **Alpha: try it, don't depend on it.** CLI, APIs and storage may change without migration.
> Production security and scale are not qualified. Git and your forge retain their merge
> authority; native landing requirements do not protect raw Git pushes or forge merges.

[Install](docs/install.md) · [Supported agents](docs/agents.md) ·
[Documentation](docs/README.md) · [Releases](https://github.com/agentscm/agentscm/releases)

## Quickstart

### Try a scripted demo first

```sh
npx agentscm demo
```

This starts a temporary local Hub, simulates three agents through its real hook endpoint and
opens the status page. No account, Docker or forge is required; nothing is written to your
repository. The agents and decisions are scripted. Press Ctrl-C to stop. See the
[scripted demo guide](docs/scripted-demo.md) for prerequisites and limits.

### Connect your agents

On a supported Unix host (macOS or Linux), run once per computer, not once per repository.
Requires Node 18 or later; see [Windows support](docs/install.md#windows) for its current limits:

```sh
npx agentscm
```

This installs a release binary, guides local Hub setup, signs you in and sets up **every detected
agent**: Claude Code, Codex, Cursor, GitHub Copilot and Gemini CLI. A managed Hub runs natively
on Apple Silicon macOS; Linux and Intel macOS need Docker or an existing Hub. Setup writes no
repository files. The local Hub's first run opens a page to create and install its GitHub App.

To join a Hub that already runs:

```sh
npx agentscm --hub-url https://hub.example.com
```

Then fully quit and reopen your agents. In a repository, ask one:

> Get started with AgentSCM.

In Codex, run `/hooks` once and trust the AgentSCM hooks. Other clients may ask you to trust
the folder or enable MCP. Installation does not mean every integration has been qualified in
live sessions; see [agent support and qualification](docs/agents.md#support-tiers).

Open a connected checkout's source and team view:

```sh
npx agentscm open
```

The page can switch between repositories connected on this computer, each using its own Hub
and access. The local source explorer is currently Unix-only. On macOS, the optional
[menu-bar companion](docs/install.md#where-files-go) also opens this view and shows connections.
Clicking its icon opens a work overview: every listed repository shows its local agent connections
and sightings beside current Hub tasks and live agents, including accessible Hub repositories
without a local checkout when the Hub supports that read. Repositories needing attention come first; **Needs you** filters them,
and **Start** sends a prompt to installed Claude Code or Codex CLI in a selected connected
checkout. The panel refreshes stale local and Hub reads when opened and stays on top when pinned.
Right click opens setup and troubleshooting.
The installed client keeps its own login and settings; open its native
session for interactive approvals and full CLI features. On Linux, `agentscm tray` provides a terminal
companion with saved checkout status and the same prompt route; a Linux status icon and native
pinned window are not yet qualified. Windows users can use `agentscm client status`.

See [install options](docs/install.md) for platform/runtime requirements, `--only`, `--dry-run`,
containers and uninstall (`npx agentscm uninstall`). A managed native Hub is qualified on Apple
Silicon macOS; Docker is an explicit fallback. Server deployment is
[agent-guided and unsupported in the alpha](docs/hosted.md).

## What you can do

| Need | Available alpha capability | Start here |
|---|---|---|
| Continue during a forge disconnection | Private task context, local command receipts, saved PR/MR drafts and explicit read-only GitHub/GitLab revalidation | [Continuity preparation](docs/continuity.md) |
| Catch up before taking over | Durable work claims, checkpoints, blockers, handovers and bounded agent briefings | [Stand-up and catch-up](docs/standup.md) |
| Coordinate concurrent edits | Shared file intents, overlap notices and handoff requests across machines; read-only Git branch/worktree collision checks | [Coordination](docs/intents.md) · [Collision checks](docs/collisions.md) |
| Inspect what an agent changed and checked | Source-bound action records with separate outcome, coverage, freshness and evidence availability | [Change capture](docs/change-capture.md) |
| Connect code and context | Read-only committed Git files, history and diffs beside available local conversations and recorded actions | [Repository explorer](docs/repository-explorer.md) · [Open this computer](docs/connect-this-computer.md) |
| Make a team's PR process visible | Experimental `pr-gate` v1: adopted steps, head-bound evidence, findings and next actions; opt-in Claude Code/Codex Stop hooks | [Supported skills](docs/design/supported-skills.md) |
| Reuse skills and workflow definitions | Local versioned authoring, pinned imports, simulation and an explicit export path to native execution | [Workflow library](docs/advanced/workflow-library.md) |
| Continue a managed task with another agent | Experimental local Claude Code/Codex continuation of enrolled Work, with bounded source edits, artifacts and retained context | [Portable responsibility](docs/advanced/portable-responsibility.md) |
| Run reviewed team rules | Experimental owner-reviewed contracts, rehearsal and guarded post-merge artifact workflows | [Operating policy](docs/advanced/operating-policy.md) |
| Retain an owner's commitment | Living Promises keep versioned requests, criteria and constraints; agent proposals and reports remain advisory | [Living Promises](docs/advanced/living-promises.md) |
| Pilot controlled native landing | Immutable revisions, pinned checks, bounded Claude Code review and human approval checked at native admission | [Native workflow loop](docs/advanced/claude-code-workflows.md) |

Coordination is **not locking**. Supported hooks refuse the first contested edit once per
session and file; a repeat passes, and hook failures let edits through. Cursor receives a
note after the edit; MCP-only clients receive advice. Only announced work is visible, and
same-file overlap is not proof of conflict. See [support boundaries](docs/agents.md).

`pr-gate` evidence is **self-reported**. A recorded test, independent review or approval is
not proof that it happened. Its opt-in Stop hook gates session completion; it does not own
GitHub merge permissions or replace branch protection. Other workflow-library declarations
can be authored or simulated without being runnable; export reports unmet bindings explicitly.

## Source and evidence you can inspect

Change capture retains bounded session facts and can bind an observed command to the source,
recipe and setup recorded before it ran. Clean Git bindings identify commits and trees;
working-file capture handles dirty source and reports omissions. Outcome, evidence availability,
coverage, source drift and freshness stay separate. A local receipt remains a caller assertion,
not independently verified execution.

Claude Code has the fuller capture adapter. Codex currently exposes session boundaries and
selected edit starts, **not** prompts, assistant messages, all tool completions or retrospective
visible history. Capture is best effort and bounded; missing events and unavailable evidence
must remain visible. See [capture and qualification](docs/change-capture.md).

Conversation capture defaults to **metadata-only**: no conversation bodies are retained or sent
to the Hub. Per checkout, you can opt into local conversations or shared conversations.
Shared bodies are redacted on the computer on a best-effort basis, then uploaded; only the
person sharing them and Hub admins can read them. Hub retention defaults to 30 days.
Turning sharing off stops future sharing; existing copies remain until expiry or deletion.
Read [sharing, access and deletion](docs/change-capture.md#sharing-conversations-with-the-hub)
before enabling it. This is distinct from task text and coordination metadata the Hub already
stores; see [Hub data](docs/hub.md#data-the-hub-stores).

The repository explorer reads local source and retained evidence. It does not run an agent,
execute checks, fetch or check out branches. One connected local page can combine that private
view with read-only team data from multiple repositories and Hubs.

## Git, forge checks and native workflows

Keep using your existing Git merge path. Experimental bridges can capture clean commits,
reconcile changes, export native source and publish a separate candidate branch with a GitHub
PR or GitLab MR. These bridges have synthetic/fake-provider qualification; live forge and
attached SSH operation are not qualified. See [Git exchange](docs/advanced/git-remote-network.md).

In the optional native workflow, checks and review apply to an immutable revision. Fixes create
new source that needs fresh qualifying evidence. Required evidence, current context, policy,
target and human approval are checked at native landing. Approval and landing are separate
operations. See the [workflow guide](docs/advanced/claude-code-workflows.md) and the
[single-server Community workspace pilot](docs/advanced/community-workspace.md).

The opt-in GitHub workflow status also handles a restricted **merge-group** case: Unix,
GitHub App connections, and a single candidate whose group source matches its qualified native
source. Combined/stacked group source is unsupported. This path is fake-tested only and has not
been live-qualified; do not require it on a queue-protected branch before qualification.
See [GitHub merge queues](docs/advanced/github-merge-queue.md).

AgentSCM does not configure branch protection or grant forge merge authority. An old green run,
a session report or an advisory status cannot authorize different source. Moving authority from
Git to the native ledger remains a separate proposed cutover, described in the [roadmap](docs/roadmap.md).

## Operating the alpha

- **You run the Hub.** Shared data goes to the service you configure. Review its
  [data handling](docs/hub.md#data-the-hub-stores) and [deployment guide](docs/deployment.md).
- **Native execution is a trusted-team pilot.** It needs persistent storage and explicit
  recovery; automatic failover and production security/scale qualification are not provided.
- **Back up native state separately.** Git exports preserve source, not workflow or approval
  state. Follow the [backup and recovery guide](docs/advanced/community-workspace.md#operations-and-recovery).
- **Telemetry requires configuration.** No usage event can be sent until an endpoint and
  data-handling profile are configured. See [data and controls](docs/telemetry.md).

## Documentation and contributing

[Documentation index](docs/README.md) · [CLI reference](docs/reference/cli.md) ·
[Architecture](docs/architecture.md) · [Roadmap](docs/roadmap.md) · [Changelog](CHANGELOG.md)

These docs describe the branch/tag you are viewing. Features on `main` may need a source build;
use the documentation at your release tag for packaged binaries. Contributor setup and the
pinned Rust/Git requirements are in [CONTRIBUTING](CONTRIBUTING.md).

Contributions are welcome. Maintainers review and merge; see [CONTRIBUTING](CONTRIBUTING.md),
[SECURITY](SECURITY.md) and the [Code of Conduct](CODE_OF_CONDUCT.md).
For setup help or hosted Hub interest: support@counterbranch.com.

AgentSCM is **source-available under FSL-1.1-ALv2**; see [LICENSE](LICENSE).
AgentSCM and Counterbranch are trademarks of DUVATL, Inc.; see [TRADEMARKS](TRADEMARKS.md).
Other product names describe compatibility and belong to their respective owners.
