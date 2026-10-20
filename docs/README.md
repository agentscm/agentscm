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
- [Asynchronous agent stand-up](standup.md): durable work claims, checkpoint handovers and bounded catch-up
- [Supported coding agents](agents.md): Claude Code, Codex, Cursor, Copilot and others; `agentscm init-repo` and `agentscm hook`
- [Multi-user coordination hub](hub.md), including [what the hub stores](hub.md#data-the-hub-stores)
- [Deployment](deployment.md)

## Reference

- [HTTP API](api.md)
- [Native CLI reference](reference/cli.md)
- [Specification (draft)](spec/agentic-scm.md) and [architecture](architecture.md)
- [Roadmap](roadmap.md)

## Advanced and experimental

The immutable ledger, Git bridges, isolated command checks and
[source-bound skill workflows](advanced/skill-workflows.md): see the [advanced index](advanced/README.md).
The [Claude Code workflow loop](advanced/claude-code-workflows.md) adds an opt-in local trigger,
isolated tests, bounded model review and exact-revision native requirements. The opt-in
[Community workspace pilot](advanced/community-workspace.md) adds persistent membership,
settings, a shared execution queue and explicit approval/native landing controls. The legacy
token dashboard and MCP inspect retained runs.

## For contributors and maintainers

- [Contributing](../CONTRIBUTING.md), [security policy](../SECURITY.md) and [code of conduct](../CODE_OF_CONDUCT.md)
- [Contributor testing](contributor-testing.md): the nextest gate, focused iteration, coverage comparison and reproducible timing
- [Local source install helper](setup.md)
- [Releasing](releasing.md)
