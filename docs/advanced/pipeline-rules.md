---
title: "Operator-local repository pipeline rules"
---

# Operator-local repository pipeline rules

This experimental Unix CLI installs repository merge and pull request rules and feeds the existing
skill-workflow journal. Git and GitHub/GitLab retain source, review and merge
authority. It does not install native landing requirements, publish artifacts,
create PRs/MRs or merge anything. The shared Hub does not yet manage these rules.

Use a build containing the `pipeline` command. Initialize a separate operator data
directory with `agentscm --data-dir /srv/agentscm/state init`. Keep it outside the
Git checkout and common Git directory. Its `origin` must identify the installed forge host and repository over HTTPS or SSH; a mismatch blocks capture without echoing the URL. The source checkout may remain dirty: the
runner captures the exact accepted commit using the existing immutable Git
snapshot reader. It never checks out, resets or edits that checkout and never
fetches objects automatically. Existing Git snapshot format, object, symlink,
submodule, path and size restrictions still apply. Git 2.45 or newer is required.

## Install and preview

The operator supplies a strict installation manifest outside candidate source.
The `recipe` is the existing workflow graph with its source fields omitted. For
productive action and profile declarations, see [productive workflows](productive-workflow-actions.md).
For example, a one-step recipe can use this manifest, after replacing the full
commit, numeric forge repository ID, paths and recipe resources with reviewed
values:

```json
{
  "repository": "acme/widgets",
  "provider": "github",
  "host": "github.com",
  "provider_repository_id": 42,
  "rule_id": "release-notes",
  "enabled": false,
  "trigger": { "target": "main", "labels_all": ["release"] },
  "recipe": {
    "version": 1,
    "name": "release-notes",
    "skill": {
      "entrypoint": "skills/release/SKILL.md",
      "resources": ["skills/release/draft.json", "skills/release/draft.md"]
    },
    "steps": [
      {
        "id": "draft",
        "needs": [],
        "required": true,
        "action": { "kind": "agent_action", "resource": "draft.json" }
      }
    ]
  },
  "profile_path": "/srv/agentscm/operator/profile.json",
  "git": "/usr/bin/git",
  "git_repository": "/srv/source/widgets",
  "source_commit": "0123456789abcdef0123456789abcdef01234567"
}
```

The profile is an explicit operator-owned regular file outside any Git checkout,
not writable by group or other users. Installation and active delivery check the
same bounded profile-file configuration. Commands in
it exercise the operator account's privileges; this mode is not a sandbox for
hostile programs. Resource prose and action inputs cannot choose executables or
expand the installed profile. Installation captures the selected local commit,
compiles the graph and pins the recipe, selected resource bytes and profile
configuration. Future accepted commits may change ordinary source inputs but
must retain exactly those selected skill resource bytes.

```sh
agentscm --data-dir /srv/agentscm/state pipeline install install.json
agentscm --data-dir /srv/agentscm/state pipeline status
agentscm --data-dir /srv/agentscm/state pipeline preview acme/widgets release-notes preview-event.json
agentscm --data-dir /srv/agentscm/state pipeline set-enabled acme/widgets release-notes --expected-generation 1 --enabled true
```

Preview accepts a synthetic `ConfirmedMerge` JSON file with `provider`, `host`,
`repository`, `provider_repository_id`, `proposal_number`, `accepted_commit`,
`target`, `labels`, `observed_at` and optional `delivery_id`. It marks the result
unverified, returns repository/target/label/disabled reasons and never accepts an
event or starts work. `observed_at` is Unix milliseconds. Matching uses all
required labels and an exact target branch. Labels describe observation time;
they do not claim to be historical merge-time labels.

`update install.json --expected-generation N` replaces an existing rule and pins
the newly reviewed recipe/resources/profile. `set-enabled` and `update` both
require the expected generation; an outdated caller gets a conflict. The stable
`rule_id` remains the deduplication identity. Keep it when editing settings.
Neither editing nor re-enabling replays an event already observed for that rule,
including an event first observed while disabled or mismatched. There is no
implicit or explicit merge replay command.

## Verify observations and run locally

A second operator JSON file configures the existing forge client:

