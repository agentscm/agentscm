---
title: "Executable operating policy"
---

# Executable operating policy

The experimental Unix `operating-policy` command turns a plain-language team rule
into an owner-reviewed contract over an existing [pipeline rule](pipeline-rules.md).
An installed Claude Code or Codex client drafts the contract. The owner inspects
its exact meaning, supplies a rehearsal corpus, observes a real merge and activates
that version. A later confirmed merge can acquire durable Hub Work and run the
existing [productive workflow executor](productive-workflow-actions.md).

This first event family produces retained local artifacts after a GitHub or GitLab
merge. It cannot update dependencies, create a proposal, publish an artifact or
merge source. It governs AgentSCM-managed execution on one Unix computer. Ordinary
editor hooks remain advisory, and operator commands retain the privileges of the
local account. The workflow is not a sandbox or a security boundary around raw
shell, Git, forge or external agent activity.

Opening the pipeline journal with this version migrates its schema from 1 to 2
transactionally, including when using existing pipeline commands. Schema 2 retains
operating contracts, decisions and reservations alongside existing pipeline state.
Older binaries reject it. Take a consistent native-state backup before upgrading
valuable data; see [pipeline backup and recovery](pipeline-rules.md). Backup and
restore preserve the saved schema, and opening a restored schema 1 journal performs
the migration. There is no schema downgrade command.

## Prepare and inspect

Install a pipeline rule with an operator-owned profile outside the source checkout,
using the pipeline guide. The source checkout must contain the accepted Git objects;
the runner never fetches, switches branches or modifies dirty source. Use a separate
initialized AgentSCM data directory. Connect the checkout to a Hub with a current
write-capable agent token bound to the same provider, host, repository and stable
repository ID. The authenticated token owner is the **observer and launcher actor**;
`eligible_actors` does not authorize forge merge authors or map bots to owners.

Before drafting or reviewing, inspect the installed recipe and profile:

```sh
agentscm --data-dir "$policy_data" operating-policy inspect release-notes
```

The result includes the repository scope, target and labels, selected skill
resources, recipe graph, exact profile configuration, identities, evidence
requirements and escalation behavior. Review these actual actions and executable
paths. A digest identifies those bytes; it does not establish their safety or prove
that an action ran.

## Draft and review the contract

Write `intent.txt`, for example:

> After a release-labelled merge to main, run the installed local release-notes
> workflow once and retain its output for owner review. Do not publish or alter source.

Choose an existing installed client and an absolute executable path explicitly:

```sh
agentscm --data-dir "$policy_data" operating-policy draft release-notes \
  --intent intent.txt --client claude --executable "$native_client" \
  --request-id release-notes-draft-1 > draft.json
jq '.contract' draft.json > contract.json
```

`--client codex` uses the corresponding installed client. The adapter reserves the
invocation before launch and retains actual native-session evidence. Definite
authentication, request-bound and executable preflight errors are rejected before
reserving a request ID. It uses a bounded native request with tool-disable options;
operational tool events are rejected. These options do not prove an empty tool
registry: clients can retain skill metadata and code-mode context. The model cannot
choose executable paths, profiles or authority. A lost response leaves an uncertain reservation and is not
automatically retried. Inspect that outcome before explicitly authorizing a new
request ID. The same completed request returns its retained result offline, without
the original executable. Its request identity includes the exact explicit inputs;
changed inputs cannot reuse that ID. Retrieving an old draft does not authorize it:
review still requires current Hub authority and matching installed bindings. The
journal retains at most 1,000 draft requests, including uncertain attempts. Reaching
that cap refuses new drafts while preserving existing evidence; this release has no
purge command.

Inspect and edit `contract.json`. The contract retains the original intent,
unsupported or ambiguous clauses in `unresolved`, eligible actors, exact installed
rule identity and these supported declarations:

- `action: "productive_artifacts"` limits the job to the installed workflow.
- `required_evidence: "current_profile_and_workflow_dependencies"` requires the
  current pinned profile within reviewed resource ceilings. Missing profile evidence
  is unknown; changed or invalid profile evidence is denied. The existing workflow
  runner separately requires successful declared graph dependencies before each
  dependent action and checks the pinned source resources.
- `escalation: "retain_for_owner"` retains a decision or Work blocker for explicit
  owner intervention. It does not send notifications or retry an uncertain effect.
- `max_dispatches` limits lifetime reservations for this immutable contract version.
  Failed, revoked and uncertain dispatches still consume reservations. This is a
  count, not a monetary spending limit.
- `max_action_runtime_ms` and `max_artifact_bytes` bound the installed profile's
  per-action time and output. A profile exceeding either ceiling cannot be reviewed.

The profile limits are per action; a graph can contain several actions. No token,
provider-cost or whole-workflow currency budget is claimed. Unsupported clauses
must be resolved explicitly; unresolved contracts cannot activate. Unknown JSON
fields are rejected.

```sh
agentscm --data-dir "$policy_data" operating-policy review contract.json
```

