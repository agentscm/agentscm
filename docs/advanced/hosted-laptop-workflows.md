# Hosted laptop workflows

> **Experimental (alpha).** This profile connects a laptop workspace to one
> operator-configured AgentSCM hub. It has been exercised against local synthetic
> servers. It has not been qualified between two physical machines or against an
> Internet-facing deployment.

The hosted laptop profile lets an agent edit in a native workspace on one machine
while the hub on another machine owns workflow execution and native authority. The
laptop keeps a source-only native object store and a workspace journal. It does not
receive the server's authority database, approval records, execution programs,
provider credentials or protected-target controls.

The operator opts a repository and execution profile into source transfer. The
configuration pins the hub repository, execution profile and template review
context. The execution profile separately pins the server native repository,
target, installed workflow requirement and any controller executables. Request
bodies select those registered identifiers; they cannot provide a data directory,
program, provider endpoint or trusted authority claim.

## One-time server and laptop setup

First configure the [community workspace](community-workspace.md), its forge sign-in
and an execution profile as described in the
[Claude Code workflow guide](claude-code-workflows.md). The forge repository must
also be connected to the hub through the existing GitHub App or GitLab connection;
an execution-profile entry alone does not make a repository eligible for token
minting. Give each laptop operator workspace membership with `reader` and
`contributor` roles for that repository. Add a `source_profiles` entry to the
operator-controlled execution configuration for the same hub repository and
profile. It names the digest of the existing template review context. Restart the
hub after changing operator configuration.

The relevant part of the execution configuration is:

```json
{
  "version": 1,
  "profiles": [
    {
      "repository_id": "example/project",
      "profile_id": "checks",
      "native_repository": "11111111111111111111111111111111",
      "data_dir": "/srv/agentscm/native-project",
      "target": "main",
      "requirement_digest": "2222222222222222222222222222222222222222222222222222222222222222",
      "docker": "/usr/bin/docker",
      "docker_socket": "/var/run/docker.sock",
      "claude": "/usr/local/bin/claude"
    }
  ],
  "source_profiles": [
    {
      "repository_id": "example/project",
      "profile_id": "checks",
      "template_context_digest": "3333333333333333333333333333333333333333333333333333333333333333"
    }
  ]
}
```

Use the existing execution profile's real paths, native repository and installed
requirement digest. Use the digest of the review context already registered for
that profile and target. It is the same exact context digest returned by the
profile's subject view; do not calculate it from a new laptop-supplied context.
Startup rejects a missing context or a context bound to another native repository
or target.

Sign in with `agentscm hub login`, then create a write-enabled token with
`agentscm hub token create --name NAME --repo OWNER/REPOSITORY`. Existing hub checks
require the connected forge repository and the signed-in user's forge access before
minting it. Do not add `--read-only`: source download, upload and workflow execution
all require the workspace's execute permission. Put only the printed secret in a
private file on the laptop. The token file must be absolute, a regular file reached
without symbolic links, private to its owner, and separate from candidate source.
The client accepts HTTPS endpoints and loopback HTTP endpoints. It does not follow
redirects or use an HTTP proxy.

Choose two new, empty paths on the laptop: one for the source-only data store and
one for the native workspace. Then initialize the workspace:

```sh
agentscm --data-dir /absolute/path/to/laptop-data skill-workflow remote-init \
  --endpoint https://hub.example.test \
  --repository example/project \
  --profile checks \
  --token-file /absolute/path/to/agent.token \
  --workspace /absolute/path/to/work \
  --change laptop-work
```

Initialization downloads a bounded source bundle rooted at the server's current
target, or at the operator-pinned template revision before that target exists. The
client verifies and imports the native objects, then creates the workspace in the
new empty directory. The local store is opened with `store::Engine`; no local
`Authority` is created. Native IDs are preserved for transferred objects, while the
server repository identity remains a separate pinned authority identity.