```json
{
  "provider": "github",
  "host": "github.com",
  "api_url": "https://api.github.com",
  "web_url": "https://github.com",
  "repository": "acme/widgets",
  "provider_repository_id": 42,
  "token_env": "AGENTSCM_PIPELINE_FORGE_TOKEN"
}
```

The credential comes from that explicit environment variable and is not stored in
rules or event records. GitLab uses `provider: "gitlab"`, its host/web URL and
`api_url` ending in `/api/v4`. Self-managed endpoints must use HTTPS on the
configured host. The local observer uses the same provider clients as Hub sync;
this file is a local operator connection, independent of multiuser Hub settings.

```sh
agentscm --data-dir /srv/agentscm/state pipeline observe --connection forge.json --proposal 17
agentscm --data-dir /srv/agentscm/state pipeline poll --connection forge.json
agentscm --data-dir /srv/agentscm/state pipeline pickup
agentscm --data-dir /srv/agentscm/state pipeline watch --connection forge.json --interval-seconds 300
```

Observation re-reads both the repository identity and numbered PR/MR through the
configured forge. Every proposal response must independently identify the target
repository: GitHub base-repository ID/name or GitLab target-project ID. Missing or
changed target identity blocks acceptance, including a name reused mid-scan. Caller-provided hints are never accepted as confirmed facts.
An open or unmerged proposal starts no merge rule; a [pull request rule](#pull-request-heads)
can start on an open head. GitHub requires its confirmed
`merge_commit_sha`. GitLab uses the merge or squash commit; a fast-forward without
either is verifiable only while the live target tip still equals the confirmed
MR head. A later target tip leaves that observation blocked.

`poll` discovers the newest 100 updated closed GitHub PRs or merged GitLab MRs.
It verifies the repository identity once per scan and re-reads each unconfirmed
candidate. An exact proposal/accepted-commit hint already confirmed in the journal
can be skipped; listing data alone never starts work. Observation timestamps are
not cutoffs: closed-unmerged and reopened proposals continue to be checked.
`watch` polls every 300 seconds by default and picks up pending work. Smaller
explicit intervals increase provider requests. A window of 100 unconfirmed
GitHub proposals can require 102 requests (repository, listing and proposals).
GitLab fast-forward proof can add one target-ref read per proposal, reaching 202
requests for that path.
It is opt-in and runs in the foreground; an operator can run it under their own
service manager for background operation. `watch --once` performs one cycle;
without a connection it only picks up accepted local work. No login service is
installed automatically. The first poll can observe already closed proposals;
inspect the rule and preview before enabling it.

The bounded poll retains its last observation timestamp and unread-page count.
It is not a full provider-history cursor: bursts, long offline periods or older
closed proposals can exceed the 100-item window. An unread count is a coverage
gap. Use `observe --proposal N` for a known older merge; complete historical
reconciliation and webhook installation are not implemented. An unverifiable
proposal (including an advanced GitLab fast-forward target) gets its own visible
blocked observation with no event accepted; later proposals are still checked.
Malformed listing entries are reported by list position without fabricating a
proposal number. Such a scan stays incomplete, retains its first unverifiable
reason at the top level and does not advance the last successful checkpoint,
even when later valid proposals are accepted. Authentication, rate-limit and
transport failures stop the scan and supersede that top-level reason. Previously
confirmed acceptances remain durable. Authentication failures pause observation until the
watcher is restarted with a corrected credential. Provider rate limits defer
observation until a validated retry time (at most one day ahead); an absent or
invalid retry time requires an operator restart. Pending local pickup continues.
Other failed reads may be observed on the next cycle.

Accepted events and their frozen outbox items commit in one SQLite transaction.
No running worker is needed for acceptance. Pickup derives one stable workflow
run ID from provider, host, stable repository ID, proposal number, accepted commit
and stable rule ID. Settings generation and delivery IDs are excluded. The first
observation freezes its rule configuration. An event's fixed named input fields
are `accepted_commit`, `proposal_number`, `target`, `labels` (a JSON string array),
`repository`, `provider`, `host` and `observed_at`. Productive declarations can
reference one with `{"kind":"event","field":"accepted_commit"}`. They are
immutable data, not conditions, commands or permissions.

## Pull request heads

A rule with `"event": "pull_request"` fires on an open, non-draft GitHub pull
request into `target` whose head is in this repository, once per head commit. A
fork's head never starts a rule. GitLab rules cannot use this event. Optional
predicates narrow it, and every one must hold:

```json
"trigger": {
  "target": "main",
  "event": "pull_request",
  "labels_all": ["security"],
  "paths_any": ["Cargo.lock", "**/Cargo.toml", "src/auth/**"],
  "min_changed_files": 1,
  "min_changed_lines": 10,
  "run_when_unknown": false
}
```

- `paths_any`: at least one changed path, a rename's old path included, matches
  one of up to 32 globs. A glob matches the whole repository path: `*` and `?`
  stay within one directory and `**` spans directories, so `*.md` matches only
  root-level Markdown and `**/*.md` matches every Markdown file. Write `dir/**`
  for a directory; a glob that starts or ends with `/` is refused.
- `min_changed_files` and `min_changed_lines` (additions plus deletions) are
  minimums from the forge's pull request counts.
- `labels_all` works as for merges, at observation time.

These predicates are refused on a merge rule.

Unknown stays unknown. The paths are unknown when:

- the pull request has more than 300 changed files (the client reads three pages
  of 100);
- GitHub's file reply is too large;
- one path is longer than 4096 bytes;
- or the list does not fit one 64 KiB journal event.

A missing count, and matching that exceeds its work limit, are unknown in the
same way. Preview then reports `could not evaluate ...`, lists the predicate
under `unknown`, and the rule does not run. With `run_when_unknown: true` the
rule runs anyway when every predicate it could evaluate holds, and preview still
lists what it could not evaluate. `poll` shows each rule's preview for every head
it reads, counts heads that were not evaluated in `not_evaluated`, and `status`
lists recent ones under `unevaluated`.

`observe --proposal N` confirms an open head as well as a merge. `poll` lists
open pull requests when any rule that is not policy-owned uses this event:

- Drafts and fork heads in the listing are not read.
- Each other head is read, its file list is read, and then the head is read again.
  If the head moved in between, nothing is accepted and the next poll reads it
  again.
- A head is skipped when every pull request rule has already evaluated that
  head, base branch and label set at the rule's current generation.

So retargeting, relabelling, editing a rule or re-enabling it evaluates a head
again, but one rule runs a head at most once. Older heads of the same pull
request, and heads of pull requests no longer open, are pruned when they started
no run. A head event is keyed apart from a merge of the same commit.

A write that would take the journal past its 128 MiB budget or a table's
100,000-row cap is refused and rolled back, with nothing accepted. When the
journal opens, an event, observation or dispatch row it cannot accept (oversized,
unparseable or invalid) is moved, with the rows that depend on it, to
`pipeline-rules/quarantine.jsonl`. `status` reports how many under
`quarantined`, so one bad row never stops the journal from opening. Problems with
the journal as a whole (identity, schema, integrity, binding or rules) still stop
it.

Preview takes the same synthetic JSON with `"kind": "pull_request"` and an
optional `changes` object (`files`, `lines`, `paths`; a missing value is
unknown). For a head, `accepted_commit` is the head commit, and the workflow also
gets the input field `event` set to `pull_request`. The dispatch keeps the
event, so status shows which rule and which head started each run.

Delivery uses the same executor, profile and limits as a merge rule. The head's
selected skill resource bytes must still equal the pinned ones, so a pull
request cannot rewrite its own rule's instructions. The runner never fetches:
the head commit must already be in the operator's checkout (for example after
`git fetch origin`), or delivery is blocked. Rules owned by an operating policy
receive merge events only.

