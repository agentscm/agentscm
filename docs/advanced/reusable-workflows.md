# Reusable skill workflow templates

> **Experimental (alpha).** This is a trusted-local, single-writer workflow library for an
> existing native AgentSCM repository. Its CLI and schema may change without migration.

A reusable workflow is one strict JSON manifest stored with a packaged `SKILL.md` in immutable
native source. The manifest supplies a neutral, named graph. Loading it for a Change binds that
graph to the Change's exact repository, revision, policy epoch and review context. The source file
is the version: AgentSCM has no mutable catalog, tag named `latest`, install/update service or
remote template registry.

This is useful when several Changes should run the same shape of tests and review while retaining
separate source and context identities. Keeping each version in a separate file is recommended,
for example:

```text
skills/checks/v1/SKILL.md
skills/checks/v1/runner.json
skills/checks/v1/workflow.json
skills/checks/v2/SKILL.md
skills/checks/v2/runner.json
skills/checks/v2/workflow.json
```

Adding `v2/workflow.json` does not move, replace or reinterpret the retained digest of
`v1/workflow.json`. An installed native target requirement is also immutable. Updating a target
from v1 to v2 is not supported; use a target without an existing requirement when configuring a
new version.

## Manifest

One manifest contains exactly one template. Unknown fields are rejected in the manifest, steps and
actions. The file is limited to 256 KiB, `workflow_version` is a positive integer and the rendered
name `NAME:vVERSION` must fit the existing workflow-name bound.

```json
{
  "version": 1,
  "name": "verify",
  "workflow_version": 1,
  "skill": {
    "entrypoint": "skills/checks/v1/SKILL.md",
    "resources": ["skills/checks/v1/runner.json"]
  },
  "steps": [
    {
      "id": "tests",
      "needs": [],
      "required": true,
      "action": {
        "kind": "command",
        "runner": "runner.json"
      }
    },
    {
      "id": "review",
      "needs": ["tests"],
      "required": true,
      "action": {
        "kind": "claude_code_review",
        "paths": ["src/example.rs"],
        "model": "sonnet"
      }
    }
  ]
}
```

The manifest must be a regular file within the selected `SKILL.md` package directory. The
entrypoint, explicitly declared resources and manifest are all read from the Change's exact native
revision; host files cannot replace them. The manifest path is added to the immutable skill package
automatically. A root-level `SKILL.md` makes the package directory the repository root, so its
bounded, explicit manifest and resource paths may span that repository. It does not implicitly
include every repository file. Relative `runner` paths resolve within the package.

Templates support the existing `source_paths` and `require_capability` action shapes as well as
`command` and `claude_code_review`. Graph, dependency, path, resource and source bounds are the
same as ordinary [source-bound skill workflows](skill-workflows.md). Every command step in one
template must use the same recipe digest because one ReviewContext has one command binding. A
template that mixes command recipes is rejected before the Change or target requirement changes.

The template never supplies a repository, revision, Change ID, context digest, policy epoch or
approval. AgentSCM takes those values from the existing Change. The `model` value identifies the
requested review adapter configuration; the operator still chooses and provides the local model
runtime, credentials and container runtime used by `check`. Loading a manifest does not download a
model, image or toolchain.

## Inspect, configure and check

Inspect a template against an existing Change without installing or running it:

```sh
agentscm --data-dir ./agentscm-data skill-workflow library inspect CHANGE \
  --library skills/checks/v1/workflow.json
```

The result includes the exact template reference and digest, the parsed template, the concrete
definition bound to the current Change, and `"admission":"not_assessed"`. Inspection is
read-only: it does not execute a step, revise the Change, install a requirement, approve source or
move a target. Before permanent configuration, the operator must review that exact reference and
digest, the rendered graph, `SKILL.md`, every declared resource and every command recipe. Template
names and prose do not establish that those source-controlled instructions are suitable for the
Change.

On Unix, an operator can bind the template's command recipe to the existing Change context and
install the first immutable requirement for that context's native target:

```sh
agentscm --data-dir ./agentscm-data skill-workflow library configure CHANGE \
  --library skills/checks/v1/workflow.json
```

Configuration validates the complete package, graph and every command recipe before its first
authority change. It is an explicit operator action and refreshes an otherwise valid Change from a
stale policy epoch to the current global epoch while publishing the configured context. A target
base mismatch, mixed command recipes, a concurrent context change or an existing target
requirement is rejected before that first change. A source-only template is also rejected when the
existing context still requires command capability or evidence; configuration does not silently
remove those requirements. Repeating an identical configuration is refused once the target has a
requirement.

When the Change base is stale, inspect the current state before preparing a reviewed replacement
context:

```sh
agentscm --data-dir ./agentscm-data show REPOSITORY_ID CHANGE_ID
agentscm --data-dir ./agentscm-data policy
agentscm --data-dir ./agentscm-data target TARGET
agentscm --data-dir ./agentscm-data context EXPECTED_VERSION reviewed-context.json
```

The replacement context must use the next Change version, current policy epoch and intended current
target base. Those facts do not prove that the candidate source is correctly rebased or otherwise
suitable; the operator must establish that relationship before publishing the file. A context that
no longer needs command execution must likewise be reviewed and updated through normal context
authority before configuring a source-only graph.

Installation uses established separate authority transactions; a storage failure or interruption
after the first published Change version can leave partial retained state, which the error reports
for inspection. A source-only template needs no command binding but still installs the same
immutable target requirement.

Run the configured graph through the existing exact-current loop:

```sh
agentscm --data-dir ./agentscm-data skill-workflow check CHANGE
```

A source-only graph needs no execution backend. Supply `--docker` with `--docker-socket` when the
selected graph contains a command step, and `--claude` when it contains a Claude review step; a
graph uses only the backends its declared actions need. Command steps execute only the packaged
strict `CommandRecipe`; template prose is never converted into a shell command. Claude review uses
the existing bounded adapter. See [skill workflows](skill-workflows.md) and
[isolated command execution](execution.md) for runtime, cancellation and isolation limits.

## Local MCP front door

On Unix, an operator can expose the same library operations to the current MCP-capable agent as a
local newline-delimited JSON-RPC stdio server. The MCP client launches this command and keeps the
process open; running it in an ordinary terminal does not itself inspect or execute a workflow:

```sh
agentscm --data-dir /absolute/path/to/native-data skill-workflow mcp
```

The default server advertises only the read-only `agentscm_workflow_library_inspect` and
`agentscm_workflow_handoff` tools. Mutation and execution require separate startup grants:

```sh
agentscm --data-dir /absolute/path/to/native-data skill-workflow mcp \
  --allow-configure --allow-execution \
  --workspace /absolute/path/to/native-workspace \
  --docker /absolute/path/to/docker \
  --docker-socket /absolute/path/to/docker.sock \
  --claude /absolute/path/to/claude
```

`--allow-configure` advertises `agentscm_workflow_library_configure`;
`--allow-execution` advertises `agentscm_workflow_check`. The data directory, optional native
workspace and optional backends are fixed when the operator starts the server. Tool arguments can
select only a Change, a native manifest path or a retained run ID; they cannot select host paths,
executables, credentials or sockets. Omit every backend the selected graphs do not need. Prefer a
separate server configuration for the current agent and repository so its fixed authority and
runtime choices are easy to review. Sharing the tools between clients on the same trusted local
account does not add isolation or multi-writer semantics.

This stdio surface is client-neutral. Claude Code, Codex, Cursor, Copilot, Gemini, Grok or another
MCP client can call it when the operator configures that client to launch the command. That is MCP
protocol compatibility, not a native AgentSCM adapter or live qualification for every client;
AgentSCM has no native Grok setup. It is also separate from the remote Coordination Hub MCP, whose
workflow tool remains read-only retained-run status.

