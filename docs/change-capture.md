---
title: "Local change capture"
---

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
retention class, and availability. Conversation bodies leave the machine only from a session in `shared-conversation` mode while
the repository's Hub setting shares agent conversations, as described in
[Sharing conversations with the Hub](#sharing-conversations-with-the-hub); the Hub refuses a
shared-approved body declared by a session that was not in that mode, and refuses every upload
while the setting is off.

Metadata-only capture does not retain conversation bodies. Local-conversation capture retains the
selected visible prompt, assistant-message, tool-input, and tool-result blocks in the private
spool. Claude thinking and redacted-thinking blocks and Codex reasoning are never selected. Passive
hook capture does not call a model or send content to a provider. Capture records Claude Code and
Codex sessions; what differs for Codex is in the [Codex adapter](#codex-adapter).

Capture has three modes per checkout, and `metadata-only` is the default:

| Mode | Bodies kept locally | Bodies sent to the Hub |
|---|---|---|
| `metadata-only` | No | No |
| `local-conversation` | Yes, unredacted | No |
| `shared-conversation` | Yes, redacted, until the Hub holds them (see Upkeep below) | Yes, after the Hub accepts the event |

The spool has finite operator-configured record, total-byte, and per-artifact limits and never
silently evicts retained evidence the Hub does not hold: the only bodies upkeep removes are shared
bodies the Hub answered for (see Upkeep below). Defaults are 4,096 records, 64 MiB total, and
16 MiB per artifact. Three derived files at the spool root count toward those limits:
`session-<digest>.events`, an append-only log of the sequences a session has issued (older
whole-map session files are still read), `delivery-index-v1.jsonl`, a delivery-order index that is
rebuilt from the outbox whenever it is missing, torn or disagrees with an envelope, and
`bodies-v1.log`, the ledger of shared bodies upkeep may free. Configuration refuses an action-capable spool below 48 records or 64 KiB total because
that space cannot retain the declared minimal managed lifecycle and its recovery metadata. At the
48-record floor, 12 records remain reserved as health headroom and 36 are available for the fixed
metadata plus configure, session start, managed Pre, child start and finish, and managed Post
records. This admission floor does not promise that every maximum-size source, output, or artifact
shape fits; larger captures still stop honestly at their configured record or byte quota. Limits
include the permanent spool and managed-capture lock inodes, observation scope, order and
compaction indexes, and the managed-format initialization commit. Initialization admits the
entire fixed layout before writing it and writes the format commit last. A crash before that commit
leaves a nonempty spool that is refused without repair or migration. Metadata
headroom is reserved for finishing started actions and recording an explicit gap when quota
pressure pauses ordinary capture: `local_retention_expired` when the spool has no room left, so the
Hub can say that this computer's store was full, and `capture_interrupted` for any other bound. A
later retained event at a
higher session sequence shows that capture resumed; the gap remains and coverage does not return to
complete. If content cannot be retained, the receipt records `missing` availability and a gap
instead of losing the completion.

Acknowledgement hides an envelope from delivery without immediately deleting it. Explicit local
compaction can replace acknowledged ordinary event envelopes with exact idempotency digests and
completed action pairs with compact retained receipts. Usage records and provider-history gap
envelopes retain their full original contents in the replay index. Managed recipe Pre/Post events
retain their full original Work, generation, and session envelope alongside the action receipts so
target or timestamp changes cannot turn a provider-occurrence replay into new work. This finite
replay index is never silently pruned: when its configured record or byte bound is reached,
compaction refuses and requires explicit export or a larger spool. Compaction never removes pending
envelopes, incomplete action starts, artifacts, deferred diagnostics, or the source metadata needed
by an outstanding action. An acknowledgement does not prove that the Hub holds a body: delivery
acknowledges a batch without uploading its bodies once its binding no longer shares, and a shared
body with nothing redacted is the same file as a local body with the same bytes. So upkeep frees
only shared bodies it has proof for (see below), and never any other local artifact. Recoverable
fully written temporary files are completed under the spool lock in every private spool directory
after a crash. Exact duplicates are removed and malformed or
ambiguous files are moved into a private quarantine. Quarantine files count toward the configured
record and byte limits and are never deleted automatically. Inspect them with `change-capture
quarantine`; `change-capture prune-quarantine` deletes only exact opaque names selected by the
operator. A committed spool may reopen above its configured write cap after crash debris is
quarantined so bounded inspection and explicit pruning remain available. Every ordinary admission
still counts the complete physical usage and refuses until capacity is restored; recovery itself has
a fixed physical-record scan bound. Active writer temporary files are protected by the spool lock
and are never quarantine prune targets. Nothing the Hub has not accepted, and no quarantine file, is
ever deleted from the local spool automatically; upkeep (below) removes only acknowledged records,
sanitized deferred facts, which are never uploaded, tool-call claims, local bookkeeping that is
never uploaded either, and shared bodies the Hub holds or no event names. The Hub copies of shared
bodies expire on their own (see [retention and deletion](#retention-and-deletion)).

**Upkeep.** A spool holds 4,096 records and 64 MiB by default. A hook that captures an event tidies
its spool beside its Hub lookups and before it writes the event, so a spool that filled with what
the Hub has accepted makes room before the event is refused; delivery comes after. Session start and
turn end tidy a spool past 80% of its record or byte limit; every other event tidies only a full
one. Upkeep deletes sanitized deferred facts that nothing will read again: one bound to a session
observed under another session's id is kept for 7 days, until that session has emitted the gap it
stands for; every other one (from before a session had Work, or bound to a session under its own
id, whose skipped sequence the next retained event already shows as a gap) counts as local
diagnosis, of which only the newest 256 from the last day are kept. It deletes tool-call claims
nothing will complete: an ordinary claim that has seen both its Pre and its Post is done and goes at
once, whether or not its Post removed it; a claim whose other phase has not come in a day (an
interrupted tool, a killed hook, an ended session) never will; a Post whose Pre recorded no action
start ends its claim at once; a claim a recipe attempt names stays until that attempt is
reconciled. It then compacts one page of up to 500 acknowledged envelopes, as `change-capture
compact` does (which takes at most 100), freeing up to 1,000 records, and skips that scan when
nothing is acknowledged.

It then frees up to 500 shared conversation bodies this computer no longer needs. A shared body is
freed only when all of these hold: a `shared-conversation` session created its file; nothing ever
kept the same bytes as local content (a local conversation body, a source snapshot, a continuation
or intent-review artifact); nothing has retained it for an hour; and for every Work whose event
names it, the Hub answered HTTP 204 to that body's upload for that Work. A body no event names (its
event was refused or never written) goes after the hour. Neither an acknowledgement nor compaction
is proof, because delivery acknowledges without uploading once a binding stops sharing: a body the
Hub never answered for stays, however old, because it is the only copy. The facts sit in a small
ledger at the spool root, `bodies-v1.log`: the body was created by a shared retain, also kept as
local content, named by an event of a Work (written before the event), and answered 204 for a Work.
The check and the removal happen under the spool lock. An event that names a shared body that is no
longer there is not written; the capture hook records a gap for it instead. A ledger that cannot be
read is moved to quarantine and a new one is started, which leaves the bodies it named on disk for
good. Bodies kept before this ledger existed are never freed, because nothing says whether local
content shares their file. Once a body is freed, the local [repository
explorer](repository-explorer.md) shows it as missing; the Hub copy is the one to read.
An import (`import-claude`, `import-codex`) retains every body before it appends any batch, so a very long import on raised
limits, whose store a live session tidies meanwhile, can stop with a refusal; run it again. A later
batch of the same Work that names a body already answered 204 adds no new fact, so the body can be
freed while that batch is still pending; this matters only if the Hub then loses its copy.

If the spool is still full after upkeep (inside the
headroom kept for gaps, where new facts are refused), the session-start context says so instead of
saying that sharing is on, because capture then records only gaps, and names the commands that help:
`change-capture inspect --spool <spool>` to see what it holds, and `unconfigure` then `forget` to
start a new store. The old store stays on disk, but records still waiting in it are no longer
delivered. `change-capture status` reports the same: its `session_start` is that sentence, and
`store_full` and `store_full_fix` say it as fields. Each session's own sequence and association
records are not expired yet, local conversation bodies, shared bodies the Hub never answered for
and bodies kept before the ledger stay until the store is exported or replaced, and the compaction
index keeps a receipt of every compacted envelope until its bound, so a checkout that runs many
thousands of tool calls can still fill its store; that is when the commands above are needed.

The compaction index is the durable commit point. Crash recovery completes its exact envelope,
acknowledgement, native-observation, active-order, and delivery-cursor cleanup. A cursor naming
compacted history cannot wedge later delivery, including when a repeated compaction has no new
selection. Completed compacted action starts retain their exact observed order for idempotent
replay while their active order-ledger entries are reclaimed. If the active order ledger reaches
its finite bound before compaction, the native hook retains the ordinary lifecycle fact, records
one priority `capture_interrupted` pause marker, and remains fail-open.

## Sharing conversations with the Hub

This is alpha. A session in `shared-conversation` mode retains the same selected bodies as
`local-conversation` (visible prompts, assistant messages, tool input and tool results; never
thinking blocks), but scrubs each one for secrets before it is written to the spool and then
uploads it to the Hub's object store.

### The repository setting

Whether a repository shares agent conversations is a Hub setting, **Share agent conversations**,
not a per-checkout switch. It is on by default on a new Hub, and stays off on a Hub that already
had people when the default became on. A repository admin (GitHub `admin`, GitLab maintainer
or owner) or a Hub admin changes it for one repository; a Hub admin can also set a Hub-wide
default that applies to every repository without its own setting. The routes are in
[conversation sharing](hub.md#conversation-sharing).

- **Checkouts follow it.** While it is on, every connected checkout of the repository, and every
  linked worktree of one, captures in `shared-conversation` mode without `configure`. The AgentSCM
  plugin hook asks the Hub at each session start; when the Hub shares and the checkout has no
  capture binding, it creates one with the one-step defaults below, marked `follows_hub`, and
  installs no hooks of its own: the plugin hook captures for it, including the events it
  registers only for capture (for Claude Code `PostToolUseFailure`, `MessageDisplay`,
  `PreCompact`, `SubagentStart`, `SubagentStop`; for Codex see the
  [Codex adapter](#codex-adapter)). Capture starts with that session. A binding that follows the
  Hub is written as config version 7, which older binaries refuse.
- **People are told and can opt out.** The first time a person's session shares in a
  repository, its agent is told in the session-start context that conversations are shared,
  who can read them, and how to stop. This is best effort: the notice is recorded as shown
  before the hook's reply is printed, so a reply the host cuts off loses it, and a session
  already running when the setting is turned on is told at its next start. `agentscm change-capture opt-out` (in the checkout) stops
  that person's own sessions in the repository, on every computer, from uploading;
  `change-capture opt-in` undoes it. The choice is per person and repository, stored on the Hub.
  Opting out also hides what the person shared before: their entries leave other readers'
  listings and their bodies are no longer served to them, while the person and Hub admins still
  read them. The bodies stay stored until they expire or their author deletes them with
  `change-capture conversation --delete`, and opting in again shows them again.
- **Every session says whether it shares, and why not.** The session-start context says
  "conversation sharing is on for this session", or that it is off with the reason and the one
  command that changes it:
  - this checkout's own setting is `metadata-only` or `local-conversation` while the repository
    shares (the local setting caps the repository's): `change-capture set-mode --root <checkout>
    --mode shared-conversation`, or, for a binding that follows the Hub, `change-capture
    unconfigure --root <checkout>` then `change-capture forget --root <checkout>`, after which the
    next session follows the repository;
  - capture is turned off in this checkout, or was set up for another repository or Hub, while the
    repository shares: `change-capture forget --root <checkout>` (after `unconfigure` if it is still
    on), and the next session follows the repository;
  - the person keeps their own sessions out: `change-capture opt-in`, and also the `set-mode` fix
    above when this checkout's own setting caps it;
  - the session keeps a lower mode it started under (it started before sharing was on, or its
    saved mode could not be read): the next session shares;
  - the repository does not share: a repository admin or a Hub admin turns it on in the Hub's
    Settings;
  - the Hub did not answer (out of time, unreachable or refused): whether the repository shares
    is unknown, and the context says so instead of "off". A checkout without a binding records
    nothing in that session and asks again at the next one;
  - an older Hub without the sharing setting: the context says only that sharing is off.
- **Off means off.** The client asks the Hub again on every hook call, and the Hub itself
  refuses uploads (403 `sharing_off`) and lists nothing as missing while the setting is off or the
  person opted out, so bodies not yet uploaded stay declared `local`; they are not uploaded later
  if sharing is turned back on. A followed binding keeps recording metadata (events and action
  receipts, no bodies) while sharing is off, until `unconfigure`. Turning it off recalls
  nothing already uploaded (see [retention and deletion](#retention-and-deletion)).
- **`set-mode` only lowers.** The Hub's answer caps every binding: a binding that follows the Hub
  shares only while the Hub says so and captures metadata only otherwise; any other binding's
  `shared-conversation` becomes `local-conversation` while the Hub says no. `set-mode` (or
  `unconfigure`) on a checkout lowers it further; it never raises sharing above the setting.
  When the Hub gives no answer (an older Hub, or no reply in time) a binding keeps its own mode,
  a followed binding is never created, and the Hub still refuses what it does not allow.
- **Existing shared checkouts need the setting.** A checkout configured for
  `shared-conversation` before this setting existed shares again only once its repository's
  setting (or the Hub-wide default) is on; until then it keeps its bodies local.
- **Redaction is unchanged.**

Linked worktrees each get their own binding and private spool. A checkout counts toward the
limit of 128 capture bindings; at the limit, followed bindings of worktrees that no longer exist
are dropped to make room (their spools stay on disk). `unconfigure` and `forget` remove a
followed binding like any other, and a disabled one is never re-created by the plugin hook;
re-enabling it from the menu bar or the local page gives it hooks of its own.

From the plugin hook, capture shares the hook's time: it gives up at 2.5 seconds (4 at session
start). A target lookup that runs out leaves a sanitized deferred fact instead of the event, and
records not yet delivered wait for the next hook.

### Per-checkout configuration

A checkout can still be set up by hand, with its own hooks; the repository setting caps its mode
as described above. Choose the mode when configuring (`change-capture configure --capture-mode shared-conversation`) or
change it later on an enabled binding:

```text
agentscm change-capture set-mode --root /absolute/repository --mode shared-conversation
agentscm change-capture set-mode --root /absolute/repository --mode metadata-only
```

`set-mode` changes only the stored mode. It requires an enabled capture binding for the checkout
(run `configure` first), leaves hooks and the spool untouched, and never deletes local or Hub data.
`change-capture status` reports the stored `capture_mode` beside what the Hub says:
`repository_shares` (null when the Hub did not answer or has no sharing setting),
`effective_mode` (the mode a session starting now captures in; `off` when no hook captures for
this binding, null when the Hub did not answer) and `session_start`, the sentence that session
would say. When `repository_shares` is true, you have not opted out, and `capture_mode` is not
`shared-conversation`, this checkout's local setting is what keeps sharing off. The macOS menu-bar companion and the
Hub page opened from this computer (**Settings → Conversation sharing**) have the same switch per
checkout, **Share agent conversations with Hub**; it is unchecked by default and does not need
`configure` first (see below). Turning it off only switches the mode. The capture config is written as version 6 only while
some checkout is in `shared-conversation` mode or a binding records a non-default Claude settings
path (see below); an older binary refuses a version 6 file instead of silently dropping a field,
and the file returns to version 5 when neither applies. Downgrading the client after this release is
unsupported: older binaries cannot read the new session association records.

### One-step setup from the menu bar and the local page

Turning sharing on from either surface calls one library function, `enable_capture`, for a
checkout that has a current client connection; turning it off calls `set_capture_mode`. All three
surfaces read the setting through one function, `capture_setting` (configured, enabled, mode, and
`lineage_mismatch` with the repository and Hub the binding was saved for); nothing keeps a second
copy of it. The page shows the distinct states Not set up (no binding), Off (disabled binding),
Metadata only, Local only and Shared with Hub.

**Consent is bound to what was shown.** The menu bar app and the page pass the repository and Hub
they displayed. `enable_capture` (and `set_capture_mode` when it turns sharing on) read the
checkout's connection (connections lock, then the capture config lock) and refuse with `this checkout is now connected
to <repository> on <hub>; refresh and confirm again` if it is not that one, so a checkout
reconnected to another Hub between the consequences and the click is never shared there. Turning
sharing off and the `set-mode` and `configure` commands (where you typed the command) pass no
expectation. Whether to create the binding, re-enable it or only change the mode is decided from
the bindings read under that same lock, so a concurrent `configure` is not overwritten.

The menu bar app asks before it acts. Clicking **Share agent conversations with Hub** only opens
the status window with the consequences and adds **Confirm: share conversations with <hub>** to
that checkout's menu; only that item turns sharing on, and it disappears if the checkout, its Hub
or its mode changes, or after two minutes. Turning sharing off needs no confirmation. The page does the same with its own
confirmation panel.

- With an enabled binding it only changes the mode, exactly like `set-mode`.
- With a disabled binding (after `unconfigure`) it re-enables through the `configure` code path with
  everything the binding saved (spool, worktree id, base ref, setup digest, rules, recipes, limits)
  and reinstalls the hooks in the Claude settings file the binding recorded (below), else the
  default `ROOT/.claude/settings.local.json`.
- With no binding it builds one through the same code path as `configure` with these defaults:
  the private spool `<config>/agentscm/capture-spools/<random>` (mode 0700, 16 random bytes in
  hex, recorded in the binding); worktree id `checkout_<32 random hex characters>`; setup digest 32
  random bytes in hex; base ref the checkout's default branch from `origin/HEAD` as `origin/<name>`
  when that resolves to a commit, else `origin/main`; one action rule,
  `Bash=execution:partial:process_exit_code`; no recipes and no Claude history root; default spool
  limits; hooks in `ROOT/.claude/settings.local.json`. The ids are random, not derived from the
  path, so they cannot be recomputed offline from Hub records by guessing paths and two computers
  with the same path never share one; the saved binding is what keeps them stable across
  re-enables. The setup digest is a caller assertion that identifies this default, not a
  measurement of the agent setup.

A Claude settings file that one-step setup creates, and the transaction files saved beside it, are
added to the repository's local exclude unless Git already ignores them, so they never appear as
untracked files. This is done only after the binding is installed, creates `.git/info` if the
repository was cloned without it, and never fails the enable: if the exclude cannot be written the
result carries a `warning` instead. A file that already existed is not touched and the tracked
`.gitignore` is never edited. The exclude (`git rev-parse --git-path info/exclude`) is shared by
every worktree of the repository, so the line applies to all of them. It stays after `unconfigure`;
`forget` removes only the lines setup added for that checkout's settings file (recorded beside the
exclude), and keeps them while another binding in the same repository still uses the same relative
settings path. `forget` of a deleted checkout cannot reach its repository and leaves the exclude as
it is.

A binding records its Claude settings path (`claude_settings`) only when it is not the default, and
`unconfigure` and re-enabling use the recorded file. A file that records it is version 6, which
older binaries refuse rather than ignore the field; bindings at the default path keep the config
readable by them.

A checkout without a current client connection gets the error `no client connection for this
checkout` and nothing is changed. Only an explicit request for `shared-conversation` selects that
mode; setting up capture in `metadata-only` mode never uploads a conversation body. Use
`change-capture configure` for anything else (other rules, recipes, a history root, a chosen spool).

A disabled binding keeps the repository and Hub it was saved for, because its private spool keeps
that history apart. If the checkout is now connected to another repository or Hub, turning sharing
on fails with that explanation instead of a lineage error. To start fresh run
`agentscm change-capture unconfigure --root PATH` (if capture is still on), then
`agentscm change-capture forget --root PATH`, which removes only a disabled binding from the config.
Both work on a checkout that has been deleted: pass its absolute path as it was saved; symlinks in
the parent directories that still exist are resolved, the deleted part is matched as given, and
nothing is touched in the missing tree (its hooks went with it). `unconfigure` fails with `no
capture binding for this checkout` when nothing matches.
Turning sharing down (to `metadata-only`, or from `shared-conversation` to `local-conversation`)
needs only the capture config lock, not a current client connection: it works after the connection
was removed or the checkout was reconnected to another Hub, never opens the spool, never uploads and
never changes the saved lineage. Raising the mode still needs a current connection and matching
lineage. Removing or replacing a checkout's client connection (`client remove`, `client setup` for
another repository or Hub) also lowers a `shared-conversation` binding for that checkout to
`metadata-only`, so reconnecting the same repository and Hub never resumes sharing without a new
confirmation. This includes a binding that follows the Hub; its session-start context then names
`unconfigure` and `forget`, which let the next session follow the repository again. Capture hooks
and delivery do nothing for a binding whose connection is absent.
The tray and the page then say "Sharing was set for <hub>; this checkout is now connected to
<other>" and offer only Off. It never deletes the spool: it prints the spool path (and `spool_retained: true`) so you can
`export` or delete it yourself. The next enable creates a new spool and new random ids.

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
  suffix keeps counters such as `max_tokens` and `input_tokens` readable. `pass`, `pwd` and `auth`
  count only as a separate name part (`pass`, `DB_PASS`, `dbPwd`, `auth`), so `bypass`, `compass`
  and `oauth_state` stay readable. Values inside JSON that was escaped into a string
  (`\"password\":\"x\"`) and token shapes glued to an identifier after `_` or `-`
  (`X_ghp_...`) are recognised too. A body that needed
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

Who can read a shared body: while the repository shares agent conversations, anyone who can read
the repository on the Hub (any of their tokens, including read-only ones and the Hub's web pages);
otherwise only the human who shared it (any of that person's tokens for the repository) and Hub
admins. Only the author and Hub admins can delete one; read-only tokens never delete. The owner of
the Work has no extra access to a helper's or reviewer's shared bodies. Each human's declaration of
a body is its own row, so two people sharing identical content each read and delete their own. A
listing shows a caller every author's entries while the repository shares, else only their own
(an admin sees all); a body request from anyone without access gets 403, whether or not that
digest exists. There is no per-body grant, and bodies are never included
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
`hub serve --shared-conversation-retention-days N` (1 to 3650). The Hub reports the configured
number to authenticated clients as `shared_conversation_retention_days` in `GET /v1/hub/whoami`.
`client status --check` carries it in the connection's `hub_check` (when the check is valid), the
menu bar app reads it from there, and the local page's `GET /v1/local/capture` lists it per
checkout (looked up with the checkout's own agent token and reused for a minute). Both surfaces
then say "kept N days on <hub>". When the number is unknown (an older Hub, an unreachable or slow
Hub, or no Hub check yet) they say "kept for as long as the Hub operator sets (30 days unless
changed)" and never name a number they were not given. The deadline is set when the body
is uploaded. A sweep every ten minutes expires due bodies and deletes their objects, in batches of
100 until none are due or 30 seconds are spent (the rest waits for the next pass); an object whose
removal fails is retried after 1 hour, doubling to at most a day, without holding up others. The author
or a Hub admin can delete a body earlier with `--delete` or the HTTP `DELETE` route (an admin deletes
every author's copy); deletion records a tombstone and then removes the object, so a deleted or
expired body answers 410. When two people shared identical content the stored object is removed
only once no other copy of it is available. A declaration that was never uploaded does not keep it;
if it is removed first, the other author's later upload writes it again, and an available copy whose
object is found missing returns to awaiting upload so the checkout uploads it again. A declaration
still not uploaded when the retention period after its declaration ends expires without ever having
an object. Identical bodies from the same author in one Work are stored once, under the first
declaration, so a later identical body shares that entry and its expiry.

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
prompts but may overcount retries; it does not claim exact replay identity. Claude Code hook
payloads normally carry no timestamp, so a hook without a valid one records the hook process's
receipt time (Unix seconds) as `observed_at`; that time is a receipt time, not a host-reported one.
It never enters event identity, which still hashes only the provider timestamp, and a re-run of
the same operation, even after the spool has compacted it, is a duplicate of the retained event
rather than a conflict, whatever its new receipt time. Transcript import keeps the
transcript's own timestamps, and Hub records written before this behaviour may hold `0`, which the
UI shows as "time not reported". Local deferred diagnostics also use a separate durable receipt
order. Raw prompt and result bodies stay local and do not contribute to shared event identity.
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

The Hub resolves the current target for a session whose Work is `active`, `paused` or `blocked`
at the session's current generation, and accepts event batches and action starts and finishes for
that Work in the same states. A Claude `SessionEnd` can make the Hub pause the session's Work (it
does once the Work's hold is no longer live), and a
resumed session (`claude --resume`, same session id) keeps capturing while the Work stays paused,
so excluding paused Work would defer the session's own `SessionEnd` and everything after a resume.
`finished` and `handover` Work, and any older generation after a takeover, are refused. Capture
only reads the state and never changes it; pausing, finishing and handing over remain Work
checkpoints.

When exact current Work resolution is unavailable or returns no authorized target, the hook keeps
only a bounded sanitized local fact. It strips prompt text, tool input/output, error, and cwd even
in local-conversation mode. One exception keeps a session's opening prompt: while the repository
shares conversations, a prompt submitted before the session claims Work is also held in a private
file under the config directory (the first 8 prompts of up to 64 KiB each; a file older than 24
hours is deleted when next read). When the
same session next resolves its claimed Work, the held prompts are captured first, under that Work,
and the file is removed; when the session's pinned mode is not shared, they are discarded.

A reply a `MessageDisplay` hook did not deliver (for example because the host stopped the hook) is
recovered at the turn's `Stop`: the hook reads the session's transcript on from where the previous
`Stop` stopped (at most 8 MiB per turn) and captures each assistant text no `MessageDisplay`
delivered, matched by its whole text (fragments of one message are joined, whitespace ignored), as
a `MessageDisplay` keyed by its transcript entry, so a repeat is a duplicate. Recovery starts where
the session claimed Work, or where this build first saw the session: replies before that are never
recovered. The texts already seen and the read offset are kept per session in the same private
directory, under a lock, and a session start removes files there untouched for 7 days. Every deferred occurrence reserves a replay-stable sequence, including
events with provider identities. An unresolved `PreToolUse` that matches an action rule also captures
the source and canonical recipe digest at that time when possible; other tool events skip the Git
scan, as they do for claimed Work. If that observed session already had an exact authorized
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

## Codex adapter

Codex command hooks use the same event names and fields as Claude Code's, so Codex is captured by
the same hook path, with `codex` recorded as the session's harness. The modes, redaction, the
spool and its bounds, gaps and the repository's sharing setting apply unchanged. The AgentSCM
plugin's hook (`agentscm hook codex --client-connection`) captures for a checkout whose binding
follows the Hub, and also for one configured by hand, whose own hooks are Claude Code's. There is
no `configure` step for Codex. Live capture reads only what a hook payload carries.

| Fact | Hook event | Notes |
|---|---|---|
| Session start and end | `SessionStart`, `SessionEnd` | A `fork` start, like a resume, does not prove earlier history was observed. Codex gives every end the reason `other`, and ends and starts the session again around each non-interactive `resume`. |
| Prompt | `UserPromptSubmit` | The `prompt` field. A prompt before the session has Work is held as for Claude Code. |
| Tool start | `PreToolUse` | Tool name, call identity and input. A shell command is `Bash`, an MCP tool `mcp__server__tool`. |
| Tool finish with its result | `PostToolUse` | The `tool_response` field: for a shell command, its output as text. Recorded as `returned`. |
| Assistant message | `Stop` | The `last_assistant_message` field: the last message of the turn, recorded just before the turn's end. |
| Token usage of one model request | `PostModelRequest` | See [token usage](#codex-token-usage). |
| Compaction, subagent start and stop | `PreCompact`, `SubagentStart`, `SubagentStop` | Markers, as for Claude Code. |

The plugin registers `UserPromptSubmit`, `PostModelRequest`, `PreCompact`, `SubagentStart` and
`SubagentStop` only for capture; they never reach the Hub's hook endpoint. Codex runs a new or
changed hook entry only after its person trusts it in `/hooks`, so a checkout that installed the
plugin before these entries existed records them once they are trusted.

What Codex does not expose to a hook is not recorded, and is not guessed:

- **An assistant message before a turn's last one.** No hook carries it. `import-codex` reads it
  from the session file.
- **Whether a tool call failed.** There is no failure hook. A shell command runs `PostToolUse`
  whatever its exit code, and its response is its output with no exit code, so it is `returned`
  and an action's outcome is `unknown`. An MCP call that failed, and a call that a `PreToolUse`
  hook refused, run no `PostToolUse`: each is recorded as started and never finished.
  `import-codex` reads the exit code and the failure from the session file.
- **Hosted tool calls**, such as web search. They run no hook.
- **Reasoning.** Codex stores it encrypted; the adapter never reads it.
- **An identity for a prompt or a session boundary.** `UserPromptSubmit` names the turn, not the
  prompt, and `SessionStart` and `SessionEnd` name nothing, so each is recorded with an
  `identity_uncertain` gap, like any event without a provider identity, and a Codex session is
  never complete.
- **A model request's identity** (see below), and **a reason for a session's end**.
- **A subagent's own prompts and replies.** Its tool calls and model requests are recorded when
  Codex runs the session's hooks for them; that was read from the hook schemas, which give those
  events an `agent_id`. The session that was run had no subagent.

### Codex token usage

`PostModelRequest` runs after each model request with its `model`, `status` and `token_usage`. A
request that reports counts becomes one request-scoped usage observation (`usage_basis`
`provider_result`, agent `codex`, provider `unknown`) and is submitted through the Work usage
endpoint, like Claude Code's. A request with no `token_usage` records nothing, so its usage stays
unknown. Usage belongs to Work: a model request made before the session has Work is not recorded
and leaves no deferred fact.

Codex counts cached input inside `input_tokens`, and a usage observation keeps the two apart. The
adapter therefore records `input_tokens` minus `cached_input_tokens` as input and
`cached_input_tokens` as cache-read input. When cached input is larger than input, input is
unknown. `cache_write_input_tokens` is recorded as cache-creation input only when it is positive,
because Codex writes 0 when a provider reports none; it is not subtracted from input.
`reasoning_output_tokens` is part of `output_tokens` and is not kept separately.

The payload does not name the request, so an observation is identified by its turn and its six
counts. The session file's record of the same request has the same turn and counts, so a session
that was recorded live and is imported later is not counted twice. Two requests of one turn with
identical counts are recorded once.

### Importing a Codex session file

Codex keeps the whole of a session only in its session file (`rollout-*.jsonl` under its
`sessions` directory). Codex documents that file's format as not stable for hooks, so no hook
reads it; `import-codex` reads one file that a person names:

```text
agentscm change-capture import-codex \
  --spool /absolute/private/spool \
  --input /absolute/codex-home/sessions/2026/11/01/rollout-example.jsonl \
  --work-id wrk_example --generation 1 \
  --session-id 11111111-2222-7333-8444-555555555555 \
  --operation-prefix import:example \
  --capture-mode local-conversation
```

`--session-id` is the id in the file's first record, which is also the end of the file's name; a
file that starts with another session's record, or with no session record, is refused. The import
has the bounds and gaps of `import-claude` and selects:

- the person's prompts and every assistant message, interim ones included;
- each command, file change, MCP call and web search, from the typed record Codex writes when it
  ends: a start at the time the record says it began and a finish with its input, the text of its
  result and a command's exit code. The finish is `returned` or `failed` as the record's status
  says, and neither when the record has no such status, as for a web search. A record's id is the
  call id Codex's hooks give the same call. A command or a file change has no tool name in the
  file, only its kind;
- each model request's token usage, as above. The command prints how many observations were new
  as `usage_observations`.

It does not select reasoning (counted as `private_blocks_ignored`), images, the instructions and
environment notes Codex adds around a prompt, or the model-level call records, which in recent
versions are scripts that wrap the calls above; plan updates and calls between agents, which the
file has only at that level, are therefore not imported. A subagent's file opens with a copy of its parent's
history; those records are skipped, and the subagent's file is imported as a session of its own,
with `--parent-session-id` when the parent is known. Importing a file again adds nothing. A
session that was recorded live gets its events a second time under the import's identities, as
with `import-claude`; its usage is not repeated.

### Codex qualification

Checked on 2026-11-12 against Codex CLI 0.167.1:

- **Its hook input schemas** (`codex-rs/hooks/schema/generated`) and hooks documentation, for the
  events and fields above.
- **One session run with these hooks** (`codex exec`, then `codex exec resume`, with a hook that
  logged each payload and handed it to the plugin's hook client) against a local Hub. The Hub
  recorded the session start, the prompt, three shell commands with their results, eight other
  tool starts, the last reply, the turn's end and the session's end, and one usage observation
  for each of the nine model requests made after the session had Work. The session's ten requests
  had the same turn and counts in the hook payloads and in the session file, and importing that
  file afterwards added only the one request made before the session had Work. This run is where
  the notes on shell responses, failed MCP calls and refused calls come from. It had no subagent,
  no compaction and no hosted tool call.
- **180 session files** written by versions 0.154.0 to 0.167.1 on one macOS machine, for the
  import. Each imports with as many events and usage observations as it has records of its own;
  together they hold about 7,700 typed tool records, 600 of them failed. In all of their roughly
  8,600 model requests `total_tokens` was `input_tokens` plus `output_tokens`, cached input was no
  larger than input, and none reported a cache write.

Hook fixtures have the schemas' fields and that session's value shapes, with every value made up.
The source code states the capabilities as `change_capture::codex::CAPABILITIES`.
