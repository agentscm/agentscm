# Native CLI reference

> **Experimental (alpha).** The native ledger commands below are not part of the main path, may change without migration, and have had limited testing. See the [advanced index](../advanced/README.md). The everyday commands (`quickstart`, `uninstall`, `demo`, `init-repo`, `hook`, `collisions`, `autopsy`, `intent`, `hub`) are described in the [README](../../README.md), the [setup guide](../setup-guide.md), [collisions](../collisions.md), [autopsy](../autopsy.md), [intents](../intents.md) and [hub](../hub.md).

`agentscm --help` (and `agentscm help`) lists only the core commands, in groups: `quickstart`, `uninstall`, `hub login`, `client setup`, `client remove`, `client doctor`, `open` and `tray`. `agentscm help --all` lists every command; all of them keep working by name, and `agentscm COMMAND --help` prints any command's usage. `workflow`, `recovery`, `team` and `package-native` are internal to the alpha and hidden even from `agentscm help --all`, as is the internal `git-remote-credential` helper. `setup` is also hidden: it is a contributor helper, documented in [native installation helper](../advanced/native-installation.md). The trusted-local task-workspace entry through `workflow` is documented below; the other hidden commands are not documented here. Other native commands are marked "[experimental]" in `agentscm help --all`.

## Commands

Every command accepts `--data-dir`; its default is `.agentscm`. The global `--no-telemetry` flag disables product usage telemetry for that invocation. Most commands print one JSON value on standard output on success, and one JSON value (`{"ok":false,"error":...,"error_kind":...}`) on standard error with a nonzero exit on failure. When standard error is a terminal, a failure is one plain `error: ...` line instead, without `error_kind`; scripts and agents that read standard error through a pipe keep the JSON. The exceptions are:

- `--help` and `--version` print plain text.
- `serve`, `hub serve` and `team serve` print nothing on standard output; they log to standard error.
- `hub` subcommands are text-oriented: `login`, `logout`, `sessions remove`, `token revoke`, `connections outbound` and `connections add-repo` report on standard error; `token create` prints only the secret on standard output; `sessions list`, `token list`, `connections list` and `connections refresh` print text lines; `github setup` prints only the setup URL.
- `hub publish` and `hub slack` print a multi-line JSON value; `hub publish` on an incomplete or rejected publication also exits nonzero.
- `intent` prints plain text (or nothing) by default and a hook JSON object with `--hook`; it deliberately exits 0 on coordination errors so that it never blocks an agent. `intent watch` runs until stopped.
- `setup` reports its errors as a structured JSON object.
- `hook` reads an agent's hook JSON on standard input and prints that agent's reply; it always exits 0, even on usage errors, and prints a valid "no information" reply when the hub cannot be consulted. `init-repo` prints plain text, shows a diff and asks before writing; without a terminal it needs `--yes`. See [supported agents](../agents.md).
- `quickstart` and `uninstall` are guided and print plain text. Without a terminal they need `--yes`; otherwise they refuse and list their flags. `uninstall` exits nonzero when it could not undo something, and keeps its record of it.

### `work`

`agentscm work start` and `agentscm work checkpoint` read strict JSON requests from a file or
standard input. Checkpoint `source_refs` accept repository paths, commit identities, Work
identities, declared captured-event metadata and exact prior versions of the checkpoint's own Work:

```json
[
  {"kind":"captured_event","session_id":"session-1","event_id":"message:42"},
  {"kind":"prior_work_version","work_id":"wrk_0123456789abcdef0123456789abcdef","event_seq":91}
]
```

Captured-event identities are declarations; the CLI and Hub do not fetch or verify a conversation
body. A prior-Work version can explicitly identify the checkpoint being corrected or superseded.
The Hub requires that exact same-Work version to exist when first recording the checkpoint, while
an exact operation replay continues to return its original receipt. See [stand-up and handover](../standup.md).

### `change-capture` (experimental, Unix)

After `agentscm client connect` has saved a write-capable connection for a checkout, configure its
Claude Code capture hooks once:

```sh
agentscm change-capture configure \
  --root /absolute/repository \
  --spool /absolute/private/spool \
  --worktree-id checkout_example \
  --base-ref origin/main \
  --setup-digest 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --capture-mode metadata-only \
  --max-spool-records 4096 \
  --max-spool-bytes 67108864 \
  --max-artifact-bytes 16777216 \
  --action-rule Bash=execution:partial:process_exit_code \
  --action-rule Write=execution:unknown:unverified \
  --recipe-file /absolute/private/test-recipe.json
```

A recipe file is private mode-0600 JSON with `id`, an absolute-executable `argv`, `coverage`,
`timeout_seconds` and `max_output_bytes`. Configuration accepts at most 16 distinct recipe ids and
digests and requires at least 48 private spool records and 65536 total spool bytes when a recipe is
configured. On `SessionStart`, the hook supplies bounded guidance containing the exact absolute
`agentscm change-capture run-recipe --id ...` invocation. The managed command accepts no command,
source, Work, generation or result overrides; it requires Claude's child-session marker and session
identity, the configured repository root as its current directory, and an exact current authorized
target before it starts the child. It clears the child environment and restores only the bounded
local execution allowlist described in the capture guide. Its real child exit status determines a
caller-asserted configured-check result. Ordinary direct Bash remains an execution with unknown
verification. The Hub does not provide a trusted recipe registry or independent assurance.
SIGINT and SIGTERM cancel and reap the runner's owned child process group and retain an incomplete
receipt. A hard kill or process crash can leave only the already-durable start; it does not turn
that invocation into a passing result or authorize an automatic rerun. The retained uncertainty
fences the same Work, checkout and recipe across session, generation, source and setup changes.
After verifying that the same local host has rebooted, run the same visible command with
`reconcile-recipe` in place of `run-recipe`. Reconciliation records the original attempt as
incomplete with an unknown result before another run is allowed. A same-boot, foreign-host or
missing host/boot identity remains unresolved; the local runner does not claim general subprocess
containment.

Recipe execution and reconciliation serialize through one checkout-wide local lock for the
configured root, repository and worktree. A concurrent reconcile response reports
`reason: "checkout_in_use"` and `scope: "configured_checkout"`; this broader lock does not mean
the requested Work or recipe already has an attempt. Unresolved attempts remain separately fenced
by Work, checkout and recipe and continue to require current authorization.

