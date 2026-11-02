---
title: "Continue Work with another local agent"
---

# Continue Work with another local agent

This experimental Unix command continues a Hub Work enrolled in the managed adapter using
installed Claude Code or Codex CLI. It preserves the Work identity and records the destination's actual native session.
The destination proposes one bounded replacement of an approved source file, an optional text
artifact and the next action. AgentSCM checks current Work authority and source before applying
the replacement, optionally runs an operator-selected check, and retains a handover checkpoint.
The next provider receives the changed source and retained context without copying a prompt.

This is trusted local execution. Enforcement covers this adapter's managed source replacements,
artifacts and checks. It does not revoke unrestricted filesystem tools in another process, sandbox
an operator-selected test command, grant forge merge authority, or attest to a hostile host.

## Preview and run

Connect the checkout to its intended Hub using the existing client setup. Enroll a new task
before its first native execution. Enrollment creates a Work through the existing Hub API and
records that this adapter has not launched an executor yet. It does not certify that an arbitrary
outside Claude/Codex session has stopped, and such sessions cannot be imported by this version.
After the first native run, all further continuations keep that same Work identity.

Keep the data directory outside the source checkout. Native clients use the existing subscription
login; there is no automatic provider or billing fallback.

```sh
agentscm --data-dir /private/agent-state continuation enroll \
  --root /source/project --request-key example-task \
  --title 'Implement the accepted example change' --path src/example.rs \
  --next-action 'Implement the first accepted criterion and record the next step'
```

The exact enrollment request is idempotent. Use the returned Work ID when previewing its first
native run and every subsequent provider transfer.

```sh
agentscm --data-dir /private/agent-state continuation preview WORK_ID \
  --root /source/project --to codex --executable /installed/codex \
  --path src/example.rs --context /private/task-context.json \
  --check /private/check.json
```

`task-context.json` is optional strict JSON with `criteria` and `constraints` string arrays,
`policy` text and an `open_decisions` string array. Supply accepted requirements, not a transcript.
The adapter also loads available local intent records, the Hub's bounded change record and its
own previous continuation outcomes. A preview includes the latest 16 effect summaries and the
latest verified artifact body, with total and omitted-history counts. The complete journal is
retained; only disclosure is bounded. A destination can use the included artifact decision
without additional tools. Missing criteria and evidence remain unknown; reported
checks retain their assurance and applicability. Unrelated private transcript bodies are never
retrieved. Selected source files must be existing regular UTF-8 files, within Work scope, at most
64 KiB each, with no symlink path components or Git/tool configuration paths. At most eight files
and 256 KiB of total native prompt data are accepted. Effect-summary disclosure is additionally
limited to 32 KiB, with compact references and explicit omission counts; complete outcomes remain
available through `history` and `show`. The Git index digest and selected file
permissions are also bound, including when existing source capture reports an index omission.

The optional check is the existing `ConfiguredRecipeV1`: `id`, absolute executable and fixed
arguments in `argv`, declared `coverage`, `timeout_seconds` and `max_output_bytes`. It is chosen
by the operator; a model response cannot introduce a command. Example:

```json
{"id":"unit-check","argv":["/usr/bin/python3","-m","unittest"],"coverage":"partial","timeout_seconds":60,"max_output_bytes":16384}
```

Inspect the preview's source bodies, context, destination, executable identity and check. To
authorize exactly that disclosure and managed work, repeat its full transfer ID:

```sh
agentscm --data-dir /private/agent-state continuation run TRANSFER_ID --confirm TRANSFER_ID
agentscm --data-dir /private/agent-state continuation show TRANSFER_ID
agentscm --data-dir /private/agent-state continuation artifact TRANSFER_ID
```

The command records a handover checkpoint and reserves before launching the native client.
The process/group identity is persisted before submitting the prompt. Source and authority are
rechecked after its explicit acceptance. A native exit of zero is not
completion: completion requires validated output, managed source/artifact progress and a Hub
checkpoint. An optional check can fail while the continuation completes: its actual failure and
required follow-up are recorded in the checkpoint; it is never described as passing or retried
automatically.
An installed client that lacks the required CLI options fails rather than switching tools.
No model is selected by this adapter. Native invocations run in a temporary directory, receive
the previewed bounded packet from this adapter, and do not persist whole conversations through
this adapter. The installed client still supplies its own runtime instructions and metadata; its
feature flags are not proof of an empty tool registry. Operational tool output is rejected before
any proposal is accepted or applied.

