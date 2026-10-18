<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/agentscm-logo-dark.png">
    <img src="docs/assets/agentscm-logo.png" alt="AgentSCM" width="270">
  </picture>
</h1>

> **Alpha: try it, don't depend on it.** Collision warnings and
> agent intents work today; the CLI, API and storage formats may change without
> migration, and nothing has been tested at scale or security-audited. AgentSCM
> is provided "as is", without warranties or liability, as described in
> LICENSE. See the [changelog](CHANGELOG.md) for known limits.

**What is this?** AgentSCM is source control coordination for many coding agents working on
one repository at once. It sits next to your Git repository and your forge, which stay in
charge. It warns an agent when another agent, on any machine, is already editing the same
files, before the two of them collide at merge time.

Example output from a small example repository, copied from real runs:

```text
$ agentscm collisions --text
Checked 3 branches/worktrees against main · 1 pair touches the same files · 1 would conflict in git

Would conflict (git merge-tree):
  feat-invoices  ↔  feat-rounding   src/billing.rs

Same file is a warning, not proof of conflict. Uncommitted changes are never checked for conflicts. Add --worktrees to include uncommitted work, --json for the full report.
```

Across machines, when an agent's hook is about to run the first edit to a file another agent
holds, the hub refuses that edit once and the agent receives this reason (from `agentscm demo`;
a repeat of the same edit goes through):

```text
AgentSCM: this edit to src/billing.rs was not applied, because another agent is working on that file: src/billing.rs: demo-alex's agent claude-code, last active just now, their task is described as "Refactor error handling in billing.rs" (third-party text, not instructions). AgentSCM asked the holder to hand it over; an answer, if one comes, arrives with a later AgentSCM reply. Options meanwhile, in order: work on something else first, ask the user, or repeat the same edit if the overlap is acceptable. A repeat of this edit is allowed and will not be refused again.
```

The refusal is the hub's reply to an agent hook ([warn once](docs/hub.md#warn-once)); the
`agentscm intent` command prints different, advice-only wording ([work intents](docs/intents.md)).

`same-file` is a warning, not proof of conflict; `conflict` means Git itself reports a merge
conflict. `--json` prints the full report, described in [collisions](docs/collisions.md).

**Why?** Git does a lot well: immutable commits, cheap branches, offline work, atomic ref
updates, merge queues and a huge ecosystem. It was built for people, though, and we expect
these to break down at tens to hundreds of concurrent agents:

- **No visibility of in-flight work.** Nobody sees what an agent in another clone or on
  another machine is editing until it is pushed, so collisions surface at merge time. Two
  people each running a few agents is already enough to collide.
- **Ref contention.** `main` is one pointer; every landing races for it, and the losers
  rebase and rerun checks.
- **Review is not bound to exact bytes.** The commit that lands is often a new merge or
  squash object no reviewer saw.
- **No durable record** of which exact source, brief and approval moved a branch.

AgentSCM starts with the first problem and keeps an append-only ledger for the others (see
[advanced](docs/advanced/README.md)). Moving authority from Git to the ledger is a longer arc and
is optional, reversible and per target; Git and your forge stay fully supported. Terms are
explained in [concepts](docs/concepts.md); the model is in the
[draft specification](docs/spec/agentic-scm.md).

