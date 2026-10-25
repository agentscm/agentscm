# Local change capture

AgentSCM can retain a bounded local account of an agent session and bind an observed command to
the source, recipe, and setup that existed before it ran. This is an alpha capture path. It does
not grant approval, claim that an agent is trustworthy, or turn a local developer checkout into a
protected execution environment.

## Privacy boundary

The local spool is an absolute directory chosen by the operator. AgentSCM creates its directories
with mode `0700` and files with mode `0600`. Conversation text, tool input, command arguments,
stdout, stderr, source manifests, and captured source bytes remain there. A shareable envelope can
contain only typed event facts and opaque artifact identities: digest, byte count, media type,
retention class, and availability. Conversation bodies leave the machine only for a checkout that
opted into `shared-conversation` mode, as described in
[Sharing conversations with the Hub](#sharing-conversations-with-the-hub); the Hub refuses a
shared-approved body declared by a session that was not in that mode.

Metadata-only capture does not retain conversation bodies. Local-conversation capture retains the
selected visible prompt, assistant-message, tool-input, and tool-result blocks in the private
spool. Claude thinking and redacted-thinking blocks are never selected. Passive hook capture does
not call a model or send content to a provider.

Capture has three modes per checkout, and `metadata-only` is the default:

| Mode | Bodies kept locally | Bodies sent to the Hub |
|---|---|---|
| `metadata-only` | No | No |
| `local-conversation` | Yes, unredacted | No |
| `shared-conversation` | Yes, redacted | Yes, after the Hub accepts the event |

The spool has finite operator-configured record, total-byte, and per-artifact limits and never
silently evicts retained evidence. Defaults are 4,096 records, 64 MiB total, and 16 MiB per
artifact. Configuration refuses an action-capable spool below 48 records or 64 KiB total because
that space cannot retain the declared minimal managed lifecycle and its recovery metadata. At the
48-record floor, 12 records remain reserved as health headroom and 36 are available for the fixed
metadata plus configure, session start, managed Pre, child start and finish, and managed Post
records. This admission floor does not promise that every maximum-size source, output, or artifact
shape fits; larger captures still stop honestly at their configured record or byte quota. Limits
include the permanent spool and managed-capture lock inodes, observation scope, order and
compaction indexes, and the managed-format initialization commit. Initialization admits the
entire fixed layout before writing it and writes the format commit last. A crash before that commit
leaves a nonempty spool that is refused without repair or migration. Metadata
headroom is reserved for finishing started actions and recording an explicit
`capture_interrupted` gap when quota pressure pauses ordinary capture. A later retained event at a
higher session sequence shows that capture resumed; the gap remains and coverage does not return to
complete. If content cannot be retained, the receipt records `missing` availability and a gap
instead of losing the completion.

Acknowledgement hides an envelope from delivery without immediately deleting it. Explicit local
compaction can replace acknowledged ordinary event envelopes with exact idempotency digests and
completed action pairs with compact retained receipts. Usage records and provider-history gap
envelopes retain their full original contents in the replay index. Managed recipe Pre/Post events retain their
full original Work, generation, and session envelope alongside the action receipts so target or
timestamp changes cannot turn a provider-occurrence replay into new work. This finite replay index
is never silently pruned: when its configured record or byte bound is reached, compaction refuses
and requires explicit export or a larger spool. Compaction never removes pending envelopes,
incomplete action starts, artifacts, deferred diagnostics, or the source metadata needed by an
outstanding action.
Local artifacts are not automatically pruned because a shared `local` availability reference would
otherwise become false. Recoverable fully written temporary files are completed under the spool
lock in every private spool directory after a crash. Exact duplicates are removed and malformed or
ambiguous files are moved into a private quarantine. Quarantine files count toward the configured
record and byte limits and are never deleted automatically. Inspect them with `change-capture
quarantine`; `change-capture prune-quarantine` deletes only exact opaque names selected by the
operator. A committed spool may reopen above its configured write cap after crash debris is
quarantined so bounded inspection and explicit pruning remain available. Every ordinary admission
still counts the complete physical usage and refuses until capacity is restored; recovery itself
has a fixed physical-record scan bound. Active writer temporary files are protected by the spool
lock and are never quarantine
prune targets. AgentSCM never expires or deletes anything in the local spool automatically. Only
the Hub copies of shared bodies expire (see [retention and deletion](#retention-and-deletion)).

The compaction index is the durable commit point. Crash recovery completes its exact envelope,
acknowledgement, native-observation, active-order, and delivery-cursor cleanup. A cursor naming
compacted history cannot wedge later delivery, including when a repeated compaction has no new
selection. Completed compacted action starts retain their exact observed order for idempotent
replay while their active order-ledger entries are reclaimed. If the active order ledger reaches
its finite bound before compaction, the native hook retains the ordinary lifecycle fact, records
one priority `capture_interrupted` pause marker, and remains fail-open.

## Sharing conversations with the Hub

This is opt-in per checkout and alpha. A checkout in `shared-conversation` mode retains the same
selected bodies as `local-conversation` (visible prompts, assistant messages, tool input and tool
results; never thinking blocks), but scrubs each one for secrets before it is written to the spool
and then uploads it to the Hub's object store. Nothing is shared unless you choose the mode, and
the choice applies to one checkout, not to a Hub, repository or user.

Choose the mode when configuring (`change-capture configure --capture-mode shared-conversation`) or
change it later on an enabled binding:

```text
agentscm change-capture set-mode --root /absolute/repository --mode shared-conversation
agentscm change-capture set-mode --root /absolute/repository --mode metadata-only
```

`set-mode` changes only the stored mode. It requires an enabled capture binding for the checkout
(run `configure` first), leaves hooks and the spool untouched, and never deletes local or Hub data.
`change-capture status` reports the current `capture_mode`. The macOS menu-bar companion has the
same switch per checkout, **Share agent conversations with Hub**; it is unchecked by default, is
disabled until the checkout has an enabled capture binding, and shows the result in its status
window. Turning it off only switches the mode. The capture config is written as version 6 only while
some checkout is in `shared-conversation` mode; an older binary refuses a version 6 file, and the
file returns to version 5 when no checkout shares.

What leaves the machine, and when:

- **Redaction is client-side and best effort.** Before a body is retained, the client replaces
  well-known credential shapes with `[REDACTED:<kind>]`: `aws_access_key_id`, `github_token`,
  `gitlab_token`, `slack_token`, `anthropic_key`, `openai_key`, `stripe_key`, `google_api_key`,
  `npm_token`, `pypi_token`, `huggingface_token`, `jwt`, `bearer`, `private_key` (PEM blocks),
  `url_password` (the password in `scheme://user:password@host`), `flag_value` (the value after a
  secret-named `--flag`, the password of `curl -u user:pass`, and `-pVALUE` in a `mysql`,
  `mysqldump`, `mysqladmin` or `mariadb` command), `assignment` (the value in a `NAME=value`,
  `NAME: value` or `NAME => value` assignment whose name looks secret) and `field` (the value of a
  JSON field whose name ends in a secret word such as `password`, `token`, `secret`, `apikey`,
  `secretkey`, `signingkey`, `dsn`, `databaseurl`, `authorization` or `cookie`; when that value is
  an object or array the whole subtree is replaced). Matching on the name
  suffix keeps counters such as `max_tokens` and `input_tokens` readable. A body that needed
  redaction carries a `redactions` count and the event stream gains a separate `redacted` gap, so
  the change record shows that a body was altered. Redaction is heuristic: it cannot recognise a
  secret with no distinctive shape, and it does not remove personal data, proprietary code or
  anything else a person would not want shared. The Hub does not re-check or verify redaction.
  `--password-file PATH` is not redacted, and neither is `mysql -p VALUE` (the value is a database
  name there). Treat `shared-conversation` as sharing the conversation with people who can read it, not as a
  guarantee that it is clean.
- **Upgrade the Hub before clients.** The new event fields and kinds are rejected by an older Hub, so
  envelopes from an upgraded client stay pending until the Hub is upgraded; metadata for a Work that
  ends first is lost. Before each upload the client asks the Hub which bodies it still lacks and sends
  only those.
- **Bodies over 1 MiB stay local.** A body above the per-body cap is kept in the spool as
  work-local and is never uploaded.
- **Mid-session switches stay local.** A session keeps the mode it started under for sharing:
  switching a checkout to `shared-conversation` while a Claude session is running keeps that
  session's bodies local, and sharing starts with the next session. Switching away stops uploads
  at the next delivery.
- **Upload follows event acceptance.** The hook appends locally first and delivers the event
  batch as before; only after the Hub accepts it (HTTP 200) are the batch's shared bodies
  uploaded. A transient failure leaves the envelope pending for the next hook, and a body that
  never uploads stays declared on the Hub as `local`. Hooks stay fail-open.

Who can read or delete a shared body: the human who shared it (any of that person's tokens for the
repository; read-only tokens may read but not delete) and Hub admins. The owner of the Work has no
access to a helper's or reviewer's shared bodies unless they shared them. Each human's declaration of
a body is its own row, so two people sharing identical content each read and delete their own. A
listing shows a caller only their own entries (an admin sees all); a body request from anyone else
gets 403, whether or not that digest exists. There is no per-body grant, and bodies are never included
in the change-record projection or handoff.
Read them with `agentscm change-capture conversation`:

```text
agentscm change-capture conversation --work-id wrk_example              # list entries
agentscm change-capture conversation --work-id wrk_example --digest SHA256 # print one body
agentscm change-capture conversation --work-id wrk_example --delete SHA256 # delete one body now
```

A printed body is untrusted agent output: read it, never run it. The routes, response limits and
status codes are in [shared conversation reads](hub.md#shared-conversation-reads).

### Retention and deletion

The Hub keeps an uploaded body for 30 days by default; the operator changes this with
`hub serve --shared-conversation-retention-days N` (1 to 3650). The deadline is set when the body
is uploaded. A sweep every ten minutes expires due bodies and deletes their objects, in batches of
100 until none are due or 30 seconds are spent (the rest waits for the next pass); an object whose
removal fails is retried after 1 hour, doubling to at most a day, without holding up others. The author
or a Hub admin can delete a body earlier with `--delete` or the HTTP `DELETE` route (an admin deletes
every author's copy); deletion records a tombstone and then removes the object, so a deleted or
expired body answers 410. When two people shared identical content the stored object is removed
only once no other copy of it is declared or available, so an author who declared but never uploaded
keeps it from being removed.

Turning sharing off does not recall anything: copies already uploaded stay until they expire or
are deleted. AgentSCM never deletes the local spool copy. Backups, exports or copies a reader
made before expiry are outside the Hub's control.

## Claude Code adapter

The native adapter accepts Claude Code command-hook JSON on stdin. Its supported input events are:

- `SessionStart`: observed startup, resume, clear, compact, or fork boundary. An allowlisted
  `source` (startup, resume, clear, compact) is kept as the event boundary; a compact restart
  continues the same session and is not counted as a second original start.
- `UserPromptSubmit`: visible prompt, if local-conversation capture is enabled.
- `PreToolUse`: typed tool start and optional local tool input.
- `PostToolUse` and `PostToolUseFailure`: typed tool finish and optional local result or error.
- `MessageDisplay`: assistant-message fragments. When Claude Code supplies a message identity,
  fragment index, final flag, or timestamp, the adapter uses those correlation fields.
- `Stop`, `PreCompact`, `SubagentStart` and `SubagentStop`: turn-ended, compaction and subagent
  lifecycle markers. A subagent marker carries the subagent's `agent_id` and optional
  `agent_type`; one without a valid `agent_id` is ignored. These markers are informational: they
  never raise coverage, and they do not record an `identity_uncertain` gap when the host supplies no
  identity.
- `SessionEnd`: session termination. An allowlisted `reason` (clear, logout, prompt_input_exit) is
  kept as the event boundary and any other reason is recorded as `other`.

Tool facts also carry the bounded host tool name (for example `mcp__server__tool`) and, on finish
events, whether the host reported the call as `returned` or `failed`. `returned` means the host
hook fired for a finished call; it is not evidence that the call achieved anything.

`Stop` is a turn boundary and is not recorded as session termination. Only a `SessionStart` whose
documented source is `startup` establishes an original session boundary. A resume, clear, compact,
or fork does not prove earlier history was observed; without a previously retained original start,
the adapter records one `adapter_started_late` gap. It records the same gap when the first observed
native event is not `SessionStart`. An ended session is complete only within the adapter's supported
visible scope when its original start and end were both observed and it has no gaps.

Events without a provider identity or timestamp use a durable local occurrence number. The adapter
records an `identity_uncertain` gap on each such invocation because the native contract cannot
distinguish a retried hook from a legitimate repeated event. This preserves repeated identical
prompts but may overcount retries; it does not claim exact replay identity. Missing provider times
remain unavailable as `0`; local deferred diagnostics use a separate durable receipt order instead
of presenting them as Unix-epoch events. Raw prompt and result bodies stay local and do not
contribute to shared event identity.
The native session id observed in a hook is validated separately from the Hub's canonical aliased
session id. Event and action envelopes plus shared sequence state use the canonical id. For a
provider-identified tool occurrence, the first durable claim records the observed identity, route,
Work, generation, canonical session, rule, and source binding before any target-scoped mutation.
Later retries use that original claim even if current target resolution has changed. This lets a Hub
alias such as `default` accept hooks from the actual native session without weakening native payload
validation or reattributing an earlier occurrence.
The adapter ignores unrecognized events. Hook failures print a local warning and return success so
capture availability cannot block the host session.

Configure a checkout once after its existing AgentSCM client connection is current:

```text
agentscm change-capture configure \
  --root /absolute/repository \
  --spool /absolute/private/spool \
  --worktree-id checkout_example \
  --base-ref origin/main \
  --setup-digest 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --claude-history-root /absolute/private/claude-history \
  --capture-mode metadata-only \
  --action-rule Bash=execution:partial:process_exit_code \
  --action-rule Write=execution:unknown:unverified \
  --recipe-file /absolute/private/test-recipe.json
```

`--claude-history-root` is optional. When configured, the Claude hook may read only the regular
JSONL file whose basename exactly matches the observed native session id and whose canonical path
remains beneath that non-symlink directory. A caller-supplied transcript path alone grants no read
authority. The reader follows a bounded per-session offset and file identity; replacement,
truncation, malformed complete records, foreign session records, or a changed Work association
produce an explicit capture gap. Replacement and truncation stop that cursor. A Work or generation
change rebinds at the opened snapshot's end of file: boundary records remain unattributed and later
appends resume under the new Work. An already-running session also starts at the current end of
file so old usage is not attributed to a newly current Work. Only structured provider usage and identity
metadata are shared. Transcript bodies remain local, and the reader makes no model request.

Request-scoped provider usage is submitted through the existing Work usage endpoint. A final
session-cumulative record is retained separately and excluded from summed insight totals, so it is
never added to the request records. Missing provider fields remain unknown. Native hook metadata
can also retain a small local setup artifact and share its opaque identity plus any observed model,
permission mode and effort, and an explicit list of unobserved setup components. No setup artifact
is created when the hook exposed none of those fields, and the configured caller setup digest is
not presented as a complete observed setup.

The private recipe file is a strict JSON object:

```json
{
  "id": "unit-test",
  "argv": ["/usr/bin/python3", "-m", "unittest", "-q"],
  "coverage": "complete",
  "timeout_seconds": 300,
  "max_output_bytes": 1048576
}
```

Configuration accepts at most 16 distinct recipe ids and digests. On `SessionStart`, the hook adds
bounded guidance with the exact absolute `agentscm change-capture run-recipe --id ...` command.
That managed command accepts no caller overrides for argv, source, Work, generation, or result. It
requires Claude's child-session marker and session identity, runs only from the exact configured
repository root, resolves the current authorized target before spawning, and binds a fresh durable
invocation to the configured recipe and source. The child environment is cleared, then only
`PATH`, `HOME`, `USER`, `TMPDIR`, `LANG`, `CARGO_HOME`, `RUSTUP_HOME`, and `CARGO_TARGET_DIR` are
copied when present. Standard output and error remain visible while bounded copies are retained.
The real child exit status produces the configured check result. The result is still
`caller_asserted`: the private local configuration is not a Hub-governed recipe registry or an
independent verifier.

The managed runner handles SIGINT and SIGTERM by cancelling and reaping its owned child process
group, then retaining an incomplete receipt. Its configured timeout and combined output bound use
the same supervision path. SIGKILL or a machine/process crash cannot run cleanup or append a
finish; a durable start remains evidence of an interrupted invocation and is never replayed as a
new command. That unresolved attempt fences the same Work, canonical checkout and recipe even if
the session, generation, source, setup, or recipe bytes later change. Explicit
`change-capture reconcile-recipe --id ...` rechecks current Work authorization. It can clear a
reservation only when no action start became durable, releasing its unused order. If a start is
durable, the child may have spawned before the attempt's spawned phase could be persisted, so the
same-boot attempt remains unresolved even when its retained phase still says `reserved`. After a
verified reboot of the same local host reconciliation can append an incomplete/unknown finish using
the original bindings. Foreign-host, missing, or ambiguous identity also remains unresolved. This
local execution path does not claim crash-proof containment. Clearing a proven no-start reservation
keeps the original managed occurrence claim until its exact Post arrives, so either race order
converges on one original-bound generic terminal receipt instead of leaving a permanent
interruption or launching the child.

A private checkout-wide lock serializes configured recipe callers for one canonical repository
root, repository, and worktree. It is broader than an attempt fence and proves only that another
local caller currently holds the lock. Work and recipe identities remain in the separate attempt
record. The attempt retains the checkout device and inode while unresolved; after a terminal
receipt clears the attempt, replacing the checkout inode does not permanently block later work.
The checkout lock and binding consume two bounded records regardless of the number of completed
Works or recipes. Private spools created by an earlier format can be inspected and exported
read-only, but they cannot be enabled for configured execution or rebound to the same checkout.
This conservative refusal avoids splitting a possibly live legacy lock or granting replay authority
to copied history.

Configuration adds marked `agentscm change-capture hook-claude` entries for the eleven lifecycle
groups and preserves unrelated hooks. Each hook has a 20-second host timeout. The hook resolves the
exact current Work and generation for the native session on every invocation, appends locally
before delivery, then replays at most ten pending envelopes in causal order through the existing
client connection with a five-second delivery deadline. Only an HTTP 200 acknowledges an envelope.
The durable scheduling cursor advances after every attempted envelope, so more than one page of
retained rejected records or a persistent transient failure cannot indefinitely hide newer work.
Only an HTTP 200 writes the separate acknowledgement; rejected, network-failed, and transiently
failed envelope bytes remain retained and unacknowledged for later retry and diagnosis.
The spool is durably bound to its canonical Hub, repository, and worktree namespace and rejects a
mismatched reuse.

When exact current Work resolution is unavailable or returns no authorized target, the hook keeps
only a bounded sanitized local fact. It strips prompt text, tool input/output, error, and cwd even
in local-conversation mode. Every deferred occurrence reserves a replay-stable sequence, including
events with provider identities. An unresolved `PreToolUse` also captures the source and canonical
recipe digest at that time when possible. If that observed session already had an exact authorized
target, the deferred record copies that target and the next reachable hook makes the outage visible
under that exact work, generation, and canonical session. A later resumed generation cannot absorb
the old gap. When the observed and canonical ids are identical, the next retained event exposes the
reserved sequence hole directly; an alias publishes a deterministic `capture_interrupted` gap so
the canonical stream does not hide it. The sanitized fact remains local and is never rewritten
into a Hub action or treated as attributed conversation content. Records created before any exact
target remain explicitly unassociated; inspect them locally with `agentscm change-capture deferred
--spool /absolute/private/spool`. Legacy private deferred records without the newer target and
reserved-sequence fields remain readable with a null sequence and cannot produce an attributed
gap; AgentSCM does not invent their association or completeness. Capture and delivery errors produce a local warning and return
success so they do not block the host; retained envelopes remain pending for a later hook. Disable
capture and remove only the marked AgentSCM hooks with:

```text
agentscm change-capture unconfigure --root /absolute/repository
```

Unconfigure retains the private binding, spool identity, and safety lineage with `enabled=false`.
It does not delete capture history or permit the same checkout to adopt another spool. An exact
same-inode offline spool move can update the stored path after inspection; replacing or recreating
the spool at the same path is a different identity and is refused before mutation. Re-enabling the
same identity may update operational settings such as rules, recipes, setup, capture mode, and
increased limits. Existing uncertain attempts remain available only to the explicit reconcile path.

`metadata-only` is the default. Use `local-conversation` only after choosing the private retention
location and access policy. A retrospective JSONL import must name one explicit local file and one
session; it starts with an `adapter_started_late` gap because saved history is not proof of live
observation. Malformed or oversized records produce a capture-interrupted gap. Imports select
visible user text, assistant text, tool use, and tool results; they do not copy the whole provider
data directory.

An execution rule is `TOOL=execution:COVERAGE:RESULT_SOURCE`. It reports whether the native action
succeeded, failed, was incomplete, or is unknown; its verification result is always `unknown`.
Therefore an ordinary Bash `printf` is an execution whose verification remains `unknown`, not a
passed check. Native verification rules can classify an exact configured tool input, but only an
explicit integer provider result can map it to `passed` or `failed`; the current Claude Code native
callback may omit that result. The managed `run-recipe` path instead observes its own child status
and emits the single verification action while the matching Bash hook retains lifecycle events and
suppresses a duplicate native action. `process_exit_code` is accepted only
for Bash and PowerShell and reads an explicit integer `exit_code` or `exitCode`; a missing or
malformed value is unknown. `unverified` is appropriate for edit tools. A failure hook records a
failed action and an incomplete verification when applicable. Successful hook transport alone
never means that a check passed. An ordinary action compares its current completion setup with the
setup bound at start before checking source drift. A changed setup makes the retained finish stale;
later retries reuse that original finish even if the current setup, rule configuration, source, or
Work association has changed again.

A registered recipe is a private, bounded record containing an id, direct argv, coverage,
timeout, and combined output limit. Its canonical digest includes all of those execution
semantics. `run-recipe` accepts only a registered id and resolves the current authorized target;
it offers no command, result, source, setup, or observation-scope override. The executable must be
an absolute path, runs directly at the repository root with null stdin, and remains subject to the
ordinary visible Bash invocation and host permission flow. AgentSCM durably retains the configured
verification start before spawning the child, streams only the admitted bounded stdout/stderr
prefixes, and maps the real child exit status to passed or failed. Timeout, cancellation, output
limit, spawn, and I/O stops cannot report a passed check. The exact managed native `PreToolUse`
occurrence derives the action identity from the observed session and provider tool-use id.
Canonical target, command, and caller timestamp do not participate in that identity. Command,
native tool, typed input, typed result, and error facts are covered by a private canonical phase
fingerprint; only the volatile callback timestamp is excluded. A changed fact under the same
occurrence is a conflict, including a premature Post whose later result differs. Retrying an exact
occurrence reuses the original retained or compacted Pre/Post and action receipts and never
launches the child again. Replay resolution runs under one permanent private managed-capture lock
before association, sequence, artifact, event, gap, or action mutation. That lock serializes local
callers only and is never child-termination evidence. A source-enriched marker without its durable
Pre event cannot launch the runner. Original-bound Post and replay writes do not reset a newer
current Work association for the same observed native session. Ambiguous, missing, or hash-only
replay knowledge is refused before spawn.
The phase fingerprint is retained only as a local SHA-256 value; it does not add raw hook payloads
to Hub envelopes.

Managed occurrence format v4 is committed last while initializing a new or empty spool. The commit
binds the spool root directory and both permanent lock-file identities. AgentSCM never creates or
migrates that commit during later capture. Any nonempty spool without a valid v4 commit—including
earlier target-bound formats, configured action history, opaque history, or a partial
initialization—is refused for all capture operations without changing its bytes. Cooperating
operations revalidate the committed directory and lock identities at guarded acquisition
boundaries. An offline same-inode move is supported; live moves, restores, and replacements are
unsupported and are refused when a later path check observes the changed identity. These checks do
not claim protection from the same operating-system owner rewriting the path after validation.

Authority-bearing open, recovery, mutation, and delivery require the spool root and every
operational directory to remain owned by the effective user with no group/other access. Initial
open also rejects retained authority/evidence files with unsafe ownership or modes. AgentSCM does
not chmod or recover an unsafe existing lineage. Read-only inspection remains available to diagnose
that state without granting replay or delivery authority.

`change-capture inspect --spool PATH` provides bounded read-only classification and lists bounded
pending, deferred, quarantine, and attempt projections with explicit omitted counts. It reports
unknown completeness and grants no execution authority. An outbox envelope is excluded from the
pending projection only when its regular acknowledgement marker contains the exact envelope digest.
A malformed or unreadable acknowledgement stops delivery and compaction for the spool. Native hooks
warn and remain fail-open. Automatic marker repair is unsupported; retain inspection/export evidence
for operator diagnosis before changing the spool. Inspection is a bounded snapshot: an empty
directory marker can remain pending without an issue entry, and concurrent compaction may produce
an acknowledgement diagnostic when an envelope disappears during the read.
`change-capture export --spool PATH
--destination PATH` creates a bounded, byte-verified, nonresumable archive without mutating the
source. The destination is installed with an atomic no-replace operation; an existing or racing
destination is never adopted, chmodded, written, or deleted. Export creates a 128-bit-random
private anchor with a finite collision budget, validates the opened anchor's directory type,
effective-user ownership, and Unix group/other mode bits. On macOS it then clears the extended ACL
through that descriptor and re-reads it; a clear or verification error, including an unsupported
filesystem response, fails closed before any archive child or evidence byte exists. It then performs
a bounded enumeration through a fresh descriptor and requires the cleared anchor to be empty before
creating its owned child; unexpected entries remain untouched and make the export fail. Descendants
created beneath the cleared, verified-empty anchor therefore do not inherit an allow entry from the
destination parent. Linux relies on the requested `0700`/`0600` creation modes constraining the mask
of an inherited POSIX default ACL. The atomic install resolves its source relative to the retained
anchor rather than the mutable anchor pathname. Required anchor and destination-parent name
transitions are synced before any evidence bytes are copied. All archive descendants are then
written relative to the retained descriptor for the directory created by this export. If an anchor,
destination, or parent pathname is renamed or replaced, the replacement is untouched and the
exporter does not recursively delete owned partial output by pathname. Portable Unix APIs cannot
remove an empty directory by its retained descriptor, while pathname cleanup would
reopen the replacement race. On a successful return, the exporter has intentionally left the
verified-empty random private anchor in place; another parent-authorized actor may still move or
remove that name after it is observed. A failed attempt can instead leave an unverified or
relocated anchor, unexpected foreign entries, the owned `owned-output` child tree, an installed
destination tree, or more than one of those artifacts. The error reports every known name and
identity, any location/privacy/content uncertainty, and never cleans through a mutable pathname. If
an expected name no longer resolves to its recorded identity, the object's current location and
existence are unknown: it may have been moved or removed, and descriptor lifetime does not promise
pathname recovery after return. A sync failure never claims a crash-uncertain installation
succeeded. A destination nested beneath the source is refused before
creation. Files above the bounded
64 MiB per-entry digest limit, unreadable files, and files whose digest changes during copying are
refused; the exporter never labels a size-only copy exact. A valid spool configured with larger
artifacts may therefore remain inspectable but not exportable by M1. A failed export never changes
the source. Inspection and export are recovery evidence, not
authorization to clear an uncertain attempt or rebind an existing checkout. M1 has no operational
same-checkout rotation: a copied,
recreated, or newly initialized spool remains a different identity. A genuinely different
canonical checkout root may establish its own fresh binding, while the old spool remains its own
evidence boundary.

The exact visible Bash command for the registered runner is carried in the native hook
configuration. Its `PreToolUse` and completion hooks still retain ordinary lifecycle facts, but an
exact command match suppresses the second generic Bash action receipt. Prefixes, suffixes, and
other shell commands do not match. The runner does not rewrite hook input, parse output text,
alter permission rules, or infer a result from native hook transport.

For a configured tool, `PreToolUse` durably records the action start before the tool executes;
`PostToolUse` or `PostToolUseFailure` records completion against that retained start. The recipe
digest identifies the canonical native tool input observed by this adapter. It does not prove that
the recipe was approved or that the configured setup came from an independent authority. Duplicate
native starts and finishes compare the incoming canonical receipt with retained bytes. A changed
tool input or typed result under the same tool-use identity is rejected as a conflict; an exact
retry does not capture source again.
Each start also receives a durable spool-local observation order and a random persisted observation
scope. The Hub compares that order only inside the same observation scope, canonical session, and
current work generation. Missing or different scopes fall back to Hub receipt order; caller clocks
and order values from independent spools are not comparable and do not strengthen assurance.

The schema and synthetic fixtures target Claude Code 2.1.300. Field compatibility alone does not
qualify a live adapter. See the [implementation status](implementation/agentscm-build-status.md)
for the tested host, runtime evidence and remaining limits. A later host version requires
qualification again.

## Source-bound observed commands

Before launching a command, AgentSCM durably records an action start with the source binding,
recipe digest, setup digest, and start time. Clean Git source is bound to immutable head, tree, and
merge-base identities. For a dirty checkout, AgentSCM reads tracked and nonignored untracked files,
retains their bytes and a content-addressed manifest in the private spool, and exposes only the
manifest artifact identity in the source binding. It does not modify the index, refs, hooks, or
working files.

A clean checkout whose attributes or Git settings can transform materialized working bytes is also
captured from the worktree and reported as partial when omissions remain. This includes configured
clean/smudge filters, working-tree encodings, ident/eol transforms, and CRLF materialization. The
immutable Git tree alone is not presented as the exact materialized input in those cases.

Ignored files are outside the captured scope. Index state, submodule contents, and unsupported file
types are explicit omissions when present. Snapshot reads traverse from a retained worktree
directory handle and reject symlink components, so replacing an ancestor cannot redirect a read
outside the opened root. Unsafe paths, changed root identity, non-UTF-8 paths, and configured file
or aggregate bounds stop capture rather than produce a false exact-source claim.
Captured symlink targets are stored as local bytes. A dirty binding with omissions is partial even
when all permitted working-file bytes were retained.

The command runs only after the action start is durable. stdout, stderr, exit status, and a local
command descriptor are retained separately. A second source capture reports whether the endpoint
source changed; `none_observed` does not prove that no transient mutation happened during the run.
Result, evidence availability, coverage, drift, and freshness remain separate facts.

The schema reports these developer-checkout observations as `caller_asserted`. Running a child
process and retaining its output does not establish independent verification or a trusted local
boundary because the candidate and checkout can influence the observer. Stronger assurance
requires a separately qualified protected or independent execution path.

Read the resulting shared projection without running a model or mutating Hub state:

```text
agentscm change-capture show --root /absolute/repository --work-id wrk_example
```

## Codex status

The existing Codex hook integration exposes session boundaries and selected edit starts. It does
not currently expose user prompts, assistant messages, every tool completion, or retrospective
visible history to this adapter. The capability declaration therefore reports those fields as
unavailable and does not claim complete Codex session capture.
