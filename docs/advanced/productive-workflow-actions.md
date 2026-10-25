# Productive workflow actions

> Experimental, trusted-local Unix profile. These actions produce bounded text
> artifacts. They do not independently verify content, approve source, publish to a
> site or merge a pull request.

A workflow can now compose productive prompts and deterministic artifact commands
in the existing source-bound graph. The runtime retains each step separately and
passes explicitly named artifacts to its dependants. Changing the selected prompt
resources and configuration can change the deliverable without adding a
use-case-specific engine action.

This extends [skill workflows](skill-workflows.md). Existing source checks,
container command checks and structured reviews retain their original contracts.
A productive action is not a review merely because its prompt asks for criticism.
Any graph containing a productive action is refused as a native admission
requirement, including when the action is optional.

## Declare the action

Add an `agent_action` node to a workflow definition or reusable template:

```json
{
  "id": "draft",
  "needs": ["explain"],
  "required": true,
  "action": { "kind": "agent_action", "resource": "draft.json" }
}
```

`draft.json` is an explicitly selected immutable resource in the skill package:

```json
{
  "version": 1,
  "inputs": {
    "explanation": {
      "kind": "artifact",
      "step": "explain",
      "name": "explanation"
    }
  },
  "output": {
    "name": "draft",
    "media_type": "text/markdown",
    "max_bytes": 16384
  },
  "action": {
    "kind": "prompt",
    "prompt": "draft.md",
    "required_tools": []
  }
}
```

Select both JSON and Markdown resources in `skill.resources`. Paths listed in `skill.resources` are native source paths. Action resource and
prompt references are relative to the package entrypoint's directory. Compilation validates dependencies
and declared producer outputs; undeclared resources and invalid artifact bindings
are refused. Source inputs name exact native source paths. Parameter inputs carry
literal values, not executable expressions. Event inputs, when supplied by the
repository-rule controller, are pinned before dispatch and cannot be replaced
under the same run identity.

A deterministic action uses the same input/output contract with:

```json
{ "kind": "command", "profile": "package-json" }
```

The command profile is chosen from an operator-installed allowlist. The process
receives a JSON object containing `prompt` and named `inputs` on standard input;
standard output is its declared artifact. Neither prompt text nor artifact text
is interpolated into command arguments or a shell command by AgentSCM.

## Choose an operator profile

Keep the profile outside candidate source. It chooses the runtime, model, allowed
command executables and arguments, timeout and output ceiling. Repository files
cannot supply those settings. For example, replace the executable placeholder
with the absolute path of an installed, supported Claude Code binary:

```json
{
  "version": 1,
  "id": "local-text-artifacts",
  "provider": "claude_code",
  "claude_executable": "/absolute/operator/installed/claude",
  "model": "sonnet",
  "commands": {
    "package-json": { "executable": "/bin/cat", "args": [] }
  },
  "timeout_ms": 120000,
  "max_output_bytes": 65536
}
```

The `package-json` example retains the input envelope as a JSON artifact. A real
operator-installed formatter may implement a different deterministic transform.
The model and command profiles are pinned for the retained run. A profile change
requires a new run; the controller does not silently select a different provider,
account, executable or execution location.

The initial prompt adapter uses the existing Claude subscription preflight with
restricted, no-tool structured generation in a private working directory. It does
not load candidate commands as tools. Nonempty `required_tools`, unsupported
providers or unavailable runtime settings block rather than fall back. A client
connected through MCP is not automatically an executable agent adapter.

Commands run with direct arguments, a cleared environment and a fresh working
directory. **This is a trusted-operator command profile, not an OS sandbox.** An
operator-selected executable can exercise that account's filesystem and network
privileges. Install only commands trusted for the approved computation; these
settings do not establish a general hostile-code isolation or secret-broker
boundary. Use the existing isolated command collector for its supported check
profile.

## Run and inspect

Initialize or use an existing native store and pin a source revision as described
in [skill workflows](skill-workflows.md). Git source can be captured through the
existing [Git snapshot bridge](git-snapshot.md); capture does not move the Git
branch or transfer merge authority.

The example `workflow.json` files are reusable templates, not directly runnable
source-bound definitions. Before the commands below, bind a template to the exact
native repository, revision and current policy epoch. For example, replace the
three uppercase identity arguments with values from your native store:

```sh
python3 - examples/productive-workflows/release-notes/v1/workflow.json REPOSITORY_ID NATIVE_REVISION POLICY_EPOCH workflow.json <<'PYTHON'
import json, sys
with open(sys.argv[1]) as source:
    template = json.load(source)
definition = {
    "version": template["workflow_version"],
    "name": template["name"],
    "source": {"repository": sys.argv[2], "revision": sys.argv[3],
               "policy_epoch": int(sys.argv[4])},
    "skill": template["skill"], "steps": template["steps"]
}
with open(sys.argv[5], "x") as output:
    json.dump(definition, output, indent=2)
PYTHON
```

