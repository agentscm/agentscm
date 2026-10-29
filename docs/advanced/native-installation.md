---
title: "Native installation helper (`agentscm setup`)"
---

# Native installation helper (`agentscm setup`)

> **Contributors only.** `agentscm setup` prepares an explicitly owned local native installation
> from a source build. It is hidden from `agentscm --help`. To set AgentSCM up for yourself or your
> team, use [`agentscm quickstart`](../setup-guide.md). The helper fails with
> `unsupported_platform` on anything but macOS.

This source module prepares a new, explicitly owned installation using the
running native binary. It calls the existing native packager, `Engine`, SQLite
`Authority`, and optional isolated `Workspace` operations. It uses no model,
shell, account, provider, network download, system installer or running service.
The source registers `agentscm setup` with explicit `begin`, `resume`, `doctor`
and `self-check` operations. It prepares local native data and a bundle of the
running executable; service and provider setup remain separate.

`src/setup.rs` is a binary leaf, alongside `distribution.rs`. Its typed entry
points are `begin`, `resume`, `doctor`, and optional `self_check`. `setup_cli.rs`
maps typed CLI arguments onto those exact operations; `main.rs` registers the
binary modules and command. Leaf fixtures import the same two binary leaf
sources; CLI fixtures invoke the actual built binary. No library or dependency
registration is included.

```sh
agentscm setup begin /absolute/new-installation
agentscm setup resume /absolute/new-installation --installation-id LOWERCASE_HEX_ID
agentscm setup doctor /absolute/new-installation --installation-id LOWERCASE_HEX_ID
agentscm setup self-check /absolute/new-installation --installation-id LOWERCASE_HEX_ID
```

The ID is the exact 32-character value returned by `begin`. An ordinary global
`--data-dir` is unused by setup; the installation root is always explicit.
Unknown arguments, relative roots, non-UTF-8 roots and noncanonical IDs fail
argument parsing. Underlying operations repeat filesystem/ownership validation.

## Ownership and layout

Supply an absolute destination whose parent already exists, belongs to the
operator and is not writable by other users. Fresh setup refuses every existing
destination, including an empty unclaimed directory. Symlink components,
relative or non-UTF-8 paths, parent traversal, ancestors containing an installation receipt,
native authority, workspace metadata or `.git` are refused. Setup does not
adopt, upgrade or clean an existing installation or user repository. The Git
non-overlap rule includes ordinary checkouts, linked worktrees and bare
repositories: every ancestor is also checked for the bounded `HEAD` file,
`objects` directory and `config` file marker combination. No Git discovery
command or candidate repository code is run.

The new installation root is private (0700), and ordinary administrative
receipts and the setup lock are private files (0600):

```text
INSTALLATION/
  .agentscm-installation.json
  .agentscm-setup.lock
  issue-recovery.json           ordinary external recovery configuration, not authority
  data/                         existing Engine and Authority state
  bundle/                       existing native-bundle/v1 output
  self-check/                   optional separate synthetic repository/workspace
```

The bounded JSON receipt lives outside native DATA and adds no native database,
schema, approval or authority record. It binds a generated installation ID,
canonical root and physical directory identity, running executable digest,
package version, platform/architecture, progress and the actual native
repository/database identity. It is an operator progress record, not a signed
credential or independent authority. A trusted local operator who can modify
the binary, receipt and native files can defeat these checks; this is not a
hostile-filesystem sandbox. Parent directories must remain under that operator's
control throughout an operation.

`begin` claims the new root and returns its exact `OwnedRequest` before native
work. The CLI emits that root and ID in its public JSON result. Retain this
result before the separate `resume` call; a later failure leaves the caller's
typed ownership value usable for retry or doctor without parsing private receipt
JSON. There is no combined `fresh` helper or command that can discard that value
on a post-claim failure. No replacement identity is silently generated.

Successful operations print one JSON value to stdout. `ok: true` means the
operation returned a report, including reports with blocked fields; it does not
mean installation or service readiness. Setup failures exit 1, print no stdout,
and emit a redacted JSON error object with `operation`, `kind`, `message` and an
optional typed `blocker` on stderr. Argument errors retain the ordinary CLI JSON
error convention and exit 2. Underlying native/filesystem error sources are not
serialized into the setup error object.

## Durable steps and resume

The sequence is claimed → initializing native → native initialized → writing
issue recovery configuration → configuration written → packaging native →
installed. Receipt writes use a bounded create-new temporary file,
file sync, rename and directory sync. A leftover temporary file is retained and
blocks automatic retry. The installation lock serializes these setup operations;
native APIs retain their own access locks and invariants.

Native initialization calls the real `Engine::open` and `Authority::open` in a
new DATA directory and records the actual returned repository identity. It does
not insert source samples, Changes, approvals or protected targets into that
repository. After recording native identity, resume uses the existing current
read-only authority opener to validate the physical directory/database and
repository ID; it never migrates or repairs that authority through setup.
SQLite's read-only opener may use existing WAL/SHM sidecars.

If interruption leaves DATA present before its native identity was durably
recorded, resume returns `InterruptedNativeInitialization`. It refuses to adopt
the unbound state or initialize another repository over it. The same rule
applies to a bundle present during `PackagingNative`: the partial or uncertain
output is retained and `InterruptedPackaging` blocks automatic replacement.
An interruption before creating the relevant output can resume the pending
step. A configuration file present during its pending write is also retained
and blocked, even if its bytes happen to match the expected profile; setup does
not adopt an unconfirmed or foreign output. Interrupted self-check directories
also remain intact and blocked.