The command preserves unrelated hooks. Each installed hook resolves the current authorized Work
and generation for its native session, appends to the private spool, then attempts bounded ordered
delivery through the saved connection. `metadata-only` is the default. `local-conversation` retains
selected visible bodies only in the spool; they are not placed in shared envelopes.
`shared-conversation` also redacts them (best effort) and uploads them to the Hub after it accepts
the event. Change an enabled binding's mode with `change-capture set-mode --root PATH --mode
metadata-only|local-conversation|shared-conversation`, which never touches the spool. `change-capture
conversation --work-id ID [--cursor C | --digest D | --delete D]` lists, prints or deletes shared
bodies (the author and Hub admins only). See
[sharing conversations](../change-capture.md#sharing-conversations-with-the-hub).

Execution success is not a passing verification. An exact verification rule has the form
`Bash=verification:complete:process_exit_code:<lowercase-sha256>`. The local client reports
`passed` or `failed` only for an exact configured recipe match. The Hub retains that as a
caller-asserted claim; it does not have an independent recipe registry. Setup and recipe digests are
bounded identities and do not establish complete provenance. Read the shared projection without
changing Hub state with:

```sh
agentscm change-capture show --root /absolute/repository --work-id wrk_example
agentscm change-capture status --root /absolute/repository
agentscm change-capture inspect --spool /absolute/private/spool
agentscm change-capture export --spool /absolute/private/spool --destination /absolute/private/archive
agentscm change-capture reconcile-recipe --id RECIPE_ID
agentscm change-capture pending --spool /absolute/private/spool
agentscm change-capture deferred --spool /absolute/private/spool
agentscm change-capture quarantine --spool /absolute/private/spool
agentscm change-capture prune-quarantine --spool /absolute/private/spool OPAQUE_NAME
agentscm change-capture compact --spool /absolute/private/spool --limit 100
agentscm change-capture settings-recovery --root /absolute/repository
agentscm change-capture unconfigure --root /absolute/repository
agentscm change-capture forget --root /absolute/repository
```

`inspect` is a bounded read-only view. It does not recover, clean, change modes, acquire mutation
authority, or make a copied spool resumable. `export` copies bounded local evidence to a new private
archive and likewise grants no authority; it refuses the export before creating the destination if
any source file exceeds the 64 MiB exact-hash limit. A checkout's first successful configuration retains the
exact v4 spool identity and namespace as its safety lineage. Later configuration may update rules,
recipes, setup and equal-or-larger limits only when that same identity is present, including after
an identity-preserving offline path move. Guarded identity checks refuse a replacement inode, legacy
or partial spool, namespace change, or lower limit before that guarded operation mutates capture
state. Live moves, restores, and replacements are unsupported; a change is detected when a guarded
path check observes it, rather than by continuous path containment. `unconfigure` removes the marked
hooks and disables new capture while retaining this lineage; configuring the same checkout against
a different spool is unavailable in this version. `forget` removes only a disabled checkout's
binding from the config so it can be set up again for another repository or Hub; it never deletes
the spool and prints its path. `unconfigure` and `forget` also work on a deleted checkout: pass its
absolute path as it was saved (symlinks in parent directories that still exist are resolved), and
`unconfigure` fails when no binding matches. `forget` also removes the `.git/info/exclude` lines
setup added for that checkout's settings file, unless another binding in the same repository still
uses them. `status` reports active, disabled, or paused state
and embeds the read-only inspection result. A disabled binding can still reconcile an already
retained uncertain recipe attempt when current authorization and the existing reboot proof allow
it; it cannot start a new recipe. At most 128 checkout lineages are retained in this local config;
disabled lineages are not silently evicted to make room.

`reconcile-recipe` looks up the retained attempt by recipe id under the Work that is currently
authorized for the native session. It does not require that recipe to remain in the current
configuration and never executes it. An uncertain attempt retained for a different Work remains
blocked and visible through inspection; the command does not redirect it to the current Work or
invent authorization for the historical Work.

Spool limits are stored with the checkout binding. `pending` excludes an outbox envelope only when
its regular acknowledgement marker contains the exact envelope digest. `compact` removes only
exactly acknowledged envelope bytes and retains bounded replay/conflict facts, provider usage and
completed action receipts. If a concurrent
settings writer or interrupted settings transaction leaves recovery material, `settings-recovery`
lists it. Restore one inspected displaced inode with `--restore TRANSACTION`; remove only unchanged
entries with `--prune-unchanged`. Reconcile an inspected crash plus late-editor write with
`--reconcile-changed TRANSACTION`; the command preserves the active settings and retained late
bytes, and later writes block again until another explicit reconciliation. Late editor writes are
retained and never pruned automatically. `quarantine` lists bounded invalid crash remnants, and
`prune-quarantine` deletes only exact opaque names copied from that listing.

`show` returns the same at-most-48-KiB projection as HTTP and MCP. It includes at most 16 retained
scope entries and 16 retained decision references; exact `scope_omitted` and
`source_refs_omitted` counts identify any trailing entries removed to fit the envelope. When
sessions reported setup it also carries up to 16 `session_setups` (model, effort and permission
mode per session) and `session_setups_omitted`.

`pending` shows shareable queued envelopes. `deferred` shows sanitized hooks that could not resolve
current authorized Work and will never be rewritten into an associated action. `observe` and
`import-claude` are explicit local operations; run their `--help` before supplying source, session
and operation identities. See [local change capture](../change-capture.md) for privacy, source and
qualification limits. Real-host and complete integrated qualification remain pending.

### `attached-worktree` (experimental, Unix)

`attach`, `state`, `capture` and `refresh` retain the Git-authoritative source lifecycle described in the [attached worktree guide](../advanced/attached-git-worktrees.md). `preview` uses the shared bounded selector without consuming an operation ID. `bind-context` joins an existing Authority Change; `compare` checks source/target/context/policy/evidence; `prepare --operation-id ID` retains a fixed typed unlanded commit in an owned mirror. `publish-branch` uses the attachment's endpoint through Mirror's local/HTTPS transport, explicit credentials/trust and an exact expected-OID lease; `recover-branch` observes an uncertain push. `publish-pr` uses the existing authenticated Hub bridge (GitHub PR or GitLab MR; see the [guide](../advanced/attached-git-worktrees.md)), with `--operation-id` naming the branch receipt and `--previous-publication` naming a prior PR association for continuation. `recover-pr --id ATTACHMENT --publication PUBLICATION_ID` only observes. Historical receipts do not imply current qualification. Run `agentscm attached-worktree COMMAND --help` for arguments.

`attached-worktree abandon-prepare --id ID --mirror PATH --operation-id PREPARE_ID` journals abandonment only before candidate construction and while the owned ref still equals the reserved parent. The original ID remains retained. Built/Verified candidates require explicit replay of the original prepare and cannot be published until Mirrored. See [attached worktree recovery and limits](../advanced/attached-git-worktrees.md).

```sh
agentscm --data-dir ./agentscm-data init

