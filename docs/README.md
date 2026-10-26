# Documentation

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

## Start here

- [README](../README.md): what AgentSCM is, a local demo and the quickstart (a team hub takes about fifteen minutes to set up)
- [Install options](install.md): npx, a release binary, `install.sh` or the container
- [Two agents, one warning](two-agent-demo.md): run a hub and see an agent warned about another's edit
- [Concepts in plain English](concepts.md): the glossary

## Guides

- [Agent source selection](advanced/agent-source-workflow.md) — inspect before checkout, expand dependencies, and report measured working-file savings in native or Git tasks.

- [Guided setup: `agentscm quickstart`](setup-guide.md)
- [Join an existing team Hub](team-member-onboarding.md): connect a developer's local agent without administering the server
- [Usage telemetry: data, activation and controls](telemetry.md)
- [Early collision warnings](collisions.md)
- [Autopsy: how often your own sessions collided](autopsy.md)
- [Work intents](intents.md) and the Claude Code hook
- [Local workspace snapshot timeline](workspace-timeline.md): capture and inspect bounded native-workspace observations, or fork an earlier observation into another Change
- [Asynchronous agent stand-up](standup.md): durable work claims, checkpoint handovers and bounded catch-up
- [Local change capture](change-capture.md): configure-once Claude hooks, source-bound action receipts and concise transcript-free handoff
- [Open the Hub from this computer](connect-this-computer.md): `agentscm open` shows your files, saved conversations and your team's work in one page
- [Repository explorer](repository-explorer.md): browse committed Git files, history and diffs alongside private local conversation and source-bound evidence
- [Work insights](work-insights.md): bounded timing, coordination, usage reports and stored publication observations
- [Supported coding agents](agents.md): Claude Code, Codex, Cursor, Copilot and others; `agentscm init-repo` and `agentscm hook`
- [Slack adapter](slack.md) and [capability map](slack-capabilities.md): optional direct messages for coordination moments that need a person, with handoff buttons.
- [Multi-user coordination hub](hub.md), including [what the hub stores](hub.md#data-the-hub-stores)
- [Deployment](deployment.md)
- [Hosted hub setup (agent-guided, unsupported in the alpha)](hosted.md)

## Reference

- [HTTP API](api.md)
- [Native CLI reference](reference/cli.md)
- [Specification (draft)](spec/agentic-scm.md) and [architecture](architecture.md)
- [Roadmap](roadmap.md)
- [Design: supported skills](design/supported-skills.md) (`pr-gate` v1 is built and experimental; the rest is proposed)

## Advanced and experimental

The immutable ledger, Git bridges, isolated command checks and
[source-bound skill workflows](advanced/skill-workflows.md): see the [advanced index](advanced/README.md).
The [reusable workflow library](advanced/reusable-workflows.md) binds one explicit, versioned
native manifest to existing Changes without adding a mutable catalog or remote executor.
Experimental [productive workflow actions](advanced/productive-workflow-actions.md) compose
bounded prompt-generated text and deterministic artifact commands in that same graph,
separately from verification and native admission. [Operator-local repository rules](advanced/pipeline-rules.md)
can enqueue those workflows after a provider-confirmed merge.
The [local workflow and skill library](advanced/workflow-library.md) adds durable authoring,
bounded pinned imports, side-effect-free simulation and an explicit export bridge to those rules.
The [Claude Code workflow loop](advanced/claude-code-workflows.md) adds an opt-in local trigger,
isolated tests, bounded model review and exact-revision native requirements. The opt-in
[Community workspace pilot](advanced/community-workspace.md) adds persistent membership,
settings, a shared execution queue and explicit approval/native landing controls. The legacy
token dashboard and MCP inspect retained runs.

## For contributors and maintainers

- [Contributing](../CONTRIBUTING.md), [security policy](../SECURITY.md) and [code of conduct](../CODE_OF_CONDUCT.md)
- [Contributor testing](contributor-testing.md): the nextest gate, focused iteration, coverage comparison and reproducible timing
- [Releasing](releasing.md)