An installed resume checks the existing native identity and bounded bundle
inventory and requires the exact external configuration described below. It
returns observations without rewriting the receipt or configuration. Changed
executables, versions, physical roots, native identities or bundle bytes refuse
reuse. Upgrades, relocation, rollback, automatic partial-directory repair and
uninstall are unsupported; this foundation never deletes those directories.

The native packager copies the current executable and its compiled materials.
The retained manifest digest and exactly five immediate entries (manifest plus
four fixed members) are checked on resume/doctor. Extra files, directories,
symlinks, byte/length changes and execute-bit mismatches are refused. The CLI
member must remain declared and actually executable; materials and the manifest
must remain non-executable. The copied CLI's bounded digest must also equal the
receipt's earlier executable pin, both before persisting `Installed` and during
inspection. This catches replacement between the independent executable opens
without modifying the shared native packager. A mismatch leaves the bundle and
`PackagingNative` receipt intact and blocked. This uses its existing v1
inventory format; it does not authenticate a publisher, qualify a release or
execute the copied binary. Under a test harness, the current executable is that
harness, so those fixtures do not establish CLI artifact identity or release
installation success.

## External issue recovery configuration

Successful owned setup durably writes the fixed
`INSTALLATION/issue-recovery.json`, outside DATA, through the existing
`IssueWorkflowRecoveryConfig` DTO. Its version is 1; `native_repository` is the
actual initialized native repository ID, and `installation_id` is the independent
ownership ID returned by `begin`. Neither comes from an issue journal. The file
is bounded to 16 KiB, serialized as compact canonical DTO bytes, created only
with create-new semantics and mode 0600, then file/directory-synced before the
receipt records completion. Configuration production never opens or creates an
issue journal, service or provider.

Begin output includes the intended fixed path. Resume and doctor reports expose
`issue_recovery_config_path` and a separate `issue_recovery_config` check. Current
inspection requires a private regular singly linked file, strict DTO parsing,
both expected identifiers and exact canonical bytes. Missing, corrupt,
extra-field, symlink, public-mode, noncanonical or mismatched configuration is
blocked. Resume and self-check refuse it; doctor can still report the real
native/bundle observations alongside the blocked configuration field. They do
not rewrite, adopt or regenerate it from receipt or journal contents.

Preserve this exact external file and the ownership receipt alongside DATA
backups. DATA-only backups do not include it automatically; include it as an
explicit ordinary attachment when using a supported backup operation, while
retaining the existing physical-quiescence and recovery requirements. Presence
of this file is not authorization, writer fencing, service health or recovery
qualification, and does not mean that an issue journal exists. Default recovery
still rejects every `issue-workflow` namespace entry. To use the explicit
trusted-local journal recovery profile, pass this external file through
`--issue-config PATH` on each backup, verify, restore and activate command.
Its schema, identity and physical quiescence requirements are defined in
`src/local_recovery.rs`. Setup itself does not invoke or activate recovery.

## Doctor and optional self-check

Doctor requires the exact owned root and installation ID. It reports current
native repository observations and bundle inventory separately. Missing,
interrupted, stale or foreign state is never promoted to ready. It does not
open ports, contact `/health`, poll an existing viewer, log in, inspect unrelated
credentials or infer authentication from configuration. Service health remains
`not_run`, operator authentication and provider/model connections remain
`not_configured`, and Team/platform service provisioning remains `unsupported`.
Disk, memory and other host-resource prerequisites remain `not_run` as well.
There is no overall `Ready` or `Qualified` result.

`self_check` is explicit and optional. It creates another fresh repository under
the installation's separate `self-check/data`, records an explicitly incomplete
synthetic Change there, checkpoints a small sample, materializes its own
workspace and checks actual immutable bytes and workspace status. It adds no
approval, integration event or synthetic checkpoint to installation DATA or any
user repository. Its receipt records that exercise; doctor labels it historical
`recorded` and does not claim that its current workspace was re-probed. Repeating
or repairing a retained self-check is unsupported in this first slice.

Team setup, forge OAuth, model execution, service/container installation,
authenticated operator bootstrap and service start/health/lifecycle require
their actual owning modules. Selecting those unsupported profiles creates no
installation.

## Source fixtures

`tests/integration/setup.rs` covers new/exact resume, claimed interruption, retained partial
native and bundle state, interrupted receipt writes, wrong ownership ID, changed
executable identity, foreign native repository, changed bundle members, path
refusal (including direct and deeper bare-Git ancestors), busy/oversized/public receipts, non-UTF-8 paths and isolated self-check
separation. Deterministic negatives cover a self-consistent bundle with the wrong
executable pin, chmod changes and extra files/directories/symlinks. The pin
fixture exercises validator rejection, without relying on updater race timing.
`tests/integration/setup_cli.rs` exercises registered argument forms, public ownership-ID
recovery after a post-claim failure, native packaging of the actual CLI, blocked
doctor fields, typed failures and isolated self-checks. All fixtures own
disposable synthetic roots. They never target an existing user repository or
installation. Run the `setup` test binary and the `setup_cli::` tests of the `integration` test binary locally to exercise
these fixtures.

Recovery configuration fixtures independently compare both IDs with the actual
native authority and public ownership value, then change each field separately.
They cover strict parser roundtrip, missing/corrupt/extra-field/noncanonical
bytes, private mode, symlink and interrupted/unexpected writes without adoption
or overwrite. The CLI fixture validates the reported real path and configuration
through the actual DTO parser and checks a blocked mismatch.