agentscm --data-dir ./agentscm-data checkpoint checkpoint.json
agentscm --data-dir ./agentscm-data files REVISION --limit 1000
agentscm --data-dir ./agentscm-data files REVISION --prefix src --contains parser --limit 1000
agentscm --data-dir ./agentscm-data stat REVISION path/to/file
agentscm --data-dir ./agentscm-data read REVISION path/to/file
agentscm --data-dir ./agentscm-data diff LEFT RIGHT --limit 1000
agentscm --data-dir ./agentscm-data history REVISION --limit 100

agentscm --data-dir ./agentscm-data start context.json
agentscm --data-dir ./agentscm-data show REPOSITORY_ID CHANGE_ID
agentscm --data-dir ./agentscm-data context EXPECTED_VERSION context.json
agentscm --data-dir ./agentscm-data review-context CONTEXT_DIGEST

agentscm --data-dir ./agentscm-data approve approval.json
agentscm --data-dir ./agentscm-data revoke APPROVAL_ID
agentscm --data-dir ./agentscm-data policy
agentscm --data-dir ./agentscm-data policy --advance EXPECTED_EPOCH
agentscm --data-dir ./agentscm-data target main
agentscm --data-dir ./agentscm-data integrate integration.json
agentscm --data-dir ./agentscm-data initialize-target initialize.json
agentscm --data-dir ./agentscm-data prepare-landing landing.json
agentscm --data-dir ./agentscm-data events --after 0 --limit 100
```

Use `-` or omit the input filename for JSON on standard input. A checkpoint request contains an optional parent and changes. File content is an array of byte values; `null` deletes a path.

`files` scans one immutable manifest page and returns entry metadata, `scanned`, `next` and `complete`. `--prefix PATH` and the literal, case-sensitive `--contains TEXT` filter that page after scanning it; search text must contain 1 to 1024 bytes. A filtered page can have no entries and still return `next`; save that JSON cursor string in a file and pass `--cursor FILE` until `next` is null. `stat` checks exact manifest membership without reading the file blob: `present` does not prove the blob is locally available, while `nonexistent` means the complete manifest has no such path. Missing source objects, denied/indeterminate reads and limits remain distinct failures with a typed top-level `error_kind` (in the JSON form). `read` retrieves the entry kind and content as `text` (a string, when the bytes are valid UTF-8) or `base64` (standard alphabet, otherwise); exactly one is present. `workflow inspect` diff entries use the same rule per side as `before_text`/`before_base64` and `after_text`/`after_base64`, omitted for an absent side.

```json
{
  "parent": null,
  "changes": [
    {"path": "README.md", "content": [104, 101, 108, 108, 111]}
  ]
}
```

`init` returns the immutable local repository identity. Start a change with a [review context](../advanced/review-context.md), then use the returned digest and generation in approvals. `show` displays the current context; `review-context` reads an exact historical one. Updating a context requires its expected generation and invalidates earlier approvals, even when source bytes are identical.

An approval is a local human administrative claim. It binds the repository/change, current change generation, protected target, exact revision, retained review-context SHA-256 digest, and policy epoch. It is not proof that a model or test ran. Unknown required inputs or capabilities and declared omissions prevent approval.

A context requiring `agentscm.checks.source-paths/v1` also needs a successful controller attempt for its exact subject before approval or integration. This built-in check reads declared source paths and checks their hashes; it does not execute candidate tests or a model. See [checks and evidence](../advanced/checks.md) for the recipe and attempt lifecycle.

`initialize-target` takes the `integrate` request shape with `expected_target` set to `null` and a change binding. It accepts a head with any number of parents, such as an imported snapshot head or the tip of a chain, after qualifying its complete source closure. It consumes the approval and journals an event with `kind` `initialize`. An existing target is rejected; there is no adoption or replacement. `integrate` still accepts only a parentless revision for an absent target.

`prepare-landing` builds the exact revision for landing offline work on a moved or existing target. The request carries a full review context for the work: its revision is `source_tip`, its base is `expected_target`, its target is `target`, and its `change_version` is the Change's current version plus one.

```json
{
  "operation_id": "prepare-1",
  "target": "main",
  "expected_target": {"revision": "TARGET_SHA256", "version": 1},
  "source_tip": "TIP_SHA256",
  "change_version": 2,
  "context": {"...": "full review context"}
}
```

The tip's first-parent chain must reach the expected target; otherwise the command fails with a conflict naming the diverged base, and nothing changes. It performs no three-way merge. It publishes a landing revision with parents `[target, source_tip]` and the tip's tree, qualifies its closure, and in one transaction checks that the target is unchanged, rebinds the context to the landing revision and advances the Change. A target that moved fails the check as a conflict. If the tip is the target or has the same tree, the result is `already_current`, nothing remains to land, and the Change is untouched. Otherwise the result is `prepared` with the landing `revision` and new Change state. It never approves or moves the target: approve the landing revision with the returned change version and context digest, then run `integrate` with `expected_target` set to the base. An approval for the source tip cannot authorize the landing revision. An identical retry returns the stored outcome; reusing an operation ID with different input, or one used by `integrate` or `initialize-target`, is a conflict.

The library also provides [portable signed evidence](../advanced/evidence.md) and
[installed policy assessment](../advanced/policy.md). These bounded interfaces assess
supplied records; authenticated installation, durable current-attempt admission,
retention enforcement and live execution qualification remain required.

```json
{
  "approval_id": "approval-1",
  "target": "main",
  "revision": "REVISION_SHA256",
  "context_digest": "REVIEW_CONTEXT_SHA256",
  "change": {"repository": "REPOSITORY_ID", "id": "change-1"},
  "change_version": 1,
  "policy_epoch": 0
}
```

Integration additionally binds an operation ID, protected target, expected target state, and the same exact subject fields. Against the intact local authority, retrying an identical operation ID returns its durable result; reusing it with different input is a conflict.

```json
{
  "operation_id": "operation-1",
  "target": "main",
  "expected_target": null,
  "revision": "REVISION_SHA256",
  "context_digest": "REVIEW_CONTEXT_SHA256",
  "approval_id": "approval-1",
  "change": {"repository": "REPOSITORY_ID", "id": "change-1"},
  "change_version": 1,
  "policy_epoch": 0
}
```

## Guided setup

`agentscm quickstart` (what `npx agentscm` runs) detects the repository, Docker, a hub and Claude Code, runs collisions, sets up live warnings (a local hub, native on macOS ARM64 or Docker as an explicit fallback; an existing hub; or none), signs in and sets up your coding agents through `agentscm client setup` (token, hooks, MCP entry and skill). Every file it writes is announced first. Quickstart's own files are recorded in `~/.config/agentscm/quickstart.json` and each connected checkout in `connections.json`; `agentscm uninstall` removes exactly those, running `agentscm client remove` for each checkout. Details and limits: [setup guide](../setup-guide.md).

```sh
agentscm quickstart                     # interactive
agentscm quickstart --yes --hub existing --hub-url https://hub.example.com \
  --agent-token-file ~/hub.token --agent claude
