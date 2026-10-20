# Native CLI reference

> **Experimental (alpha).** The native ledger commands below are not part of the main path, may change without migration, and have had limited testing. See the [advanced index](../advanced/README.md). The everyday commands (`quickstart`, `uninstall`, `demo`, `init-repo`, `hook`, `collisions`, `autopsy`, `intent`, `hub`) are described in the [README](../../README.md), the [setup guide](../setup-guide.md), [collisions](../collisions.md), [autopsy](../autopsy.md), [intents](../intents.md) and [hub](../hub.md).

`workflow`, `recovery`, `team` and `package-native` are internal to the alpha and hidden from `agentscm --help`. The trusted-local task-workspace entry through `workflow` is documented below; the other hidden commands are not documented here. Other native commands are marked "[experimental]" in `agentscm --help`.

## Commands

Every command accepts `--data-dir`; its default is `.agentscm`. The global `--no-telemetry` flag disables product usage telemetry for that invocation. Most commands print one JSON value on standard output on success, and one JSON value (`{"ok":false,"error":...}`) on standard error with a nonzero exit on failure. The exceptions are:

- `--help` and `--version` print plain text.
- `serve`, `hub serve` and `team serve` print nothing on standard output; they log to standard error.
- `hub` subcommands are text-oriented: `login`, `logout`, `token revoke`, `connections outbound` and `connections add-repo` report on standard error; `token create` prints only the secret on standard output; `token list`, `connections list` and `connections refresh` print text lines; `github setup` prints only the setup URL.
- `hub publish` prints a multi-line JSON value, and on an incomplete or rejected publication also exits nonzero.
- `intent` prints plain text (or nothing) by default and a hook JSON object with `--hook`; it deliberately exits 0 on coordination errors so that it never blocks an agent. `intent watch` runs until stopped.
- `setup` reports its errors as a structured JSON object.
- `hook` reads an agent's hook JSON on standard input and prints that agent's reply; it always exits 0, even on usage errors, and prints a valid "no information" reply when the hub cannot be consulted. `init-repo` prints plain text, shows a diff and asks before writing; without a terminal it needs `--yes`. See [supported agents](../agents.md).
- `quickstart` and `uninstall` are guided and print plain text. Without a terminal they need `--yes`; otherwise they refuse and list their flags. `uninstall` exits nonzero when it could not undo something, and keeps its record of it.

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

`files` scans one immutable manifest page and returns entry metadata, `scanned`, `next` and `complete`. `--prefix PATH` and the literal, case-sensitive `--contains TEXT` filter that page after scanning it; search text must contain 1 to 1024 bytes. A filtered page can have no entries and still return `next`; save that JSON cursor string in a file and pass `--cursor FILE` until `next` is null. `stat` checks exact manifest membership without reading the file blob: `present` does not prove the blob is locally available, while `nonexistent` means the complete manifest has no such path. Missing source objects, denied/indeterminate reads and limits remain distinct failures with a typed top-level `error_kind`. `read` retrieves entry kind and bytes.

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

`agentscm quickstart` (what `npx agentscm` runs) detects the repository, Docker, a hub and Claude Code, runs collisions, sets up live warnings (a local hub with Docker, an existing hub, or none), signs in, mints an agent token for the repository and merges the Claude Code hooks. Every file it writes is announced first and recorded in `~/.config/agentscm/quickstart.json`; `agentscm uninstall` removes exactly those. Details and limits: [setup guide](../setup-guide.md).

```sh
agentscm quickstart                     # interactive
agentscm quickstart --yes --hub existing --hub-url https://hub.example.com \
  --agent-token-file ~/.config/agentscm/tokens/acme-widgets.token --hook project
agentscm uninstall                      # asks first; --yes, --remove-hub-data
```

