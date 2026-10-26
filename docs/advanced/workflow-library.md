# Local workflow and skill library

`agentscm open` exposes a durable workflow library in the Hub page through the loopback front door. Library mutations require the existing same-origin browser grant and a currently selected connected checkout. A standalone `agentscm serve` read token cannot write the library, and the front door's read-only Hub token is never accepted as local write authority.

Skills can be created from Markdown or previewed from a public GitHub repository pinned to a full 40-character commit. GitHub imports request only explicit text paths from `raw.githubusercontent.com`, reject redirects, and enforce file, package, and timeout limits. A conventional `npx skills add owner/repo --skill NAME` string is parsed as a source descriptor and uses the same pinned fetch. AgentSCM never runs `npx`, npm lifecycle code, imported scripts, or imported instructions. Accepting a preview stores immutable bytes, file digests, and provenance.

Workflow drafts use generation compare-and-swap updates. Publishing freezes a version and its skill version IDs. Steps describe skill guidance, builds, deploys, productive actions, or another published workflow. Simulation validates declarations and trigger matching synchronously; it never opens the workflow runtime journal, runs a process or model, admits a change, deploys, or queues work. Repository guidance similarly inspects a bounded list of regular manifest and CI files and returns commands for review without running them.

The confirmed-merge trigger carries an exact target branch and required labels. It becomes execution authority only through the existing operator-local pipeline observer, which re-reads forge facts. A library attachment's **Export setup** operation returns a composed skill tree, an existing `PipelineRecipe`, and a pipeline install manifest template. Before an export is marked `runnable: true`, AgentSCM builds a `SkillBundle` from the exact emitted bytes, enforces the native 64-resource and 1 MiB limits, parses every selected action and prompt, verifies artifact dependencies, and runs the existing recipe structural validator. Action source-file inputs remain blocked until the resulting commit exists and can be checked by the native compiler.

An immutable nested workflow and an empty-input `success` follow-on are flattened into one recipe before installation. Each invocation gets its own generated action declaration, while imported bytes are retained under `skills/<skill-version-id>/`. Prompt paths are resolved inside the action's own skill before being rewritten into that namespace. Artifact inputs can name only an action step in the same workflow definition; the producer ID and prompt path are rewritten together as the workflow is flattened. A nested step can never become required when any enclosing workflow step is optional.

A `success` follow-on depends on every preceding leaf and therefore starts only when every preceding action succeeds, including actions whose `required` flag is false. Composition rejects cycles, duplicate or overlong flattened IDs, more than 16 referenced workflows, more than 32 resulting steps, nonempty workflow inputs, and `failure` or `always` follow-ons. Imported resources are not pruned: an export that exceeds the native 64-resource or 1 MiB limit is blocked with the measured count or byte total. The existing runner receives one ordinary immutable recipe and does not dispatch back into the library. Build steps still need a reviewed command context and recipe, and deploy steps have no deployment executor. Unsupported declarations are returned as `unmet_bindings`, persisted on the attachment, receive no install manifest or install actions, and are never labeled runnable.

To activate a runnable export, write its files at the exact relative paths, review and commit them, fill the operator-owned profile, forge identity, Git executable and checkout fields in the manifest, and replace `source_commit` with that full commit. Then use the existing commands:

```sh
agentscm --data-dir /private/operator/state pipeline install install.json
agentscm --data-dir /private/operator/state pipeline status
agentscm --data-dir /private/operator/state pipeline preview owner/repo RULE_ID event.json
agentscm --data-dir /private/operator/state pipeline set-enabled owner/repo RULE_ID --expected-generation 1 --enabled true
```

The library does not install, enable, dispatch, or claim deployment itself. See [repository pipeline rules](pipeline-rules.md) for profile and forge observer requirements.

## Local API

The front door provides:

- `GET /v1/local/skill-library`
- `POST /v1/local/skill-imports/preview` and `/accept`
- `GET|POST /v1/local/workflow-library`
- `GET|PATCH|DELETE /v1/local/workflow-library/{id}`
- `POST /v1/local/workflow-library/{id}/versions`
- `POST /v1/local/workflow-simulations`
- `POST /v1/local/repository-guidance`
- repository-scoped attachment list, put, and validate routes below `/v1/local/repositories/{repository}/workflow-attachments`

Mutation bodies are strict JSON, creation uses idempotency keys, and mutable records use expected generations. Archive retains immutable referenced bytes; deleting a draft with published versions is refused.
Publication rejects an invalid composition with HTTP 400 and the exact lint diagnostics that must be fixed; it does not replace those diagnostics with a generic server failure.
