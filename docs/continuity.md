---
title: "Continuity preparation"
---

# Continuity preparation

> **Experimental, macOS and Linux.** Keep task context, local command receipts and saved
> publication drafts while GitHub, GitLab or your connection is unavailable.
> This feature prepares a handoff to your normal Git and review workflow. It does
> not push branches, open pull requests or merge requests, or merge changes.

Use an existing Git checkout and Git 2.45 or newer. No native source migration or running Hub is needed
to retain context, run an explicit local command, inspect a task or save a draft.
A live reconnect check needs the checkout's existing AgentSCM Hub connection and
a Hub that supports continuity observations.

## Save the task before or during a disconnection

Put the task description in a local text file outside the repository, then run:

```sh
agentscm continuity --repo . start \
  --id repair-parser --base refs/remotes/origin/main \
  --provider github --repository acme/widgets \
  --context-file /absolute/path/to/task.txt
```

For GitLab, use `--provider gitlab --repository group/subgroup/project`.
For a self-managed GitLab instance, also supply `--host gitlab.example.com`.
The declared host and repository must match the checkout's origin; the provider
is verified against the saved Hub connection during a live observation.
Use a canonical HTTPS or SSH origin. Local journal reads ignore global Git URL
rewrites and `safe.directory` overrides; shorthand origins may be refused.
Generic Git servers without a supported forge connection are outside this flow.

The selected base must already exist locally. Starting a task does not fetch it.
The retained timestamp describes when local facts were captured, not when the
forge was last reached. Task context is supplied by the caller; it is not an
automatic cache of issues, reviews or shared coordination state.

## Run a local check and inspect its receipt

```sh
agentscm continuity --repo . check --id repair-parser \
  --operation parser-tests --timeout-seconds 300 -- /absolute/path/to/check
agentscm continuity --repo . status --id repair-parser
agentscm continuity --repo . revalidate --id repair-parser
```

The command runs with the local user's authority through the existing bounded
capture runner. Only run commands you trust. This is not a sandbox, an independent
review or a replacement for hosted CI. Dependencies, runtimes and any models the
command needs must already be usable on the computer.

Each operation records its command and source binding before execution. Reusing
an operation ID does not run it again. An interrupted attempt remains unknown;
inspect it before deliberately starting a new operation. Local source changes
make earlier receipts stale or unverified. Capture omissions, ignored inputs,
external dependencies and environment changes limit what a source comparison
can establish. A successful process exit does not establish approval.

## Save an exact PR or MR draft

After committing the intended changes through your normal reviewed process:

```sh
agentscm continuity --repo . draft --id repair-parser --draft parser-review \
  --source-branch fix/parser --target-branch main \
  --title 'Fix parser recovery' --body-file /absolute/path/to/pr-body.md
```

A draft requires clean local source and retains the exact commit, tree, base tip,
branch names and description. GitHub drafts describe pull requests; GitLab drafts
describe merge requests. These are private local records, not draft PRs/MRs on
the forge. Later commits do not silently update them. Save a new draft ID for a
new revision, and cancel the older draft explicitly:

```sh
agentscm continuity --repo . cancel-draft \
  --id repair-parser --draft parser-review
```

## Reconnect explicitly

```sh
agentscm continuity --repo . revalidate \
  --id repair-parser --draft parser-review --live
```

This reads the configured Hub and forge. It checks the connected repository's
numeric identity and observes the source and target branch tips. It reports
missing branches, changed tips and local applicability separately. An outage,
expired token, denied access, rate limit or incomplete response leaves remote
state unverified. It never substitutes retained remote data for a fresh read.

Observations are not atomic: another developer can change a branch after it was
read. No reconnect result grants publication or merge authority. An unpublished
source branch is reported as missing, not silently created.

A zero exit status from `revalidate` means it returned an observation; inspect its
freshness and blockers. A check marked `current` matches the captured source; its
separate recorded exit code and outcome still determine whether the command passed.

Inspect the saved draft and resolve any changed source or target through your
usual Git workflow. Fetch and reconcile deliberately, rerun affected checks and
obtain the reviews and approvals required for that exact change. Then publish
through your existing GitHub or GitLab tooling. Continuity performs no remote
mutation and has no automatic retry or duplicate-publication mechanism.

## Private storage and limits

By default, records live under the user's configuration directory in
`agentscm/continuity`, separated by checkout. `--state-dir` selects an absolute
private directory outside the checkout. Context, draft descriptions, command
receipts and captured artifacts stay local; live revalidation sends branch names
to the connected Hub, not task text or command output.

Use a private local filesystem that supports file locking and atomic no-replace
rename. Unsupported filesystems fail initialization. Local source inspection
disables Git lazy fetch; missing objects must be obtained separately before use.

Records remain until explicitly deleted. Inspect retained information with
`status`, and delete a completed task with:

```sh
agentscm continuity --repo . delete --id repair-parser
```

Deletion refuses unknown attempts by default. After inspecting and dealing with
any interrupted command, `delete --id repair-parser --discard-unknown` explicitly
discards that task's retained records. It does not stop a command that survived
its supervisor, undo its effects or make its outcome known.

An abrupt termination during `start` can leave an unbound `spool-ID` directory
or `.continuity-init-*` file before a task is recorded. A repeated start never
adopts that spool. After stopping all commands using this state directory and
confirming `status --id ID` reports no task, inspect and explicitly remove only
that ID's residual spool. Temporary initialization files can be removed under
the same stopped-command condition. These residues can contain private source.

If deletion reports that a spool was replaced, it preserves the replacement.
Stop callers and inspect the old state before manual recovery; do not delete an
unrecognized replacement. A fresh `--state-dir` lets new work proceed while the
old private state remains available for inspection. Normal task deletion never
claims that an interrupted command's effects have been undone.

This is not an independent backup of the repository, Git LFS objects, issues or
review history. It does not retain collaborative locks, enable disconnected
multi-user coordination, configure a GitLab Hub, or make cloud agents work
without connectivity. Local fixtures cover the new behavior; live forge
qualification and other operating systems remain separate coverage limits.