The orchestrating client does not select or become the workflow reviewer. A manifest containing
`claude_code_review` still uses the optional fixed Claude executable and the existing bounded
Claude review adapter, even when another client called `agentscm_workflow_check`. The caller's
brand or model assertions do not become independent review, evidence, approval or admission.
Successful `check` records the same exact-current receipt as the CLI; native authority still
performs its normal checks.

## Read-only handoff

Inspect a retained run as a handoff without advancing it:

```sh
agentscm --data-dir ./agentscm-data skill-workflow handoff RUN_ID
```

The handoff retains the exact workflow source and run identity, recorded step results, the binding
scope actually stored by that definition and the next operator action. A command action retains its
Change version and context digest. This is a historical binding, not a claim that the context is
still current; inspect the named Change with `show` before continuing the exact run. A source-only
run has no stored Change attribution; use the configured `check` result for its current context and
receipt. Failed, unknown and omitted work stays failed, unknown or omitted. The command does not
execute or retry steps, checkpoint source, modify the Change, grant approval or claim admission;
its output retains `"admission":"not_assessed"`.

## Read over HTTP

`agentscm serve` returns the same inspect and handoff JSON from
`GET /v1/local/skill-workflow-library?change=CHANGE&library=PATH` and
`GET /v1/local/skill-workflows/RUN_ID/handoff`, so a local status page can show them. Configure,
check and the other mutations are not available over HTTP. See
[local template and timeline reads](../api.md#local-template-and-timeline-reads).

## Supported skills: manifest version 2 and the lock file

> **Experimental.** Interfaces and gate names may change.

A supported skill is a first-party manifest embedded in the `agentscm` binary under
`skills/supported/NAME/vN/skill.json`. Manifest `version: 2` keeps the version 1 fields and adds a
one-line `summary`, `applicability` (`triggers`, `prerequisites`, `not_for`), `parameters`
(`string`, `bool` or `integer`, each with a default and an optional `enum`), `subjects`, `roles`
and `gates`. A gate names the role that records it (`by`), the evidence kinds it needs,
`min_assurance` (`self_reported` < `observed` < `receipt`) and how it is enforced (`stop`,
`forge_status`, `native`, `advise` or `signal`). Evidence parameters may refer to a manifest
parameter, such as `label_param`. Unknown fields, duplicate gate ids and files over 256 KiB are
refused.

Every gate also carries a required `plain` string (1 to 200 bytes): one plain-English line saying
what happens at that gate. `agentscm skill show`, the Hub Skills library and the MCP
`agentscm_skill_list` tool print a "How it works" walkthrough generated from the manifest alone:
one line per gate, in order, with the `plain` text, who does it (the author agent, a different
session, or a maintainer), the evidence it needs, the minimum assurance and whether the step blocks
or only warns. Only the Stop hook and native approval block today; the forge merge check is planned
and the walkthrough says so. Parameters, such as the CI label, change the wording.

A repository adopts skills by committing `agentscm-skills.lock.json`: `version: 1` and up to 32
pins of `name`, `workflow_version`, `sha256` of the exact manifest bytes, `source`
(`{"kind":"first_party"}`; other kinds are reserved) and optional `params` overrides. A pin whose
digest matches no embedded manifest is `unknown_version`, a reserved source is
`unsupported_source`, and params the manifest does not declare are `invalid_params`; none of these
resolves to another version. `agentscm init-repo` adds a short "Supported skills" section to the
agent instruction files when this lock file exists.

Evidence recorded by agents is **self-reported**. The hub labels it so wherever it is shown, and
every version 1 gate accepts it. It is not proof that tests ran or that a review happened.

## Trust boundary and limits

The local native data directory, its operator and configured runtimes remain trusted. This feature
does not add a multi-writer catalog, remote worker scheduler, hostile-code sandbox or arbitrary
script loader. It does not install dependencies, fetch missing container images or select a model
provider account. Native workflow requirements protect the native landing path only; they do not
replace Git hooks, forge branch protection, CI requirements or review policy. Back up native
workflow and authority state separately from Git exports.
