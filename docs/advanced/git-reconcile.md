---
title: "Explicit local Git/native reconciliation"
---

# Explicit local Git/native reconciliation

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

`git-reconcile` repeatedly compares a selected clean Git commit and the actual
native protected target. Git remains authoritative. Each observed result is
historical, even when its saved state is `synchronized`; reading status does not
inspect Git, the filesystem working tree or the current native target.

The first profile requires complete supported trees with no exclusions. Dirty
tracked files, executable files, symlinks, submodules and other unsupported
snapshot features block observation. Untracked files remain outside the captured
tree and are never deleted. See [the snapshot profile](git-snapshot.md).

Initialize against an existing native target whose complete file tree already
matches the selected clean Git commit:

```sh
agentscm --data-dir /private/native git-reconcile init init.json
agentscm --data-dir /private/native git-reconcile observe observe.json
agentscm --data-dir /private/native git-reconcile status
agentscm --data-dir /private/native git-reconcile lookup observe-1
```

`init.json` contains `git` (absolute executable), `repository` (Git checkout),
`target`, `git_commit` (full lowercase commit ID) and `native_target`
(`revision`, `version`). Keep the Git checkout, common Git directory and native
data separate and non-nested. A new journal is required; interrupted initialization
is retained for inspection and never adopted automatically.

`observe.json` contains `operation_id`, `expected_generation`, `git_commit` and
`native_target`. The native state is checked against the authority rather than
accepted from the request. A successful command returns
`{"ok":true,"reconciliation":{...}}`. The result carries its generation, the last
verified baseline pair, the observed Git/native pair and any captured candidate.

* `synchronized`: the complete native file trees matched during this observation.
  This explicitly advances the journal baseline only.
* `git_changed`: the Git side changed; `candidate_revision` is a native file-set
  candidate parented on the verified native baseline, awaiting explicit review.
* `native_changed`: native work needs explicit outbound handling. If a prior Git
  candidate is already in the actual native ancestry, `candidate_incorporated`
  records that fact rather than treating it as a fresh incoming conflict.
* `conflict`: both sides changed without convergence or known incorporation. The
  baseline and both tips remain available for native merge inspection/resolution.
* `unknown`: the observation failed, or an operation is pending after interruption.
  Missing results are never evidence that external work did not happen.

The command creates no Change, approval or integration event, and never moves a
native target. Use the native merge/workspace and explicit review/landing flow.
Once the accepted source and native trees converge, another explicit observation
can verify and record the new baseline. Git history is not imported or inferred;
the native candidate parent records the file-set baseline only. Commit messages
are never used to infer incorporation or approval.

## Retry and local handoff

The journal `git-reconcile.sqlite` has application ID `0x41534752`, schema 1,
DELETE journaling and FULL synchronization. A fixed operation is saved before
capture or export. Identical completed retries return their historical result;
different input for the same ID is rejected. Pending operations block new ones.
A pending exit retains the mirror owner file's Unix device/inode and a private
128-bit random instance marker in its `binding` column. After validating the owned
mirror, the reconciler creates `agentscm-reconcile-instance` with exclusive creation
and syncs the file and parent directory before reserving work. Identity is read
under the mirror's recovery lease. A pending retry requires the existing exact
marker and never creates a missing one; a fresh replacement cannot inherit a
pending export through ordinary inode reuse. Missing, malformed or changed markers
require explicit reconciliation. Preserve the marker with mirror backups and keep
both instances when restoring or relocating; a restored owner inode may differ.
Copying identity metadata is not proof of exclusive writers or source authority.
This is local continuity checking, not writer or host fencing.
A failed immutable capture is retained as a completed unknown observation, so a
new explicit observation may follow without claiming the earlier one succeeded.
Lookup reports `missing`, `pending` or `complete`; completed results may themselves
be unknown. Operations stop at 10,000 rows. Preserve the journal and source objects.

```sh
agentscm --data-dir /private/native git-reconcile prepare-exit exit.json
```

The request contains the observation fields plus `destination` (the exact canonical
absolute path of an existing owned local mirror) and `export` (the fixed
[Git export request](git-export.md)). Path aliases and a changed canonical
destination are refused on new and pending attempts; a retry cannot redirect a
fixed handoff into another mirror.
It requires the exact last synchronized pair and fresh checks of the source and
actual native target. The integration event must match that target. It retains
pending state before composing the existing byte-verified export and expected-ref
CAS, then rechecks source/native state before recording `exit_prepared`.
An uncertain export remains pending and blocks alternatives; inspect both
journals and retry the identical request. Never reset refs or delete journals to
make a stale operation succeed.

`exit_prepared` is a historical local mirror handoff receipt, containing the
native event/target, Git commit/tree and destination. The native workflow owns the
separate explicit exit decision and outstanding workspace/conflict checks. This
command does not disconnect, delete workspaces, fence editors, overwrite the
source checkout, push, create PRs/MRs, or change source authority.

A native landing, local mirror export, remote branch publication and accepted
forge merge are distinct outcomes. This local profile does not establish remote
GitHub/GitLab synchronization or live provider qualification. A connection must
observe the actual accepted forge result before claiming accepted-branch sync.

Read-only status and lookup open only an existing current journal without schema
creation or migration. Mutation binds the original canonical data/source paths;
a relocated restored journal is historical evidence and requires explicit
external-state reconciliation. Cooperating handles retain the native data access
lease through journal close. Backup support additionally requires the recovery
module's schema and typed reference validation; SQLite integrity alone is not
proof of a valid pending handoff. Raw tools/editors still require physical
quiescence. Bounds and synthetic tests do not establish crash/power-loss or live
forge qualification.