This is a native source workspace, not an attached Git checkout. Checkpoint scans
include untracked source files unless an exclusion rule leaves them out
(`.gitignore`, a root `.agentscmignore`, the private `.agentscm-workspace/exclude`
and, with a root `.git`, Git's global and `info/exclude` rules). Tracked paths are
always captured. Run `agentscm workspace preview` on the initialized workspace
before the first `remote-check` to see what will be sent. Ignore handling is not a
credential scanner: keep credentials outside the workspace. See
[source selection](workspace.md#source-selection). For local Rust builds, set
`CARGO_TARGET_DIR` to a separate directory. Git capture/export remains an explicit bridge operation.

The laptop records protocol progress below its data directory with owner-only
permissions. Keep that state with the object store and workspace journal. Deleting
it removes the information needed to distinguish a retry from a new operation.

## Check, fix and retry

After editing the workspace, ask the hub to check its exact checkpoint:

```sh
agentscm --data-dir /absolute/path/to/laptop-data skill-workflow remote-check \
  --endpoint https://hub.example.test \
  --repository example/project \
  --profile checks \
  --token-file /absolute/path/to/agent.token \
  --workspace /absolute/path/to/work \
  --change laptop-work
```

The client checkpoints current bytes, refreshes the latest protected target, and
uses the native merge journal before uploading. Clean target changes are applied to
the workspace. A source conflict remains explicit and blocks the check until it is
resolved; remote work is never silently overwritten. The uploaded declaration is
derived from the authenticated actor, registered profile, source repository and
local Change. Source provenance may name a different native object-store identity,
but it cannot replace the server's configured authority identity.

The hub queues the installed workflow against the exact imported revision. Failed
source checks return bounded observations and an action for the agent. Fix the
workspace and run `remote-check` again. A success is reported only after the client
observes a successful receipt for the exact current subject and then confirms that
the local workspace is still clean with the same bytes. Editing during a run, or the
workspace base moving during the run, counts as a change and causes the client to
withhold success.

A successful check does not approve or land a revision. Human review, explicit
approval and explicit native integration remain separate hub actions with their own
roles and exact-subject checks.

## Optional Claude Code Stop hook

The Stop hook is opt-in local operator configuration. Preserve every existing Stop
hook and add the AgentSCM command as another entry; do not replace the entire
`hooks.Stop` array. Use the same arguments as `remote-check`, changing the command to
`remote-stop`. For Claude Code versions that support exec-form hooks, the added
entry has this shape:

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/agentscm",
            "args": [
              "--data-dir", "/absolute/path/to/laptop-data",
              "skill-workflow", "remote-stop",
              "--endpoint", "https://hub.example.test",
              "--repository", "example/project",
              "--profile", "checks",
              "--token-file", "/absolute/path/to/agent.token",
              "--workspace", "/absolute/path/to/work",
              "--change", "laptop-work"
            ],
            "timeout": 240
          }
        ]
      }
    ]
  }
}
```

The hook returns `{}` only for the same exact success that `remote-check` would
accept. Failed, stale, conflicting, indeterminate and unauthorized states return a
bounded blocking reason that Claude can use as fix feedback. The hook cannot grant
approval or perform landing, and omitting or bypassing it does not bypass server
admission.

## Recovery and limits

Operation and transfer IDs are durable and bind their original inputs. An exact
retry returns retained state. Reusing an ID with different input is rejected. If a
reply is lost after the server begins importing a bundle, recovery reports the
transfer as unknown; the client does not replay that upload under an old ID or infer
that it failed. Keep the old store and its journal intact: they retain the operation
identity and evidence for inspection. Do not delete or edit the journal to force a
retry. After the operator has reconciled any native mutation and external execution,
explicit recovery uses `remote-init` with a **new data directory, new empty workspace,
and new Change key**. Copy only the intended source edits from the old workspace,
review them against the new target, and run a fresh check. Never copy the old
workspace metadata, data store, token file or journals into the new workspace.
Conclusive failures before any native authority mutation are `Rejected`, so a
refreshed source request can proceed without treating them as an uncertain write.
Their charged storage costs and terminal replay records remain retained.
The original operation and its storage/execution costs remain retained; recovery
does not certify it or authorize replay of an uncertain external call.

Journals are bound to the authenticated person. Replacing the token with another
person's token is refused; each person needs a separate store/workspace/Change.
A token revoked during the flow stops later reads and writes. Reauthorization
does not turn an earlier unknown, stale or failed operation into success.

Source metadata and large bundle I/O use separate bounded slots. Responses expire
after 60 seconds, releasing buffered data even if the reader stops consuming it.
A slow or unavailable connection can therefore require a retained-state retry.

The current source profile accepts one offered revision root, at most 16,384 native
objects, an offer of at most 1 MiB and a bundle of at most 64 MiB. The server charges
source bundle bytes and retained protocol metadata against a 1 GiB budget per
configured profile. Up to 256 unfinished or unknown transfers may be retained.
Completed transfers compact their request metadata into an exact-request digest
and retain terminal replay evidence; completed rows do not use the unfinished
transfer slots. Compaction does not discard source objects or release charged
bundle costs. Exceeding a bound
fails the operation; there is no shortened successful transfer.

The repository's local acceptance tests use two distinct temporary stores and
workspace directories, the real OAuth actor extraction, hub store, HTTP source
routes, shared execution queue and native authority. They use only built-in source
checks, so Docker and a model provider are not required. These tests cover success,
failure/fix feedback, stale and repository-scoped requests, token revocation,
durable retry state and the absence of automatic approval or landing. They do not
qualify TLS termination, an Internet-facing reverse proxy, process crashes at every
durability boundary, large retained datasets, two physical laptops, or a real
provider-backed workflow.