### What the pull request shows

Each time `observe` or `poll` runs (and so each `watch` cycle), it posts a commit
status for every pull request run whose state the forge has not been told yet:

| Run state | Status on the head commit | Description |
| --- | --- | --- |
| accepted, waiting or running | `pending` | AgentSCM rule fired on this commit; the run has not finished |
| complete | `success` | AgentSCM rule run succeeded on this commit |
| blocked or failed | `failure` | AgentSCM rule run failed or was blocked; see pipeline status |

- The context is `agentscm/pipeline/<rule_id>`. The status is set on the exact
  head commit that ran, so a result for an older head never appears on a newer
  one. A new head shows nothing from this rule until the rule fires for it.
- GitHub keeps one status per commit and context, so all runs of one rule on one
  commit share it. This happens when two pull requests have the same head, or after
  a relabel or retarget. The status is `failure` if any of those runs failed,
  `pending` while any is unfinished, and `success` only when every one succeeded.
- `pending` goes out in the same cycle that accepts the run. `watch` posts the
  final state on its next cycle after pickup. A rule that fired always has a
  status, or a retried post failure.
- Descriptions are fixed text. Nothing from the pull request (title, body, branch,
  paths or labels) is put in a status.
- A post that fails (refused, rate limited, unavailable) is kept as unposted and
  retried on later cycles. Results that have never failed are posted first, so
  failing posts cannot hold back new heads. An answer that cannot change (HTTP
  422, for example a commit GitHub no longer has, or a 64-digit commit) is given up
  after three attempts. It stays listed with `gave_up: true` and is tried again
  only when the run's state changes. `status` lists it under `unposted_reports` with
  the attempt count and reason, and `observe`/`poll` output lists each attempt
  under `reports`. An authentication or rate-limit failure, or a refusal that
  applies to the whole token (HTTP 401, 403 or 404), stops that cycle's posting.