agentscm uninstall                      # asks first; --yes, --remove-hub-data
```

Flags: `--yes`, `--init` (also run `init-repo`; with `--yes` it is skipped unless passed), `--tray` (macOS only; open the companion after successful setup; without it, an interactive macOS run that connected agents asks once whether to keep the companion and start it at login), `--local` (same as `--hub local`), `--hosted` (print the hosted guide; connects only with `--hub-url`), `--hub local|existing|skip`, `--hub-url URL`, `--hub-runtime native|docker`, `--hub-port PORT`, `--hub-project NAME`, `--slack-config-file PATH` (see [Slack](../slack.md)), `--provider github|gitlab`, `--agent-token-file PATH` (skip sign-in and minting), `--github-login LOGIN` (required with `--yes --hub local` unless `gh` is signed in), `--agent claude|codex|cursor|copilot|gemini` (repeatable; default Claude Code if the `claude` CLI is on `PATH`), `--no-agents`, `--no-collisions`.

### Status-bar companion and client status

`agentscm tray` opens the optional macOS status-bar companion and returns after its background
process is ready. It reads the same checkout connections as the CLI. **Start at Login** is off by
default and can be changed from the menu. This alpha has no separate app install or updater; the
binary is currently unsigned and unnotarized. Linux and Windows retain the CLI status fallback:

```sh
agentscm client status [--root PATH] [--check] [--json]
```

Without `--check`, status reads local configuration only. `--check` performs bounded authenticated
Hub reads and reports recent participation recorded by the existing native integrations. The tray
also shows local hook and MCP transport outcomes observed while it is running; these observations
stay in memory and expire. Missing or idle activity stays unknown. `--json` emits a versioned
status contract, including separate onboarding and local-activity binding identifiers.

Each connected checkout has **Agent Integrations** actions for Claude Code, Codex, Cursor,
Copilot and Gemini. They reuse the existing owned hooks, MCP and skill installers with the saved
connection and stable installed binary. **Reapply** repeats setup; it does not certify runtime
health or accept the native client's trust prompts. A tray started by macOS can have a minimal
`PATH`, so Claude Code or Codex may be installed but unavailable to the tray. Each agent action has
a **Copy Terminal Setup** fallback. The copied command is executed only when you paste and
run it in Terminal; it runs normal `client setup`, which may contact the Hub, request forge sign-in,
and create or replace a managed coordination token. The native client's own trust prompts still
apply.

A Git checkout where a client-connection hook ran without a connection is listed under **Not
connected** while the tray runs (local only, at most eight, forgotten after an hour). **Connect
with** an agent runs `client setup` for its main checkout against the Hub of the most recent saved
sign-in, without starting a new sign-in; **Copy Terminal Setup** copies `quickstart` instead.

Setup actions show a progress and result window with the client's next steps; they do not install
the coding client itself. **Open Hub** uses the exact Hub's saved human sign-in to mint a 30-minute
read-only token for the selected repository. The token goes to the browser in a URL fragment,
which the UI removes. An existing browser sign-in is preserved. Missing or expired sign-in,
changed checkout bindings and failed Hub requests produce a visible error.

## Usage telemetry

```sh
agentscm telemetry status
agentscm telemetry inspect
agentscm telemetry off
agentscm telemetry on
agentscm telemetry configure --endpoint https://example.com/v1/telemetry/batches --profile permitted-default
agentscm telemetry configure --profile consent-required
agentscm telemetry configure --profile unknown
agentscm telemetry configure --clear-endpoint
```

`status` reports `enabled`, `disabled` or `waiting` and a reason. `inspect` shows the bounded event fields without enabling transmission. `off` persists the opt-out and removes queued records and pseudonymous identifiers; `on` shows the disclosure and records an affirmative preference but still cannot send without an eligible profile and endpoint. `configure` only provisions the recipient and profile: it does not record notice or consent, and changing either binding clears collection state while preserving an explicit `off`. Configuration is per user. The endpoint must be an absolute HTTPS URL ending in `/v1/telemetry/batches`. `--endpoint` and `--clear-endpoint` cannot be combined; either may be combined with `--profile`. See [usage telemetry](../telemetry.md) for precedence, delivery and durability limits, data categories and activation limits.

## Demo

```sh
agentscm demo [--no-open]
```

`demo` simulates three agents against a throwaway local hub, using file names from the Git repository you are in, and prints a link to its status page. It never writes to the repository. Its last line, and the status page banner, name the next step: `agentscm quickstart` (or `npx agentscm`) for the guided setup. `--no-open` does not open a browser.

## Agent wiring

```sh
agentscm init-repo [--agents claude,codex,cursor,copilot,gemini] [--hub-url URL] [--yes]
agentscm hook <claude|codex|cursor|copilot|generic> [--hub-url URL]   # called by agents, not by people
```

`init-repo` writes only the committed agent instruction files for the chosen agents (all five by default) into the Git repository you are in: `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `.cursor/rules/agentscm.mdc` and `.github/copilot-instructions.md`. It writes no MCP config, hooks or token. It needs no data directory and is idempotent. `--hub-url` (default: `AGENTSCM_HUB_URL`, then `hub.json`) fills in the setup command in those files and in the next steps it prints; without one they keep a `<HUB_URL>` placeholder. Each developer then runs `agentscm client setup --agent <agent> --hub <hub>` once per checkout. (`init` is the unrelated command that prepares a data directory.) `hook` takes the token from `AGENTSCM_HUB_TOKEN` or `AGENTSCM_HUB_TOKEN_FILE`, else `hub.json`; the hooks `client setup` writes instead pass `--client-connection` (also accepted for `gemini`), which uses only the checkout's saved connection and cannot be combined with `--hub-url` or `--repo`. Details, the file list and limits: [supported agents](../agents.md).

## Serving

```sh
agentscm --data-dir ./agentscm-data serve [--bind ADDR] [--max-concurrency N] [--collisions-repo PATH] [--timeline-workspace ROOT] [--browser-origin ORIGIN]... [--public]
agentscm --data-dir ./agentscm-data serve --explorer-repo ABS_PATH --explorer-repository ID [--explorer-spool ABS_PATH] [--browser-origin ORIGIN]...
```

