# Concepts in plain English

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

The main user-facing AgentSCM terms, each with a one-line Git comparison. The
comparisons are approximate; where the difference matters, it is stated. See
the [specification](spec/agentic-scm.md) for the precise model and what is
implemented, and the [roadmap](roadmap.md) for status.

Conventions: **AgentSCM** is the product and `agentscm` is the command. A
**Change** (capital C) is the unit of work and a **Change version** is its
counter; **generation** names only internal counters. A **revision** is a native
object and a **snapshot** is a Git capture.

## Source

| Term | Plain English | In Git terms this is like… |
|---|---|---|
| **Repository** | One project's AgentSCM data, identified by a random ID created by `init`. | A `.git` directory, but its ID is fixed and does not depend on any remote. |
| **Revision** | A native object: an exact, unchangeable record of every file, with its parent revisions. Identified by a SHA-256 hash. Not the same as a Git snapshot, which is a capture of a Git commit. | A commit, minus author, message and date. Native revision IDs differ from Git commit IDs. |
| **Checkpoint** | Saving the current files as a new revision. Cheap; do it often. | `git commit` with no message. |
| **Data directory** | The folder (`--data-dir`, default `.agentscm`) that holds one native repository's source store and integration authority. | A `.git` directory, in the sense of where the state lives. |
| **Workspace** | A working directory tied to one line of work, with crash-safe undo. | A `git worktree`. |
| **Diff** | The file differences between two revisions, read page by page. | `git diff A B`. |
| **Merge session** | A saved, resumable merge: common ancestors, conflicts and your chosen resolutions survive restarts. | `git merge` with conflicts, except the in-progress state is durable and inspectable. |
| **Bundle / offline exchange** | Copying exact source objects to another machine (offer → request → bundle → import). Carries source only, never approvals. | `git bundle`. |

## Work and review

| Term | Plain English | In Git terms this is like… |
|---|---|---|
| **Change** | One piece of work with a stable name that survives new revisions, rebases and context updates. It is not a diff or an edit. | A pull request, or a Gerrit Change-Id. |
| **Change version** | A counter that goes up each time a Change's review context is replaced (`change_version` in JSON). Approvals name the version they saw. Not the same as a *generation*, the internal counter that fences a workspace or a check run. | The PR's "head changed since approval" check, as a number. |
| **Review context** | The brief a reviewer approves against: intent, acceptance criteria, files in scope, exclusions, known omissions, required checks. Immutable; editing it makes a new one. | The PR description, but frozen and hashed, so an approval proves which description it saw. |
| **Context digest** | The SHA-256 hash that identifies one exact review context. | A commit hash, for the PR description. |
| **Check / recipe / attempt** | A check is an automated test of a revision; the recipe is its exact definition; an attempt is one run of it. Two kinds exist: a built-in check that verifies source file hashes, and a command check that runs a declared command in an isolated container (Unix with Docker, experimental). | A CI job definition and one CI run. |
| **Isolated check** (execution) | Experimental. A command check run in a locked-down container: no network, read-only source, no credentials, fixed limits. The result comes from what the controller observed, never from what the command says about itself. | A CI job on a sandboxed runner, bound to one exact commit. |
| **Agent candidate** | Experimental. A new revision built from the files an agent printed at the end of a successful isolated run. It still needs its own review and approval. | A bot's suggested commit, not yet in any pull request. |
| **Evidence** | A record of what a check observed for one exact subject. Evidence is not permission. | A CI status on a commit, bound to the exact subject and recipe. |
| **Approval** | An administrative yes (today local and unsigned) to one exact revision, review context, Change version, target and policy epoch. Used once. | A PR approval that is automatically dismissed by any new push, description edit or rule change, and cannot be reused. |
| **Policy epoch** | A version number for the repository's rules. Raising it invalidates every approval given under older rules. | Changing branch protection rules and forcing everyone to re-approve. |

## Landing