- The forge token needs permission to write commit statuses (`statuses: write`).
- Statuses are only written, never read back, so a posted status cannot start a
  rule again.
- When a journal moves to this version, runs that already existed count as
  reported, so an upgrade posts nothing on older, often closed, pull requests.
- `success` means the workflow run succeeded. Whether a finding fails the run is
  up to the workflow's own steps.
- **A required check is not a gate against people who can push.** Anyone with
  push access can set `agentscm/pipeline/<rule_id>` to `success` themselves. So
  can a workflow in the pull request, through the repository token, and the
  pull request's author controls that workflow file. A token-posted status
  cannot be pinned to this observer. If you require it anyway, set the default
  workflow token to read-only.
- A head gets no status when the rule does not fire for it: the rule doesn't
  match, or its changed files could not be read (`unevaluated` in `status`,
  unless the rule sets `run_when_unknown`). With the context required, such a
  pull request waits on "Expected" and can't merge until someone reviews why.
  Don't require the context unless the rule fires on every pull request.

Recovering a blocked run with `recover` sets the head back to `pending`.

**Who runs code on your machine.** A pull request rule runs the profile's local
commands and the skill agent on an unreviewed head. It runs them with the
operator account's privileges and credentials. Any account that can push a
branch to the repository can start that run, including bots and apps such as
Dependabot. The head's files can also try to steer the agent (prompt injection).
Before enabling one:

- Run it under a dedicated operator account that holds no credentials beyond the
  forge read token, on a machine or container you can discard.
- Keep the profile's commands to read-only analysis.
- Restrict who can push branches to the repository. A label is no gate: anyone
  who can push can also label.

## Inspect and recover

`status` shows configuration, the newest 256 outbox states, claim generations,
retained reasons and poll checkpoints. A `next_before` sequence provides the next
page through `status --before SEQUENCE`; `status --dispatch EXACT_ID` reads an
individual dispatch, so older work remains inspectable and recoverable. Pending items survive restart. One local runner lease
prevents simultaneous pickup/recovery owners; SQLite generation checks fence
stale claims. Delivery starts or adopts the same existing workflow run, binds the
confirmed inputs, then advances through the existing action attempt journal.
A process loss after start therefore cannot create a second logical run.

A removed/changed profile, disabled rule, changed selected resource, unavailable
commit or source-capture restriction blocks delivery with a retained reason.
Removing or modifying the old frozen profile also revokes pending work after a
rule update. Updating the rule alone does not change an already frozen delivery.
The runner rechecks the current enabled flag, claim generation and frozen profile
before each step and while an action is active. Disabling the rule or revoking
that profile cancels the owned process and blocks further delivery; cancellation
cannot undo effects already performed. Use the workflow cancellation command for
other retained-run cancellation.