`serve` serves collision warnings (`GET /v1/collisions`) for the trusted Git checkout named by `--collisions-repo`, and the experimental read-only native API for the data directory. `--bind` defaults to `127.0.0.1:7331`. A non-loopback `--bind` is refused unless `--public` is passed. The container image passes `--public` itself; publish the port on loopback only, or put authentication in front of it. `--timeline-workspace` serves one native workspace's timeline read-only under `/v1/local`. `--browser-origin` lets that exact origin read `/v1/local` cross-origin, and it requires `AGENTSCM_SERVE_READ_TOKEN`. See [local template and timeline reads](../api.md#local-template-and-timeline-reads).

`--explorer-repo` and `--explorer-repository` enable the [repository explorer](../repository-explorer.md)
for one absolute Git checkout path and logical repository ID. It always requires
`AGENTSCM_SERVE_READ_TOKEN` and a loopback bind, including when `--public` is present.
`--explorer-spool` opts in to private capture from an existing absolute spool directory whose
namespace matches that ID. The explorer is Unix-only, reads immutable commits and performs no
fetch, checkout or execution.

## Trusted-local task workflow

`workflow start` is the integrated local entry for a task that will continue through checkpoint, inspection, checks, approval and landing. The command is hidden from general help because its schema remains experimental.

```sh
agentscm --data-dir ./agentscm-data workflow start \
  --change task-1 --workspace ./work --revision REVISION --issue task-1 \
  --intent "Update the example parser" \
  --criterion "The parser accepts the new form" \
  --scope crates/example \
  --path Cargo.toml --path Cargo.lock --path crates/example \
  --recipe RECIPE_SHA256 --toolchain TOOLCHAIN_SHA256 \
  --environment ENVIRONMENT_SHA256

agentscm --data-dir ./agentscm-data workspace expand ./work expand-1 0 \
  --path crates/shared
agentscm --data-dir ./agentscm-data workflow checkpoint checkpoint-1 1
```

Later steps: `workflow prepare OPERATION_ID` records the clean workspace tip as the source to land; `workflow inspect` prints the bounded diff and an `inspection` token; `workflow approve APPROVAL_ID --inspection TOKEN` approves that exact revision; `workflow integrate OPERATION_ID --inspection TOKEN` lands it. `TOKEN` is the `inspection` field of the `workflow inspect` JSON.

Repeated `--path` values select the ordinary files materialized into the workspace. They are independent from repeated `--scope` values, which declare the review-context boundary. Expansion advances only the workspace generation; `workflow checkpoint` at that new generation advances the workspace base and updates the guided Change context. Omit `--path` for a full workspace, or use a sole `--path .` as an explicit full-tree selection. See [task-scoped source and workspaces](../advanced/task-workspaces.md) for the full flow and boundaries.

## Skill workflows

`skill-workflow` is the experimental local runner for a graph of checks on one immutable native
revision. It is separate from the hidden guided `workflow` command mentioned above. Prepare an
existing native data directory, a packaged `SKILL.md` and a strict JSON definition as described in
[skill workflows](../advanced/skill-workflows.md).

```sh
agentscm --data-dir ./agentscm-data skill-workflow plan workflow.json
agentscm --data-dir ./agentscm-data skill-workflow run local-run-1 workflow.json
agentscm --data-dir ./agentscm-data skill-workflow status local-run-1
agentscm --data-dir ./agentscm-data skill-workflow resume local-run-1 --max-steps 1
agentscm --data-dir ./agentscm-data skill-workflow cancel local-run-1
```

Those commands work as shown for source-only workflows. Command steps require a current Change
and `command/v1` review context plus paired `--docker` and `--docker-socket` flags on `run` and
`resume`. Cancelling a running command needs the backend flags too. Use
`skill-workflow recover RUN_ID STEP_ID --docker PATH --docker-socket SOCKET` to reconcile an
interrupted command without replaying it. See the guide for recipe binding and cleanup limits.

Execution and recovery are local CLI operations. The native API, operator-mapped hub/MCP and
dashboard inspect retained runs. The optional [Claude Code loop](../advanced/claude-code-workflows.md)
adds `skill-workflow configure`, `check`, `claude-stop` and `require`. Configured native targets
require an exact-current finalized graph receipt; a standalone successful run is not an approval.

The [reusable workflow library](../advanced/reusable-workflows.md) loads one strict, named and
versioned manifest at an explicit native path. `inspect` is read-only, `configure` binds the
template to the existing Change and installs the first immutable target requirement, and
`handoff` reads a retained run without advancing it:

```sh
agentscm --data-dir ./agentscm-data skill-workflow library inspect CHANGE \
  --library skills/checks/v1/workflow.json
agentscm --data-dir ./agentscm-data skill-workflow library configure CHANGE \
  --library skills/checks/v1/workflow.json
agentscm --data-dir ./agentscm-data skill-workflow handoff RUN_ID
agentscm --data-dir /absolute/native-data skill-workflow mcp \
  [--allow-configure] [--allow-execution] \
  [--workspace /absolute/native-workspace] \
  [--docker /absolute/docker --docker-socket /absolute/docker.sock] \
  [--claude /absolute/claude]
```

Manifest and package bytes come from the Change's exact native revision. Review the inspected
graph, skill, resources and recipes before configuring it. Separate manifest files are recommended
for separate versions; there is no mutable tag or update command. Explicit configuration refreshes
a valid stale policy epoch. It rejects a stale target base, an existing target requirement and a
source-only graph paired with command requirements before changing the Change. Inspect a stale base
with `show REPOSITORY CHANGE`, `policy` and `target TARGET`, then publish a reviewed next context
with `context EXPECTED_VERSION FILE`; these state checks do not establish source or rebase
correctness. An interruption during the later multi-transaction installation can leave partial
retained state and is reported for inspection. Source-only `check` needs no execution flags;
command and Claude-review steps require their respective operator-selected backends.

Handoff preserves failed, unknown and omitted results and never executes, approves or lands source.
It reports only bindings retained by the definition: command actions carry a historical
Change/context binding that may no longer be current, while a source-only handoff carries source
and run identity without inventing Change attribution. The configured `check` output carries its
current context and receipt.

`skill-workflow mcp` is a local Unix stdio server for any MCP-capable client. Read-only startup
advertises library inspect and workflow handoff. The two explicit allow flags separately advertise
library configure and check. The operator fixes the native data directory, workspace and execution
backends in the server command; requests cannot override them. This is distinct from the remote Hub
MCP, which exposes retained workflow status only. Client identity does not choose the reviewer: a
`claude_code_review` step still requires the fixed `--claude` backend, whichever MCP client made the
call. No native Grok integration or live-client qualification is implied.