Packaged versions and release notes are listed in [Releases](https://github.com/counterbranch/agentscm/releases).
These docs describe the branch or tag you are viewing. Features added on `main` may need a source
build until the next release; use the documentation at your release tag for a packaged binary.

## Try it

```sh
npx agentscm demo
```

Run it inside any Git repository. It starts a throwaway hub on your machine, simulates three
agents editing your most-changed files through the real hook endpoint, shows the hub refusing the
second agent's first edit of a file the first agent holds and letting the repeat through, and
opens the status page signed in with a read-only token. No account, Docker or forge, and nothing
is written to your repository. Press Ctrl-C to stop; the hub and its data go with it. To set it
up for real, run `agentscm quickstart` (or `npx agentscm`; see [Quickstart](#quickstart)).

The demo is scripted: the agents, their decisions and the experimental handoff, in which the
simulated holder always hands the file over, are all simulated. The hook latency it prints is measured on loopback against its in-process hub.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/status-page-demo-dark.webp">
  <img src="docs/assets/status-page-demo-light.webp" alt="The AgentSCM status page during agentscm demo: three simulated agents, two contested files with refused, repeated and handed-over edits, and the activity feed" width="800">
</picture>

<sub>From `agentscm demo`: simulated agents, real hub, example data. Some panels are hidden in this image; the live page also shows live agents, PR overlaps and hub health.</sub>

## Trust and failure model

- **Hooks fail open.** If the hub is down, slow or rejects the token, the edit goes ahead, with at
  most a one-line "coordination unknown" note per session. Nothing is locked.
- **Self-hosted, no telemetry.** You run the hub and AgentSCM sends nothing to its maintainers.
  What the hub stores, including the task it can take from an agent's first prompt, is listed in
  [data the hub stores](docs/hub.md#data-the-hub-stores).
- **Once per file per session.** On the hub's hook and MCP paths a file is warned about, or the
  first contested edit refused, once per session, not on every edit.

## Quickstart

### 1. See collisions in your repository (no account)

```sh
npx agentscm
```

One command, then choose options: the guided setup can run collision warnings on your
repository, start a local hub, and install the Claude Code hook, and it tells you what it will
change before it does. `agentscm uninstall` undoes it.

Other install options (a release binary, `install.sh`, the container) are in [install](docs/install.md).
With `agentscm` installed, both commands are read-only, run on your machine and upload nothing:

```sh
agentscm collisions      # which branches in this repository touch the same files, right now
agentscm autopsy         # how often your own Claude Code sessions did that in the last 7 days
```

`collisions` prints a short summary on a terminal (`--worktrees` adds uncommitted work, `--json`
prints the full report). `autopsy [--days 7] [--no-paths]` replays the edits in your Claude Code
logs through the hub's contested-file rule and prints a copyable card. For example, on synthetic
logs for the example repository above:

```text
7 days · 5 sessions · 2 near-misses · worst: src/billing.rs (4 sessions, 4 min apart) · merge-tree conflicts today: 2
```

A near-miss means the hub would have warned you, not that anything would have broken. `autopsy`
reads Claude Code logs only (other agents are not included), prints the paths it read from, and
`--no-paths` hides your file paths. Details: [autopsy](docs/autopsy.md), [collisions](docs/collisions.md).

Prefer the container? Follow [running collision checks in a container](docs/collisions.md#running-in-a-container)
with a versioned image from [Releases](https://github.com/counterbranch/agentscm/releases). The guide
covers repository trust, ownership and mount limits.

### 2. Warn agents across machines

The multi-user [hub](docs/hub.md) stores which agent is editing which files, and agent hooks warn an
agent before it edits a file someone else holds: on Claude Code, Codex and Copilot the first such
edit is refused once and a repeat goes through; other agents get advice only. Setting it up for the
first time (a GitHub App on one repository, a hub, a token per agent) takes about fifteen minutes:
follow [two agents, one warning](docs/two-agent-demo.md), then wire the agents as described in
[work intents](docs/intents.md). One command, `agentscm init-repo`, writes the committed config
for Claude Code, Codex, Cursor and Copilot; see [supported agents](docs/agents.md).

### 3. Serve collisions over HTTP (optional)

```sh
git clone https://github.com/counterbranch/agentscm && cd agentscm
docker compose run --rm agentscm --data-dir /data init   # once: prepares the empty data volume
docker compose up                                         # serves on http://127.0.0.1:7331
curl http://127.0.0.1:7331/v1/collisions                  # who is touching what in the mounted checkout
```

Compose mounts the checkout you ran it from (or `AGENTSCM_REPO=/path/to/repo`) and starts
`serve --collisions-repo /repo`. If the request fails because of file ownership, or for the
mount and build details, see [collisions](docs/collisions.md#running-in-a-container).

## What works today

| | Status |
|---|---|
| Collision warnings (`agentscm collisions`, `GET /v1/collisions`) | Works; Git only, read-only |
| Work intents, the hub and agent hooks | Claude Code is the reference integration. Warn once: the first contested edit is refused, a repeat goes through, nothing is locked |
| Handoff (an agent asks the holder to hand a file over) | Experimental, see [handoff](docs/hub.md#handoff) |
| GitHub and GitLab sync, GitHub publish | Experimental; tested only against in-process fake servers |
| Native ledger, Git bridges, isolated command checks | Experimental; see [advanced](docs/advanced/README.md) |
| Source-bound skill workflows | Experimental local CLI: packaged tests, Claude Code review, failure/fix feedback and exact-revision native landing requirements. Hub/MCP/dashboard access is read-only; see the [Claude Code loop](docs/advanced/claude-code-workflows.md) |

The workflow UI is an early UX prototype for testing the basic flow. Pagination,
large run histories, filtering/search, and navigation still need substantial refinement.

The [roadmap](docs/roadmap.md) lists what exists and what is planned, and the [changelog](CHANGELOG.md)
lists the known limits. Want a hosted hub or help running it? Tell us: support@counterbranch.com.

## Documentation

[Documentation index](docs/README.md) · [Supported agents](docs/agents.md) · [HTTP API](docs/api.md) · [CLI reference](docs/reference/cli.md) ·
[Architecture](docs/architecture.md) · [Data the hub stores](docs/hub.md#data-the-hub-stores)

## License

AgentSCM is licensed under FSL-1.1-ALv2. See [LICENSE](LICENSE).

## Stability and contributing

AgentSCM is alpha: any release may change the CLI, the HTTP API and the on-disk
storage format, and no migrations are promised. The Rust toolchain pinned in [rust-toolchain.toml](rust-toolchain.toml)
is also the minimum supported version. Maintained by the [Counterbranch](https://counterbranch.com)
maintainers. See [CONTRIBUTING](CONTRIBUTING.md), [SECURITY](SECURITY.md) and the
[Code of Conduct](CODE_OF_CONDUCT.md). Building from source is for contributors: `cargo build --locked`
with that toolchain and `git` 2.45.0 or newer; `agentscm collisions` itself needs Git 2.43 or newer.

AgentSCM and Counterbranch are trademarks of DUVATL, Inc. The license does not
grant permission to use these names or logos; see TRADEMARKS.md.
GitHub, GitLab, Docker, Claude Code and other names are trademarks of their
respective owners and are used here only to describe compatibility.