| Term | Plain English | In Git terms this is like… |
|---|---|---|
| **Target** | A protected name such as `main` that points to one revision, with its own version counter. | A protected branch. |
| **Target version** | A counter that goes up on every move of the target, so "it moved away and back" is still detected. | The ref's old value in `git update-ref`, plus a counter. |
| **Landing / integrate** | Moving a target to an approved revision, only if the target is still where you expected. One transaction: check, move, consume approval, record event. | Merging a PR with `--force-with-lease` semantics and branch protection enforced in the same step. |
| **Expected target** | The target's revision and version that a landing request says it expects to replace (or `null` when the target must not exist yet). A mismatch is a conflict, never an overwrite. | The old value in `git update-ref <ref> <new> <old>`, plus a counter. |
| **Initialize target** | Creating a target that does not exist yet, under the same rules. | Pushing the first commit of a new protected branch. |
| **Prepare landing** | When your work started from the current target, building the exact revision that would land (parents: target, your tip) and binding a fresh review context to it. It does not merge diverged work. | Creating the merge commit a PR would produce, so it can be reviewed before merging. |
| **Journal** | An append-only local log a command keeps so an interrupted operation can be inspected and retried. | A reflog that never expires. |
| **Integration event** | The append-only record of one landing: before and after, approval, context, operation. | A reflog entry that never expires and records why the branch moved. |
| **Operation ID** | A name you choose for a request, so a retry after a lost reply returns the original result instead of acting twice. | An idempotency key; Git has none. |
| **Operator** | The person who runs and owns a local AgentSCM installation. In the trusted-local profile the operator is trusted and supplies every administrative claim, so approvals are not signed. | The owner of a local repository. |
| **Ledger / authority** | The store of immutable revisions, contexts and events, plus the lifecycle records (targets, approvals, policy epoch) that decide what lands. | No Git equivalent; the forge database is the closest, and it is mutable. |

## Working alongside Git