Further `skill-workflow` commands support the Stop triggers. `codex-stop` takes the arguments of `check` and `claude-stop` and is a Codex CLI Stop hook with the same pass/block behavior. `remote-codex-stop` takes the arguments of `remote-check` and `remote-stop` for hosted workspaces. `task-stop` and `task-codex-stop CHANGE --endpoint URL --repository OWNER/NAME --profile ID --token-file ABS_PATH --workspace PATH` are read-only Stop hooks for agent-owned tasks: they pass only when the facade `qualification` is current and qualified and the clean workspace base is the qualified revision, and never run, upload, finalize or checkpoint (see [agent workflow tasks](../advanced/agent-workflow-tasks.md#stop-hook-agent-owned-mode)). `install-stop --settings FILE [--apply] [--remove [--workspace PATH]] -- <claude-stop|remote-stop|codex-stop|remote-codex-stop|task-stop|task-codex-stop> ARGS...` previews by default and writes the hook entry into an operator-local Claude Code or Codex hook file only with `--apply`; it replaces only the same workspace's entry, whatever the stop subcommand or change, and `--remove` removes all its entries or only those for `--workspace`. The Stop handlers require `stop_hook_active` and allow at most 8 consecutive automatic continuations per agent, session, workspace and change; a parse error in a Stop subcommand exits 1, a non-blocking error. See the [Stop hook guide](../advanced/claude-code-workflows.md#opt-in-claude-code-stop-hook) and the [hosted variant](../advanced/hosted-laptop-workflows.md#optional-stop-hooks).

## Agent-owned workflow tasks

`agent-task` is the Unix operator/client route for the same authenticated task facade and existing workflow journal described in [agent-owned workflow tasks](../advanced/agent-workflow-tasks.md). It does not create another scheduler and does not invoke the server executor.

Generate an owner-private signing key, then use the reported public identifiers when preparing the strict enrollment JSON. The CLI never accepts private key bytes in arguments and never overwrites an existing key file.

```sh
agentscm agent-task key-generate /secure/collector.key
agentscm agent-task key-info /secure/collector.key
agentscm agent-task command-runtime-info --docker /usr/local/bin/docker \
  --docker-socket /var/run/docker.sock --state-dir /secure/collector-state
agentscm agent-task review-transport-info --claude /usr/local/bin/claude \
  --state-dir /secure/collector-state
agentscm --data-dir /srv/native agent-task enroll enrollment.json --private-key /secure/collector.key
agentscm --data-dir /srv/native agent-task profile-install collector-profile.json
agentscm --data-dir /srv/native agent-task inspect REGISTRATION_ID --profile PROFILE_ID
agentscm --data-dir /srv/native agent-task revoke revocation.json
```

Enrollment JSON is the version-1 `ExecutorEnrollment` object. `key-info` supplies `verification_key_id`, `verification_key_sha256` and `public_key_hex`; set a positive immutable `registration_epoch`, repository, executor/operator IDs, allowed step IDs, `signature_algorithm: "ed25519"` and `issued_at_ms`. Revocation JSON is the version-1 `ExecutorRevocation`; `revoked_at_policy_epoch` must equal the currently reported policy epoch plus one. A profile is the strict version-1 `CollectorProfile` described in the task contract and is immutable after installation. The two transport-info commands supply the otherwise internal runtime/executable/provider digests for its action. The review probe checks the selected executable and existing Claude Code subscription authentication but performs no review/model call.

```json
{
  "version": 1,
  "registration_id": "laptop-tests-1",
  "executor_id": "laptop-tests",
  "operator_id": "workspace-operator",
  "repository": "NATIVE_REPOSITORY_ID",
  "step_ids": ["test"],
  "registration_epoch": 1,
  "verification_key_id": "KEY_INFO_VALUE",
  "verification_key_sha256": "KEY_INFO_VALUE",
  "signature_algorithm": "ed25519",
  "issued_at_ms": 1792368000000
}
```

Every task operation is a strict tagged `AgentTaskRequest` JSON file sent to the shared authenticated HTTP facade. This covers `requirements`, `next`, `instructions`, `reference`, `claim`, `collector_claim`, `heartbeat`, `result`, `status`, `cancel`, `reconcile`, `recovery`, `collector_descriptor` and `collector_finalize`. The server supplies the actor from the bearer token; the JSON cannot select credentials, native paths, executables or providers.

```sh
agentscm agent-task call --endpoint https://hub.example.com --repository acme/widgets \
  --profile laptop-checks --token-file /secure/agent.token request.json
agentscm agent-task upload --endpoint https://hub.example.com --repository acme/widgets \
  --profile laptop-checks --token-file /secure/agent.token upload.json
```

For example, a requirements request contains only the exact workspace subject; subsequent responses supply task IDs/digests used by claim and status requests.

```json
{
  "operation": "requirements",
  "subject": {
    "change_id": "change-1",
    "change_version": 3,
    "context_digest": "CONTEXT_SHA256",
    "revision": "REVISION_SHA256",
    "requirement_digest": "REQUIREMENT_SHA256"
  }
}
```

For trusted collection, save the exact `collector_descriptor` result and use the same locally available authoritative native data directory. A command profile requires the pinned Docker executable and local socket; a review profile requires the pinned Claude Code executable. The profile, not the client, selects recipe, image, model, limits and provider scope.

```sh
agentscm --data-dir /srv/native agent-task collect \
  --endpoint http://127.0.0.1:7331 --repository acme/widgets --profile laptop-checks \
  --token-file /secure/agent.token --descriptor descriptor.json \
  --state-dir /secure/collector-state --private-key /secure/collector.key \
  --transfer-request-id upload-001 --docker /usr/local/bin/docker \
  --docker-socket /var/run/docker.sock
```

Collection rechecks current authenticated authorization before external dispatch and before signing/upload. The signed transfer is durably retained first. If upload fails, do not run `collect` again: retry the exact bytes with `retry-upload` and the identical descriptor, state directory, key and transfer request ID. Uncertain reserved effects remain nonpassing and cannot be rerun through this recovery path.

Task-only profiles do not download native source or authority state. The collector must already have the exact immutable revision, context and installed requirement in `--data-dir`; automatic independent-laptop bootstrap and two-machine collector portability are not implemented. Token/key/state paths must be absolute, outside candidate source and located directly in an owner-private directory; the CLI opens the verified immediate parent and leaf without following a final symlink. Trusted ancestors may include platform aliases such as macOS `/var`, and the whole operator account remains trusted. Loopback IP HTTP is allowed for a local hub; other endpoints require HTTPS. Requests use no proxy, no redirects, finite timeouts and bounded input/output.

## Local validation attestations

`local-validation` (experimental, Unix) records a maintainer's local gate as a signed, Git-bound attestation. `run --recipe FILE --policy FILE --registry FILE --key-file ABS_PATH --issuer-id ID --state-dir ABS_DIR [--repo DIR] [--base-ref main] [--base-rev origin/main] [--expires-in-seconds N]` refuses a dirty tree, runs the recipe commands in order with a cleared environment (an allowlist plus `AGENTSCM_TEST_*`, with `HOME` passed; this only avoids handing tokens over in environment variables, and the commands still run as the signing user and can read that user's files), stops at the first failure, signs the record, retains `SHA256.envelope.json` and `SHA256.record.json` in the 0700 state directory, retains each command's stdout and stderr as `SHA256.cmdN.stdout` and `SHA256.cmdN.stderr` (0600, N is the command position; their digests are the record's), then prints the envelope digest, outcome and those file paths. Ctrl-C (SIGINT) cancels the running command's process group and the run is signed as a failure with that command `cancelled`; a second Ctrl-C exits immediately. Run `git fetch origin main` first, because the merge-base is computed against the local `origin/main`. It exits nonzero for a failed run, which is still signed and retained. Create the key with `agent-task key-generate`. `show --state-dir DIR --attestation SHA256` prints a deterministic PR-comment body whose signed blocks are base64. `verify (--comment-body FILE ... | --envelope FILE --record FILE) --policy FILE --registry FILE --repository ID [--recipe FILE] [--repo DIR] [--head REV] [--base-ref main] [--base-rev origin/main]` reads Git only and prints a one-line JSON verdict. `--recipe` (default `.github/local-validation/recipe.json`, relative to the current directory) is the trusted default-branch recipe: its digest must equal the policy's (else an error verdict) and the record must contain exactly its commands, ids and argv, in order (else `incomplete run`). Repeat `--comment-body` to judge a whole PR thread: among attestations whose verified signature names the observed repository and commit, the one with the greatest signed issue time decides (ties: greatest envelope digest), so comment order and reposting an older attestation do not matter, and bodies that are junk, stale, tampered or signed by an unregistered key are ignored. It exits 0 for success, 1 for failure, 2 for error (such as an uncertain clock, or an unreadable policy, registry, recipe or body file, which prints an `error` verdict) and 3 (pending) when no attestation applies to the head. `post --state-dir DIR --attestation SHA256 --github-repo OWNER/NAME --pr N --token-file ABS_PATH [--api-url URL]` (GitHub) refuses unless the PR head equals the attested commit, adopts an earlier comment by the token's own login carrying the same first-line marker (editing it only if the body differs) or else creates one, and writes `SHA256.posted.json` in the state directory. A different attestation is a new comment. The producer never posts a commit status; a separate verifier does. `reconcile --github-repo OWNER/NAME [--base main] [--config-dir .github/local-validation] [--repo .] [--token-env GH_TOKEN] [--api-url URL] [--target-url URL]` (GitHub, public repositories only) reads `policy.json`, `recipe.json` and `registry.json` from `--config-dir` (relative to the default-branch checkout `--repo`, no `..`), takes the token from the environment variable `--token-env` names, and sets the `agentscm/local-validation` status of every open pull request whose base is `--base` and that has a trusted attestation comment or an existing status. It sets `pending` on every such head first, fetches heads from `origin` as Git data without credentials, re-reads the open pull requests and trusted comments before each write and skips a head that changed (including when the `--config-dir` tree at `origin/<base>` differs from the checkout's `HEAD`: the head stays `pending`, it is not an error, and the remaining heads are left pending too; a failed fetch or read of that tree is an error), and reports a head shared by several pull requests as `pending` (unsupported). An incomplete listing fails closed. A head that cannot be fetched or observed gets nothing more (it stays `pending`) and counts as an error. GitHub keeps at most 1000 statuses per SHA and context; a head reaches that after about 500 runs (two statuses per run), and past it the `pending` write is refused, the head is not re-verified and the earlier verdict remains. It exits 0 when every pull request was handled or left pending for a newer run, 1 on any error or unread input and 2 when the policy's repository differs from `--github-repo`, `--target-url` is invalid, or `--config-dir` is unsafe, unreadable at `HEAD` or has uncommitted or untracked changes (nothing written). It has been tested only against a fake GitHub. A signature shows that a registered key signed these bytes, not that the commands ran as recorded.

