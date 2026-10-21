# Explicit local Git remote exchange

> **Experimental (alpha).** This transport supports one operator-trusted local bare
> SHA-1 remote. HTTPS, SSH, authentication, GitHub/GitLab code synchronization and
> PR/MR creation are outside this local profile. See [authenticated HTTPS/SSH
> transport](git-remote-network.md) for explicit network access.

`git-remote` adds explicit fetch and recoverable ref publication to an existing
[owned export mirror](git-export.md). It uses that mirror's Git objects, export
mappings, access guard and exclusive export lock. There is no additional native
source store. Git and the native ledger keep their separate authority: fetching
creates no native Change, approval or target; publishing requires an intact
verified native export, and changes only the explicitly bound Git branch.

Create a bare remote separately and initialize an owned export mirror using
`git-export init`. Bind the mirror once, then inspect the branch:

```sh
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git bind \
  --remote /absolute/path/to/remote.git --branch main
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git observe
```

Git 2.45 or newer is required, using the same version check as snapshot capture.
`--git` selects a trusted absolute executable. The remote must already exist and
be bare, non-shallow, use SHA-1 and be separate from the mirror. URLs, relative
remote paths, worktree remotes and unsupported branch names are rejected. The
binding records the canonical remote path and its directory's device/inode;
replacing the directory requires a new explicit binding in a new mirror.
A branch observation must agree between direct local reads and the server's
advertisement. A hidden branch or a detected race fails closed instead of
becoming an absent-branch observation. This is
an operator-owned filesystem profile, with no defense against a malicious local
owner, raw concurrent Git writes or arbitrary filesystem mutation.

After exporting event 1, publish it to an absent remote branch by omitting
`--expected-oid`. For an existing branch, supply its full lowercase SHA-1 commit
from an explicit observation:

```sh
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git publish outbound-1 --event 1
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git publish outbound-2 \
  --event 2 --expected-oid FULL_PREVIOUS_COMMIT
```

An existing branch must be an ancestor of the exported commit. Publication checks ancestry and uses
an exact ref lease. It cannot rewind a divergent branch. No remote branch deletion, merge,
rebase or automatic retry is provided. The owned mirror keeps its isolated client configuration. The server discards
client-added Git command overrides and retains the operator's `HOME`,
`XDG_CONFIG_HOME`, `PATH`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_GLOBAL` and
`GIT_CONFIG_NOSYSTEM` only when they were actually set in the invoking process.
Unset configuration sources keep Git's normal system/global lookup. Relative
policy paths and PATH entries are anchored to the invoking directory before Git
changes directories; config symlinks retain their include-file semantics.
Unrepresentable UTF-8/path settings are rejected before transport effects.
Original `GIT_CONFIG`, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT` and numbered
`GIT_CONFIG_KEY_*`/`GIT_CONFIG_VALUE_*` overrides are unsupported and refused
before binding or operation reservation. Use ordinary config files for remote
policy. The remote's repository, global and system hooks/receive policy remain
in force. Remote hooks/config are trusted local programs; a rejecting hook
blocks publication and is never bypassed.

Fetch a commit already observed on the bound branch:

```sh
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git fetch inbound-1 \
  --expected-oid FULL_OBSERVED_COMMIT
```

Fetch checks the branch against that exact commit and retains its complete Git
closure at `refs/agentscm-fetch/inbound-1`, without changing the mirror's export
ref. The retained commit is historical if the remote moves during or after the
fetch. A normal clean checkout of that exact commit can use the existing
[snapshot capture](git-snapshot.md) or [reconciliation](git-reconcile.md) flow;
their fidelity and admission limits still apply. A fetch receipt does not import
Git history into the native ledger.

## Durable intent and recovery

`agentscm-remote.sqlite` uses application ID `0x41534754`, schema 1, DELETE
journaling and FULL synchronization. Preserve it with the entire mirror and
export journal in a quiesced backup. Each operation records its immutable
request and exact commit before transport effects. The journal retains at most
1,000 operations, each at most 16 KiB. Each Git process has the exporter's
60-second deadline and bounded output. It starts in a dedicated Unix process
group; timeout or output-budget termination kills that group, including
cooperating receive/hook descendants, before returning uncertainty. Fetched closure validation permits at
most 200,001 reachable objects. There is no transport disk quota or whole-command
latency guarantee; this profile is for trusted local repositories.

The result's state must be inspected even when the CLI returns `ok: true`, which
means the operation receipt was obtained:

- `pending`: durable intent; an interrupted write may already have happened.
- `fetched`: objects and the operation ref were retained locally.
- `published`: Git acknowledged the push and a subsequent read saw the exact commit.
- `unknown`: a command failed or the post-write read could not confirm it.
- `rejected`: a leased push finished with exactly one `!` line for the target ref
  marked `[remote rejected]` or `[rejected]` (UTF-8, no CR; the exit code is not
  trusted), and a re-read still showed the expected preimage. The record carries
  a closed `rejection` kind: `hook_declined`, `protected_branch`, `stale_info`,
  `non_fast_forward`, `fetch_first` or `other`. The kind is derived from Git and
  server summary text and no server text or stderr is stored, so
  `protected_branch` appears only when the server's reason says "protected
  branch". It is terminal, had no effect and does not fence new operations. If
  the re-read shows the desired commit the record is `observed_desired`; if
  the ref is unadvertised on a network remote, `observed_unadvertised` (there,
  `rejected` is recorded only for a server-side `[remote rejected]`; a
  client-side `[rejected]` implies the ref was advertised and is
  `observed_unadvertised`); any other
  state, `diverged`; a failed re-read, `unknown`. Lost or partial replies,
  timeouts, output over budget (stderr shares the 16 KiB budget, so a very chatty
  hook yields `unknown`) and transport failures also stay `unknown`.
- `observed_desired`, `observed_expected`, `diverged`: explicit recovery saw the
  desired commit, the expected preimage, or a different branch state.

An identical operation ID returns its saved receipt and never replays a push.
Changed input under that ID is rejected. Pending/unknown operations (not `rejected`) fence new
work until explicit recovery:

```sh
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git lookup outbound-2
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git recover outbound-2
```

Recovery only reads the remote; it never repeats a publication. Observing the
desired commit does not prove which actor wrote it. Observing the preimage does
not prove a prior write never happened and was later undone. If recovery cannot
read the remote, uncertainty remains. Inspect the recovered state and choose a
new operation ID and current expected ref for any new attempt. Saved receipts
remain historical; `observe` is a separate point-in-time read.

GitHub's existing experimental [hub publisher](../hub.md#publishing-a-revision)
creates a new branch and PR through server-held credentials. GitLab's existing
hub connection observes MRs/branches and can post coordination comments/statuses;
GitLab native-export branch/MR publication is not implemented. Neither route nor
this local transport establishes native admission or a live authority cutover.
