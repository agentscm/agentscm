---
title: "Operator-local repository pipeline rules"
---

# Operator-local repository pipeline rules

This experimental Unix CLI installs repository merge rules and feeds the existing
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
An open or unmerged proposal starts nothing. GitHub requires its confirmed
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
payloads fail closed. No deployed-format migration is claimed for
this experimental journal.