## Local workspaces and offline exchange

Keep the initialized repository data directory outside the working directory. Start a Change for `REVISION` before creating its workspace. Workspace checkpoints advance the local workspace base; updating the Change's review context remains an explicit operation.

```sh
agentscm --data-dir ./agentscm-data workspace create ./work CHANGE_ID REVISION
agentscm --data-dir ./agentscm-data workspace create ./work CHANGE_ID REVISION --path src --path Cargo.toml
agentscm --data-dir ./agentscm-data workspace expand ./work EXPAND_OPERATION EXPECTED_GENERATION --path tests
agentscm --data-dir ./agentscm-data workspace path-state ./work path/to/file
agentscm --data-dir ./agentscm-data workspace status ./work
agentscm --data-dir ./agentscm-data workspace preview ./work --limit 1000
agentscm --data-dir ./agentscm-data workspace checkpoint ./work OPERATION_ID 0
agentscm --data-dir ./agentscm-data workspace state ./work

# Sender creates an offer; receiver requests its missing objects.
agentscm --data-dir ./agentscm-data sync offer offer.json REVISION
agentscm --data-dir ./receiver-data init
agentscm --data-dir ./receiver-data sync request offer.json request.json
agentscm --data-dir ./agentscm-data sync bundle offer.json request.json source.bundle
agentscm --data-dir ./receiver-data sync import source.bundle ./transfer-state
```

Offer, request and bundle output paths must be new. Imports preserve source identities and validate complete source/history closure. They do not import approvals, protected targets, policy or review contexts. Transfer state and workspaces can contain private source; keep them outside the product checkout and back them up or remove them according to your own retention needs.

`workspace preview` lists the files a checkpoint would capture and the paths that `.gitignore`, `.agentscmignore`, the private `.agentscm-workspace/exclude` or (with a root `.git`) Git's own exclude files leave out, each with its deciding rule; it never prints contents. Base paths are always captured within the effective scope; out-of-scope base paths remain inherited unchanged. See [source selection](../advanced/workspace.md#source-selection).

Without `--path`, `workspace create` retains full-workspace behavior. Repeated `--path` values select exact base paths and descendants; `--path .` selects the full tree. Each scope construction accepts at most 256 roots and 65,536 total path bytes before canonicalization. `workspace expand` durably adds roots under an operation ID and expected workspace generation. It preserves dirty paths already in scope but refuses newly selected local occupants; move an out-of-scope occupant aside, expand, then reapply its edit. `path-state` distinguishes `materialized`, `missing_local`, `unmaterialized`, `outside_scope_local` and `nonexistent`. Scoped checkpoint overlays only materialized scope on the complete base: a missing tracked in-scope path is deleted, while an unmaterialized base path is preserved. See [task-scoped workspaces](../advanced/task-workspaces.md).