Use `continuation history WORK_ID` and its `--before CURSOR` pages to discover earlier transfer
IDs, then `show` or `artifact` to inspect them. Preview's repeatable `--artifact TRANSFER_ID` selects
up to four earlier artifacts from the same Work instead of the latest one; their combined bodies
must fit within 64 KiB. Omitted outcomes are not evidence that they did not occur. Authority,
latest-generation and unresolved-execution checks always inspect the complete journal, so old
uncertain work cannot disappear behind a display limit. There is no 64-transfer lifetime limit.

To continue in the other direction, preview the same Work with `--to claude` and its installed
executable. Omitted source paths, context and check inherit the last completed transfer. The
current Work next action and source bytes are captured again. Review and confirm the new ID.

## Recovery and revocation

```sh
agentscm --data-dir /private/agent-state continuation recover TRANSFER_ID
agentscm --data-dir /private/agent-state continuation revoke TRANSFER_ID
agentscm --data-dir /private/agent-state continuation resolve TRANSFER_ID
```

Received output and acceptance survive a restart. Failed native calls retain bounded exit,
termination, capture and validated session diagnostics, without raw stderr or failed result bodies.
Nonfatal Codex runtime-warning items retain only a count; a final response and successful turn
are still required, and operational tool events and top-level errors remain rejected. Recovery reuses the actual native session and
the existing generation-checked Work takeover. A source replacement whose response was lost is
reconciled by its recorded before/after bytes; recovery never blindly repeats it. Divergent user
edits remain untouched. Artifacts use the existing content-addressed spool. Configured checks use
the existing durable action receipts and supervised execution through an operator recipe entry
point. That entry point does not synthesize a native tool hook. An unfinished check can be
reconciled only after its recorded process/group is observed stopped; recovery never launches a
check to determine whether an earlier check succeeded. Check admission persists the exact recipe
operation identity, including the repository root's device and inode, before launch. A changed
identity or a reservation missing that identity remains unresolved; a different empty receipt
lookup cannot prove the admitted check never started.

A reserved native launch with no retained result is uncertain. Heartbeat expiry does not establish
that the process stopped. `resolve` observes the recorded PID and process group being absent, or
a changed boot identity on the same host. A crash between spawning and persisting the PID marker
requires the latter observation; an operator assertion cannot replace it. This version never
relaunches a reserved native call automatically. Declined or malformed completed responses can
also be resolved without granting the destination a Work generation.

Resolution preserves the response, source effects and receipts, leaves unknown check outcomes
visible, and releases only an observed safe boundary for a fresh explicit preview. Divergent source
effects and unobservable processes remain blocked. A missing or expired Hub receipt does not prove
whether its checkpoint committed. `recover` retains the original request; an explicit `resolve`
can record a new administrative handover against the observed current Work version after proving
managed execution stopped. It never reruns native, source or check effects. Revoked previews can be resolved without
running them. Revocation preserves prior effects; it does not undo writes or terminate unrelated
agents. A check whose dispatch was admitted before revocation may finish, while subsequent managed
effects/checkpoints are refused. Local source/artifact admission is serialized with revocation.
Remote authority is rechecked at each boundary; this is not an atomic transaction spanning the Hub
and arbitrary external processes. Changed Work generation, token, repository connection, source
or scope requires inspection and fresh authorization for affected work.

Ignored source files and submodule contents retain the existing capture omissions. Checks are
trusted local observations, not independent assurance or human approval. Synthetic recovery tests
and real installed-client qualification must be reported separately; source presence alone does
not establish a qualified release or hosted support.

## Qualification

The ignored `continuation::live_installed_claude_codex_claude_continue_the_same_work` integration
test starts an isolated local Hub and synthetic repository, enrolls one task, then runs installed
Claude, Codex and Claude again. It requires source changes driven by the preceding provider's
retained artifact decision, unique native session identities, the same Work, and durable progress.
Select it explicitly with `AGENTSCM_LIVE_CLAUDE` and `AGENTSCM_LIVE_CODEX` set to installed absolute
executables. It preserves existing native authentication and makes no model/billing override.
Default tests use synthetic protocol clients and cannot substitute for that qualification.

The Codex adapter uses explicitly disabled runtime feature names inspected in
`codex-cli 0.165.0-alpha.1.2`, including apps, plugins, hooks, shell/execution, code mode,
browser/computer use, skill discovery and multi-agent surfaces. It requests a read-only sandbox
and runs outside the source checkout. These settings are client-version-specific; successful
qualification must be recorded for the installed version. They do not attest to arbitrary native
plugins or a hostile executable, and an unknown operational output item is not treated as success.