Fetch missing source only through an explicitly reviewed operator command, for
example `git -C /srv/source/widgets fetch origin main`. The pipeline never runs
that command. After restoring the required local objects or old profile, requeue
the exact dispatch using the generation shown by `status`:

```sh
agentscm --data-dir /srv/agentscm/state pipeline recover pds_EXACT_ID --expected-generation 1
agentscm --data-dir /srv/agentscm/state pipeline pickup
agentscm --data-dir /srv/agentscm/state skill-workflow status pipeline-EXACT_RUN_ID
```

Recovery is explicit, requires the runner lease and keeps the same run. An action
that may already have run remains uncertain in its attempt journal; recovery does
not retry it. Only a succeeded run marks the outbox `complete`. Failed, blocked,
cancelled and incomplete outcomes remain `blocked` with their exact run-state
reason. After fixing a preflight problem that reserved no action attempt, recovery
can continue the same pending step and run ID. Interruption after claiming work
but before any action reservation also leaves a blocked item that needs this
explicit recover command; a later pickup does not requeue it automatically.
A failed or cancelled reserved
attempt stays historical; requeueing never reruns that effect. Inspect the run's
separate artifact fields. Completion is not a claim of independent verification,
publication or approval. There is no abandon/close or retarget/adopt operation for
a frozen dispatch; if its exact pinned installation is gone, it remains visibly
blocked. Even touching or re-signing a controller can change its pinned identity.
No profile/provider fallback, automatic publication, hosted dispatch or forge
merge occurs.

The local integration suite uses in-process fake GitHub/GitLab and real local Git.
It exercises source and event inputs, offline restart, duplicate delivery, CAS,
stale claims, disabled rules, revoked profiles, missing objects, resource drift
and interrupted-run adoption. Live credentials, real provider observation and an
operator's background service remain separate qualification.

## Repository binding, controller identity and offline recovery

Use a dedicated native data store for this forge repository; do not share it
across repositories or map it to an unrelated Hub repository. Each native data
root permits one stable forge identity: provider, host and numeric
repository ID. Installation reserves that binding before importing any source;
installing, observing or compiling another repository is rejected. A repository
rename can update its path under the same stable identity and rule ID, with the
usual expected generation. It does not create a second repository or replay an
old event. Use a separate native data root for a different repository, and keep
any Hub repository mapping consistent with that root. A failed first installation after binding
leaves the empty root reserved to its original forge identity; it is never
silently rebound. Retry the installation for that identity or prepare a new,
separate operator data root.

Installation resolves the Git controller to a canonical regular executable,
requires operator/root control of it and its parent directories, excludes the
actual checkout, Git/common directory and native state, and records file metadata
and a bounded content hash. Those facts are checked before controller probes or
execution. The action profile digest likewise binds its command and Claude
executable identities. Replacing a binary blocks the frozen delivery even if its
configured path remains the same. A reviewed replacement needs a rule update;
that update does not retarget already accepted work. These are trusted-local
integrity checks, not a hostile-host sandbox or cryptographic host attestation.

The journal at `pipeline-rules/state.sqlite` has its own application identity and
schema version. Offline `recovery backup --quiesced`, `recovery verify` and restore
include it as a typed SQLite database: exclusive checkpoint, WAL/SHM exclusion,
integrity and foreign-key checks, and bounded validation of the singleton native/
forge binding, rule generations, event identities, frozen dispatches and claims.
Verification repeats against the copied payload. Pending and claimed work remain
retained; activation does not restart a process or replay an uncertain action.
The native backup does not copy an external Git checkout, operator profile,
controller executable or subscription credentials. Restored work can remain
blocked until the operator supplies the same pinned files and locally available
Git objects; it never fetches, authenticates or runs automatically. Unsupported
schemas, untagged earlier development journals, damaged tables and inconsistent
payloads fail closed. Opening a schema 1 journal migrates it transactionally to
schema 2, which adds [operating-policy](operating-policy.md) contracts, decisions
and reservations. Backup, verification and restore accept both exact schemas and
preserve the saved version; opening a restored schema 1 journal then migrates it.
Older binaries reject schema 2, and no downgrade is provided. Take a consistent
backup before upgrading valuable state. This remains an experimental journal;
the migration and restore checks are local qualification, not a deployed-service
upgrade guarantee.
