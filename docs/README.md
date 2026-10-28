---
title: "Documentation"
---

# Documentation

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See
> the [project README](../README.md) for product status
> and the [changelog](../CHANGELOG.md) for known limits.

## Start here

- [Try the scripted demo](scripted-demo.md): no account or Docker, no repository changes.
- [Install and connect agents](install.md): run a local Hub or join one that already runs; review platform limits first.
- [Guided setup](setup-guide.md): what quickstart detects, changes and leaves for you to confirm.
- [Concepts](concepts.md): the Hub, intents, hooks and native workflows in plain English.

## Coordinate work

- [Supported agents](agents.md): client support levels, setup and hook behavior.
- [Collision warnings](collisions.md) and [work intents](intents.md): announce work and see where agents overlap.
- [Join an existing Hub](team-member-onboarding.md) or read the [Hub operator guide](hub.md).
- [Two-container Hub walkthrough](two-agent-demo.md): Docker, GitHub App and a repository you can connect are required.
- [Deployment](deployment.md): local and single-server operating choices. [Hosted setup](hosted.md) is an unsupported alpha path.

## Native workflow alpha

- [Architecture and trust boundaries](architecture.md)
- [Claude Code workflow loop](advanced/claude-code-workflows.md)
- [Community workspace pilot](advanced/community-workspace.md)
- [Usage telemetry: data and controls](telemetry.md)
- [Roadmap and proposed work](roadmap.md)

These native workflow features are opt-in experiments for trusted teams. They do not provide a
production service, a hostile-code security boundary or a qualified scale claim.

## Reference and help

- [CLI reference](reference/cli.md) and [HTTP API](api.md)
- For setup, sign-in and connection problems, start with [install](install.md), [guided setup](setup-guide.md)
  or [team member onboarding](team-member-onboarding.md#verify-trust-and-behavior).
- [Contributor testing](contributor-testing.md), [releasing](releasing.md) and the
  [contribution guide](../CONTRIBUTING.md)
- [Security policy](../SECURITY.md)

The [full documentation source](https://github.com/agentscm/agentscm/tree/main/docs) includes advanced
workflow, integration and implementation details beyond this guided set.