| Term | Plain English | In Git terms this is like… |
|---|---|---|
| **Shadow** | Running AgentSCM next to an existing Git repository while GitHub or GitLab stays in charge. Nothing is written to Git. | A read-only mirror used for a trial. |
| **Snapshot** (Git capture) | One clean Git commit captured as one native revision, without its history. The capture is the snapshot; the native object it produces is a revision. Needs Git 2.45 or newer. | `git archive` of one commit, imported. |
| **Reconcile** | Comparing a Git commit with the native target and recording whether they match, which side moved, a conflict, or unknown. | `git status` between two repositories, saved with a result. |
| **Baseline** | The last Git commit and native revision that reconcile verified as matching. | No direct equivalent: a remembered pair of equal trees, not a merge base. |
| **Candidate revision** (reconcile) | A native revision that reconcile captured from a Git-side change, waiting for review and landing. | A fetched commit not yet merged. |
| **Forge** | The hosted service around your Git repository: GitHub, GitLab. | — |
| **Export** | Writing a landed native revision as a commit into a separate local bare Git mirror, with a compare-and-swap on its ref. Never pushes. | Committing into a mirror you own and moving its branch with `git update-ref <ref> <new> <old>`. |
| **Mirror** | The separate bare Git repository that export writes to. AgentSCM owns one branch in it (`refs/heads/agentscm`). | A bare clone used only as a staging remote. |
| **Prepare exit** | Exporting a reconciled native state to the local mirror and recording that handoff. It changes no authority. | Pushing to a staging remote you own, with a lease. |
| **Cutover** (proposed) | An explicit, recorded decision, per target, that AgentSCM rather than the forge decides what lands on that target. Optional and reversible; Git and the forge remain fully supported, and nothing requires it. See the [path to cutover](spec/agentic-scm.md#7-path-to-cutover). | Moving branch protection for one branch from GitHub to another system. |

## Many agents and the hub

| Term | Plain English | In Git terms this is like… |
|---|---|---|
| **Hub** | The shared service several people and their agents connect to (`agentscm hub serve`). It holds agent tokens and intents, and records sign-ins and every change in an event log. | The coordination part of a forge, minus code hosting. |
| **Forge identity** | Who you are on the hub: your GitHub or GitLab account, identified by its numeric user ID, so renaming your login keeps you the same person. | Your GitHub or GitLab account. |
| **Administrator** | A hub user named in the hub's configuration (not stored on the user record). Administrators set up forge connections and can disable users. | A GitHub organisation owner. |
| **Sign-in session** | What `agentscm hub login` gives a person after signing in with GitHub or GitLab: at most 8 hours, kept in memory on the hub (it ends if the hub restarts) and in a private file on the person's machine. It mints and lists agent tokens; an agent token cannot. | A browser login, as opposed to a personal access token. |
| **Agent token** | A key a person creates for one agent and one repository. It expires within 30 days and can be revoked; everything the agent does is attributed to the person who created it. | A fine-grained personal access token. |
| **Forge connection** | A hub's link to GitHub or GitLab: a GitHub App or a personal access token (GitLab: an access token), plus the repositories it covers. The hub polls it (every 5 minutes by default) and receives webhooks. A person can mint an agent token only for a connected repository. | A GitHub App installation. |
| **Observation** | A fact the hub or a tool records about something it read. The hub observes open pull or merge requests and recent branches through a forge connection, which feed overlap notices. A reconcile observation compares one Git commit with the native target. Both only record what was seen. | A fetch, with the result written down. |
| **Outbound** | An opt-in, per-connection setting, off by default (`agentscm hub connections outbound <id> on\|off`). When on, the hub writes one comment and one commit status on pull requests that overlap a live intent, and publishing needs it. | Letting a bot comment on your pull requests. |
| **Publication** | Experimental. Sending one exported native revision to GitHub as a new branch and pull request through the hub (`agentscm hub publish`). It reports separately whether the revision is natively landed, whether the branch exists and whether the pull request is open or merged. GitHub only. | Pushing a branch and opening a PR. |
| **Event log** | The hub's record of sign-ins, token changes, intents and notices. General events cannot be edited or deleted; stand-up work events have explicit [operator retention](standup.md#storage-maintenance). | An audit log. |
| **Intent** | A short-lived note an agent posts to the hub: "I'm about to change these files for this task". Advisory, never a lock. | Nothing; Git has no shared "I am working on this" signal. `git lfs lock` is the closest, and it blocks. |
| **Machine label** | The name of the computer shown in intents and warnings. Set with `AGENTSCM_MACHINE`; the default is the hostname. | The hostname in a reflog or CI runner name. |
| **Claude Code hook** | The hook `agentscm client setup` adds to Claude Code (it runs `agentscm hook claude --client-connection`), or the manual example in `hooks/` that runs `agentscm intent`, so that before an edit overlapping work produces a warning. | A pre-edit Git hook. |
| **Directory spec** | An intent path ending in `/**`, meaning everything under that directory. Any other path means exactly that file. | A pathspec like `src/`. |
| **Notice** | A message the hub keeps for an agent when another intent is announced that overlaps its own, naming who and which files. The agent collects it by asking (long-poll); nothing is pushed. | A notification that someone opened a PR touching your files, but before they write the code. |
| **Collision scan** | `agentscm collisions`: a check of which branches and worktrees in one Git repository change the same files, and whether committed ones would conflict in a Git merge. Same-file overlap is a warning, not proof of conflict. | Running `git merge-tree` between every pair of branches before anyone opens a PR. |
| **Poll** | One round in which the hub reads each forge connection (`--poll-seconds`, default 300, at least 10). A comment or status written by outbound can lag a change by up to one poll. | A scheduled `git fetch`. |
| **Object storage** | Where the hub keeps file contents: a local directory (default) or an S3-compatible bucket chosen with `AGENTSCM_OBJECTS_URL`. | The object database of a bare repository. |
| **Conformance level** | How much of the spec a tool implements: L0 observe Git, L1 ledger + shadow, L2 native authority. | — |

## Status labels

| Term | Plain English |
|---|---|
| **Alpha** | The whole project's maturity: not for production, and APIs, CLI, storage and schemas may change without migration. |
| **Implemented** | In this repository's code and tests. The native ledger is the trusted-local, single-operator profile; hub features are an alpha multi-user service. Not a production or scale claim. |
| **Experimental** | Implemented, but exercised only against fakes or a narrow environment, with the limits stated: isolated command checks, GitHub publication, and GitHub and GitLab observation. |
| **Partial** | Some of the behavior exists; the gap is named. |
| **Proposed** | Described, with no implementation yet. |

These match the [roadmap](roadmap.md) and the [specification](spec/agentic-scm.md).