Durable trusted-local [native merge sessions](../advanced/merge.md) retain divergent
conflicts, bounded same-file text merges and generation-checked workspace
application. See [workspaces](../advanced/workspace.md) and [offline exchange](../advanced/sync.md).

## Git snapshot bridge

Capture one explicit, clean Git commit as a single native root revision. This is a one-way snapshot, not Git history import, and it never writes to the Git repository. The receipt path must be new, or byte-identical from an earlier run of the same capture, and outside the checkout, its Git directory and the data directory.

```sh
agentscm --data-dir ./agentscm-data git-snapshot preflight --repository ./checkout --revision COMMIT
agentscm --data-dir ./agentscm-data git-snapshot capture --repository ./checkout --revision COMMIT --receipt ./receipts/snapshot.json
```

`capture` writes `progress: ` lines to standard error when it is a terminal or when `--progress` is given (at most one line per file read chunk and per batch, plus one when the revision is published); standard output stays one JSON value, and on failure the error JSON is the last line of standard error. `preflight` is read-only and never opens the data directory. Unsupported entries (gitlinks, symlinks whose target is not an in-tree relative path, non-portable paths and others), uncommitted tracked changes (including edits hidden by skip-worktree or assume-unchanged) and configured Git filters make a tree ineligible; no partial snapshot is written. See [the Git snapshot guide](../advanced/git-snapshot.md) for the profile, receipt, Git invocation and limits.

## Supported skills (experimental)

`agentscm skill` browses the first-party skills embedded in the binary and adopts them per repository. Adoption is the lock file `agentscm-skills.lock.json` at the repository root; committing it is the review. Evidence an agent records is **self-reported**: the Hub stores what the session said, and every gate satisfied that way is labelled `[self-reported]` in `status` output.

- `skill list [--json]` and `skill show NAME [--version N] [--json]` work offline: summary, when to follow it, prerequisites, what it is not for, parameters with defaults, gates (id, who, enforcement, minimum assurance), an adopted marker from the lock file and, for `show`, the agent procedure (`SKILL.md`) under "Procedure:". The `sha256` is a bundle digest of `skill.json` and `SKILL.md`.
- `skill add NAME [--version N] [--param key=value ...]` pins the embedded manifest (sha256, source `first_party`), validates parameters against it (`key=null` resets one) and prints a before/after of the pin. `skill remove NAME` removes it. Neither commits.
- `skill report-adoption` sends the lock file to the Hub (`PUT /v1/hub/repos/{repo}/skills/adoption`) and prints which adoption source the Hub applies. When the repository has a forge connection the Hub refuses the report (409 `forge_authoritative`): the lock file on the default branch is the only adoption, so merge it there.
- `skill status [--branch B] [--json]` shows the branch's run: gates, state, the next action and whether finishing is blocked.
- `skill evidence --gate G --kind K --summary S [--url U] [--pr N] [--skill NAME] [--detail-json FILE] [--branch B]` records self-reported evidence at `git rev-parse HEAD`. The Hub refuses a review recorded with the same agent token and session as an author, and evidence for a commit older than the run head. A refusal (for example `stale_head`, `not_independent`) is printed with its message and exits non-zero.
- `skill stop --agent claude|codex` is an opt-in Stop hook: nothing blocks unless you install it (see below). It reads the hook JSON on standard input and blocks (`{"decision":"block","reason":...}`) while the branch's run needs the agent (`blocks_finish`), or when `pr-gate` is adopted, the branch is not the default branch, has commits ahead of it and has no run yet. It also blocks, at any gate before the merge, when local `git rev-parse HEAD` differs from the head the run last saw (record tests for the new commit; the review must be redone). It allows the stop with a note when the lock pins `pr-gate` but the Hub has not adopted it yet. It never blocks while waiting on CI, approval or merge, stops blocking after 8 consecutive continuations, and allows the stop with a warning on standard error when there is no lock file, no connection, the Hub is unreachable or the directory is not a Git repository.

Status, evidence, report-adoption and stop use the checkout's `agentscm client connect` connection. `skill-workflow install-stop` only installs its own Stop subcommands, so add the hook by hand, for example in `.claude/settings.json`: `{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"agentscm --data-dir ~/.agentscm skill stop --agent claude"}]}]}}`. Use `--agent codex` in a Codex Stop hook. Without the hook the gates are still recorded and shown but nothing blocks.

## Limits

- Paths use portable ASCII, at most 1,024 bytes per path and 255 bytes per component. A checkpoint accepts at most 4,096 changed paths and 16,384 directory prefixes summed across those paths. This aggregate work limit is checked before publishing objects; it is not a latency guarantee.
- The local SQLite profile trusts its operator and filesystem. The optional `agentscm team` profile adds scoped identity and explicit roles but requires a trusted local PostgreSQL/filesystem and separately configured HTTPS proxy; it provides no hostile-code sandbox.
- `initialize-target` and `prepare-landing` re-qualify the full reachable history of the head or landing revision on every call, so cost grows with history. Qualification fails with a limit error beyond 100,000 reachable objects or 1,000,000 qualification visits, and `prepare-landing` walks at most 100,000 first-parent steps. This is a current limit; incremental qualification from an admitted target is not implemented.
- SQLite is one local writable authority. Starting more containers does not scale or safely replicate it.
- Integrated, qualified candidate execution and automated model review, authenticated network sync, Git history import, continuous Git synchronization, automatic failover, service quotas, retention leases, and garbage collection remain incomplete. Local source bundles, merge sessions and workspace recovery remain bounded primitives. The Git snapshot bridge captures only one committed tree, without history. Executable files and in-tree symlinks are captured as entry kinds under profile `agentscm-git-snapshot/v2`; other permission bits are not preserved.
- Objects remain until the operator removes the data directory. Operators must back up the data directory consistently and provide their own recovery procedure.
- Service bounds are alpha limits, not measured capacity claims or service-level objectives.

The authority upgrades schema v1 through v8 to v9 transactionally on open. The v1 migration retains historical events and exact completed-operation retries, labels their semantics as legacy, and revokes unconsumed opaque approvals. The v3 schema adds controller check attempts and evidence. The v4 schema adds the integration event kind and the landing preparation journal. The v5 schema adds isolated `command/v1` check attempts and records. The v6 schema adds command check cleanup custody. The v7 schema adds immutable target workflow requirements and exact-subject receipts. The v8 schema adds workflow executor enrollments. The v9 schema adds collector profiles. Legacy context hashes never become retained review evidence. Older binaries reject newer schemas; there is no downgrade path. Use an independently verified consistent backup before upgrading valuable data. The trusted-local recovery command requires physical quiescence, old-writer isolation and explicit external-state reconciliation; it does not provide automatic failover.