Review creates immutable versioned bytes in observation mode. It does not activate
execution. Before first attaching a policy, complete all pending, claimed or blocked
legacy dispatches for that rule, or install a separate rule. The result again includes the resolved installation. An amendment needs
`--expected-generation N` and returns to observation mode with a fresh budget and
rehearsal requirement. The same owner must review it. A policy-owned installed
recipe/profile cannot be changed through legacy pipeline update or enable commands;
this release supports amendments to the operating contract, not replacement of its
installed workflow. Install and review a separate rule for a different workflow.

## Rehearse and observe

Write `scenarios.json` independently of the drafting agent. It is an array of
`{name, event, context, expected}` objects. Events have the `ConfirmedMerge` shape
listed in the pipeline guide. Each context contains `actor`, nullable Boolean
`authority_current`, `bindings_current`, `prerequisites_passed`, integer
`remaining_dispatches` and Boolean `duplicate`. Expected decisions are `run`,
`ignore`, `deny` or `unknown`.

Use distinct cases covering a matching event, unlisted actor, revoked authority,
changed binding, failed prerequisite, duplicate event, exhausted budget and unknown
prerequisite. Include unrelated targets/labels when relevant. A null prerequisite
represents unavailable evidence. The eight required coverage cases are a minimum;
the software cannot establish whether the author is independent or whether the
corpus captures the team's full intent.

```sh
agentscm --data-dir "$policy_data" operating-policy rehearse release-notes scenarios.json
agentscm --data-dir "$policy_data" operating-policy observe release-notes \
  --connection forge-connection.json --proposal 41
```

Rehearsal and live admission use the same evaluator. The retained report includes
expected decisions, actual traces, exact contract identity and coverage. Replay
uses synthetic fact inputs and performs no operational action. A passing report
is not human approval or live runtime qualification. Live admission derives the
profile evidence from the installed file; it never accepts rehearsal facts from
candidate source. Workflow source-check failures are enforced by the existing
runner and block dependent actions.

The forge connection uses the existing strict HTTPS pipeline configuration and a
named credential environment variable. Observation re-reads the provider's actual
merged proposal and stable repository identity. Labels reflect observation time.
Activation requires a successful matching observation under the exact reviewed
version, as well as current profile evidence. An ignored, denied or unknown
observation does not qualify. In observation mode, no dispatch or Work is created.
Retrying the same event does not append decision rows. Legacy pipeline observation
and pickup skip policy-owned rules, leaving them available to operating-policy
commands while unrelated legacy rules continue. When all installed rules for a
repository are policy-owned, legacy polling skips provider requests. Every event already observed
for a stable rule remains deduplicated after activation, including an event first
seen while paused, mismatched or denied. Choose a different confirmed merge to
exercise the activated rule.

## Activate and execute

Use the reviewed contract digest, a passing rehearsal digest for that exact version
and its current generation from the returned status:

```sh
agentscm --data-dir "$policy_data" operating-policy activate release-notes \
  --digest "$contract_digest" --rehearsal "$rehearsal_digest" \
  --expected-generation "$policy_generation"
agentscm --data-dir "$policy_data" operating-policy observe release-notes \
  --connection forge-connection.json --proposal 42 --execute
```

`observe --execute` is a bounded foreground event-to-work command. For a newly
accepted event, it reserves the budget and outbox entry atomically, acquires the
same deterministic Hub Work identity, runs the pinned workflow and records a Finish
or Blocker checkpoint. Re-delivering the same event creates no second Work or
runtime attempt. Without `--execute`, accepted work remains pending for an explicit
`operating-policy run DISPATCH_ID`. There is no background daemon in this release.

Current policy, reservation, profile, Hub connection, token, Work generation,
sequence, lead session and scope are checked before managed effects and during
execution. The runner uses the accepted immutable Git commit, preserving dirty
checkout edits. Native action output is a reported artifact; it is not independent
verification. Inspect the useful output and retained checks before relying on it.

```sh
agentscm --data-dir "$policy_data" operating-policy status release-notes
agentscm --data-dir "$policy_data" skill-workflow artifact RUN_ID STEP_ID ARTIFACT_NAME
```

Use `skill-workflow --help` for the installed build's artifact output options.
Status retains decisions and outbox outcomes even if a profile has disappeared.
The underlying workflow journal retains step attempts and artifacts.

## Pause, revoke and recover

```sh
agentscm --data-dir "$policy_data" operating-policy pause release-notes \
  --digest "$contract_digest" --expected-generation "$policy_generation"
agentscm --data-dir "$policy_data" operating-policy revoke release-notes \
  --digest "$contract_digest" --expected-generation "$policy_generation"
```

The owner of the private local journal can pause or revoke offline, including
during a Hub outage or token expiry. This only narrows authority; activation still
requires current Hub authentication. Revocation cannot be changed to pause and
reactivated: a new reviewed version is required. A current pause or revocation
refuses pending Work creation and the next disallowed
managed action. Running subprocesses are cancelled through the existing executor's
boundary polling; an effect already completed is not undone. Revocation preserves
history and requires a newly reviewed version to reactivate. A stale digest,
generation or owner cannot change the lifecycle.

After inspecting an interrupted or blocked dispatch, an owner can explicitly resume
its retained Work and workflow:

```sh
agentscm --data-dir "$policy_data" operating-policy recover DISPATCH_ID \
  --expected-generation CLAIM_GENERATION
agentscm --data-dir "$policy_data" operating-policy run DISPATCH_ID
```

Recovery retains its exact checkpoint request before sending it, so a lost reply
replays the same operation instead of appending another checkpoint. A dedicated Hub
sequence rejection permits a refreshed request only on a later explicit recovery
attempt, after the owner inspects current Work. That rejection is emitted only when
retained history proves the operation never committed. Generic errors, transport
failures and history gaps after receipt compaction preserve the uncertain request;
they never authorize a fresh automatic attempt.

Try ordinary `recover` first so a retained receipt can resolve the outcome. If
compaction leaves that request ambiguous, or a later owner checkpoint changes
its resulting state, inspect the exact retained operation in `operating-policy status`
and the current Hub Work. Explicit reconciliation names both observations:

```sh
agentscm --data-dir "$policy_data" operating-policy recover DISPATCH_ID \
  --expected-generation CLAIM_GENERATION \
  --reconcile-operation RETAINED_OPERATION_ID --observed-seq CURRENT_WORK_SEQUENCE
```

Both flags are required together. The sequence must still match current Work and
be newer than the old request's expected sequence. Reconciliation requires the same
live owner, generation, token, session and scope, plus a terminal retained workflow
appropriate to the dispatch: successful for a completion, or failed, blocked or
cancelled for resumption. A non-complete dispatch can also reconcile when its
workflow was never created: the whole runtime directory must be absent, or a
validated runtime must report that exact run missing. Existing incomplete journals,
read errors and pending or running workflows are refused. It retains the exact old request with outcome `unknown`,
then records one new administrative Progress or Finish checkpoint. A new Finish
retains the accepted source commit as evidence. It does not
classify the old request as rejected or prove that it never committed. The history
is append-only and bounded to 16 reconciliations per Work binding within its 64 KiB
limit; exhaustion preserves evidence and refuses further reconciliation. A repeated
ordinary recovery replays the newly retained request. No reconciliation retries
uncertain runtime actions.

A legacy refusal that predates Work binding can be requeued only when no workflow run exists.
Recovery requires the same live Hub identity, Work generation/session/scope and
active policy. It never creates replacement Work. A takeover or narrowed authority
blocks it. Existing uncertain action attempts remain non-replayable; recovering the
outbox does not make them safe to repeat. If execution completed but its Work
checkpoint response was lost, recovery observes the retained workflow and records
the matching completion without rerunning the action. A newer owner must inspect
and reconcile changed responsibility rather than silently adopt old authority.

## Qualification

Ordinary integration tests use synthetic forge facts, the real loopback Hub and
real local command execution. They exercise exact activation, failed/unknown live
profile evidence, failed graph dependencies, revoked pending work, cancellation,
recovery identity, retained-history corruption and deduplication.

Two ignored tests require an explicitly selected installed Claude Code executable
with existing access and an explicitly authorized model selector. They do not
fall back to another model, add billing or mutate a forge:

```sh
AGENTSCM_TEST_NATIVE_CLAUDE="$native_client" \
AGENTSCM_TEST_NATIVE_CLAUDE_MODEL="$native_model" cargo nextest run --locked --run-ignored only \
  -E 'test(operating_policy::live_native_drafting_and_confirmed_event_run_existing_productive_executor)'
```

This first test invokes two real native drafts (supported intent and unsupported
publishing), confirms events through a local forge HTTP fixture, and invokes the
existing prompt ActionExecutor to produce a useful artifact through authenticated
Work. The forge facts are synthetic; it is not external-forge qualification.

For the exact production HTTPS `observe --execute` path, supply a private local
qualification configuration:

```json
{
  "source_checkout": "/path/to/existing/source",
  "connection": "/path/to/forge-connection.json",
  "observe_proposal": 41,
  "execute_proposal": 42
}
```

Both proposals must already be merged in the configured GitHub repository. The
execute proposal's accepted commit must already be available locally and include
`examples/productive-workflows/release-notes/v1/{SKILL.md,explain.json,explain.md}`
and `examples/productive-workflows/synthetic-change.txt`. The test reads these
existing example resources at that exact commit, uses a fresh local Hub and data
directory, drafts/reviews/activates the contract and checks the filter values, default,
preserved search query and empty-result behavior in the output, with one Work after
duplicate delivery. Its real merge event still processes a synthetic
example source file; it does not establish production release-note quality.

```sh
AGENTSCM_TEST_NATIVE_CLAUDE="$native_client" \
AGENTSCM_TEST_NATIVE_CLAUDE_MODEL="$native_model" \
AGENTSCM_TEST_OPERATING_LIVE_CONFIG="$qualification_config" \
cargo nextest run --locked --run-ignored only \
  -E 'test(operating_policy::live_https_observe_execute_runs_once_through_the_production_cli)'
```

The connection's named credential variable must already be set. Source and forge
are read-only in this test. Qualification counts as passed only when the actual
installed-client test completes successfully; ordinary safety tests or generated
JSON cannot substitute for it.