This binds existing source; it does not import the template or its resources into
the native store. Capture those files first. The output filename must be unused;
the conversion refuses to overwrite an existing definition.

```sh
agentscm --data-dir /path/to/store skill-workflow plan workflow.json
agentscm --data-dir /path/to/store skill-workflow run release-001 workflow.json \
  --action-profile /path/to/operator/profile.json
agentscm --data-dir /path/to/store skill-workflow status release-001
agentscm --data-dir /path/to/store skill-workflow artifact release-001 draft draft
```

The artifact command returns the named retained UTF-8 output as JSON, after
checking its binding and content identity. Status and Hub projections contain
metadata rather than artifact bodies: producer run and step, exact revision,
media type, size, digest, execution outcome and observed model when available.
Treat generated Markdown/JSON as untrusted data, not executable HTML or instructions
to perform external actions.

Each action currently declares one nonempty artifact, at most 256 KiB, with one of
`text/plain`, `text/markdown` or `application/json`. A JSON declaration validates
JSON syntax; it does not establish a domain-specific acceptance test. The profile
may impose a smaller ceiling. Binary video, browser recording and arbitrary tool
execution are unsupported by this initial adapter. A storyboard or release-note
draft is a text artifact, not a produced video.

## Failure and recovery

The journal reserves an action before starting its process and retains the output
before reporting successful completion. A successful process without a valid
named artifact cannot satisfy a downstream binding. Missing or malformed output,
nonzero exit, timeout, cancellation and output limits remain visible non-success
outcomes. Required dependencies prevent downstream execution on missing output.

A second controller cannot claim an action while it is running. If the operator
has established that its controller was interrupted, fence the retained attempt:

```sh
agentscm --data-dir /path/to/store skill-workflow recover-action release-001 draft
```

Recovery records an uncertain outcome and prevents a late process result from
overwriting it. It does not restart the process or certify that an operator
command had no external effects. Artifact retention follows the local workflow
journal; this initial implementation does not provide artifact garbage collection.

A retained interrupted attempt is not automatically executed again. The controller
preserves uncertainty rather than assuming that the process did nothing. Inspect
the attempt and any operator command effects before intentionally starting new
work. Existing native command recovery remains specific to its container custody;
it is not a generic replay command for productive actions.

Artifact validation, model assertions, independent verification, human approval
and publication are distinct. These productive steps implement creation and
retention; external publication still needs a separately authorized, exact-subject
operation through the applicable existing source/publication flow. A local success
is not a successful PR/MR, accepted merge, deployment or website update.

## Qualification boundaries

Synthetic tests exercise separately tracked prompt actions, named handoff and a
deterministic action using two different recipes, plus invalid inputs, output
failure and recovery boundaries. Fixture adapter envelopes do not qualify a real
model or a native editor integration. Report actual runtime qualification
separately. Existing GitHub/GitLab publication and transport limits continue to
apply; this action contract does not qualify live forge behavior or native authority
cutover.

Operator profiles now bind canonical executable identities, including file identity
and SHA-256, for the selected Claude binary and allowed command programs. Programs
must be regular executables controlled by the operator or root, outside Git
checkouts and native authority data. Execution rechecks the selected identity
before preflight, before dispatch and after return. Replacing or changing a program
changes the profile identity; retained runs cannot silently continue with it.
Pin an immutable, operator-managed runtime installation when installing a rule.
An updater that replaces a binary or repoints a symlink changes the binding. The
identity includes canonical path, device, inode, owner, mode and modification
time as well as bytes; copying identical bytes to a different host does not
preserve it. Rule updates apply only to future accepted work. Already accepted
work needs its original runtime identity; this version has no operation to adopt
a replacement runtime for it. Preserve that installation until retained work is
resolved, and plan runtime changes before enabling unattended rules.

An executable may itself be an interpreter script or platform shim. These checks
bind that file, not the entire interpreter, dynamic-library or delegated-tool
installation. Install and maintain those dependencies as trusted operator state.
These checks trust the local operator account and operating system; they are not
hostile-host attestation or an OS sandbox.

The productive CLI forwards SIGINT and SIGTERM into cancellation while it waits for
an action, terminates the owned child process group and retains a non-success
outcome. Repository pickup and watch use the same cancellation path; an idle watch
also stops on those signals. A forced controller kill, machine crash or unresolved
external effect still requires inspection and explicit recovery rather than replay.

Cancellation is cooperative. Startup integrity hashing, local database waits and
controller probes can finish before the token is observed; each Claude
version/auth probe has a ten-second bound. Repeated signals do not force an
immediate exit during cleanup. A forced process kill can leave an unknown attempt
and requires inspection; it is not a safe retry mechanism.