Flags: `--yes`, `--init` (also run `init-repo`; with `--yes` it is skipped unless passed), `--hub local|existing|skip`, `--hub-url URL`, `--provider github|gitlab`, `--agent-token-file PATH` (skip sign-in and minting), `--github-login LOGIN` (required with `--yes --hub local` unless `gh` is signed in), `--agent-name NAME`, `--ttl 30d`, `--hook user|project|none`, `--no-collisions`.

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
agentscm init-repo [--agents claude,codex,cursor,copilot,gemini] [--hub-url URL] [--repo OWNER/NAME] [--yes]
agentscm hook <claude|codex|cursor|copilot|generic> [--hub-url URL]   # called by agents, not by people
```

`init-repo` writes the committed MCP, hook and instruction files for the chosen agents (all five by default) into the Git repository you are in. It needs no data directory, is idempotent and never writes a token. (`init` is the unrelated command that prepares a data directory.) `--repo` names the repository as `owner/name` (default: from the `origin` remote). `hook` takes the token from `AGENTSCM_HUB_TOKEN` or `AGENTSCM_HUB_TOKEN_FILE`, else `hub.json`. Details, the file list and limits: [supported agents](../agents.md).

## Serving

```sh
agentscm --data-dir ./agentscm-data serve [--bind ADDR] [--max-concurrency N] [--collisions-repo PATH] [--public]
```

`serve` serves collision warnings (`GET /v1/collisions`) for the trusted Git checkout named by `--collisions-repo`, and the experimental read-only native API for the data directory. `--bind` defaults to `127.0.0.1:7331`. A non-loopback `--bind` is refused unless `--public` is passed. The container image passes `--public` itself; publish the port on loopback only, or put authentication in front of it.

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

## Limits

- Paths use portable ASCII, at most 1,024 bytes per path and 255 bytes per component. A checkpoint accepts at most 4,096 changed paths and 16,384 directory prefixes summed across those paths. This aggregate work limit is checked before publishing objects; it is not a latency guarantee.
- The local SQLite profile trusts its operator and filesystem. The optional `agentscm team` profile adds scoped identity and explicit roles but requires a trusted local PostgreSQL/filesystem and separately configured HTTPS proxy; it provides no hostile-code sandbox.
- `initialize-target` and `prepare-landing` re-qualify the full reachable history of the head or landing revision on every call, so cost grows with history. Qualification fails with a limit error beyond 100,000 reachable objects or 1,000,000 qualification visits, and `prepare-landing` walks at most 100,000 first-parent steps. This is a current limit; incremental qualification from an admitted target is not implemented.
- SQLite is one local writable authority. Starting more containers does not scale or safely replicate it.
- Integrated, qualified candidate execution and automated model review, authenticated network sync, Git history import, continuous Git synchronization, automatic failover, service quotas, retention leases, and garbage collection remain incomplete. Local source bundles, merge sessions and workspace recovery remain bounded primitives. The Git snapshot bridge captures only one committed tree, without history. Executable files and in-tree symlinks are captured as entry kinds under profile `agentscm-git-snapshot/v2`; other permission bits are not preserved.
- Objects remain until the operator removes the data directory. Operators must back up the data directory consistently and provide their own recovery procedure.
- Service bounds are alpha limits, not measured capacity claims or service-level objectives.

The authority upgrades schema v1 through v6 to v7 transactionally on open. The v1 migration retains historical events and exact completed-operation retries, labels their semantics as legacy, and revokes unconsumed opaque approvals. The v3 schema adds controller check attempts and evidence. The v4 schema adds the integration event kind and the landing preparation journal. The v5 schema adds isolated `command/v1` check attempts and records. The v6 schema adds command check cleanup custody. The v7 schema adds immutable target workflow requirements and exact-subject receipts. Legacy context hashes never become retained review evidence. Older binaries reject newer schemas; there is no downgrade path. Use an independently verified consistent backup before upgrading valuable data. The trusted-local recovery command requires physical quiescence, old-writer isolation and explicit external-state reconciliation; it does not provide automatic failover.
