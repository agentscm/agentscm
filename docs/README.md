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

## Catch up and inspect changes

- [Stand-up and catch-up](standup.md): durable ownership, checkpoints, blockers and handovers.
- [Open this computer](connect-this-computer.md): local checkouts, available evidence and shared work in one page.
- [Repository explorer](repository-explorer.md): committed files, history and diffs without fetching or changing source.
- [Change capture](change-capture.md): bounded action records and optional conversation sharing; Codex capture is narrower than Claude Code.
- [Work insights](work-insights.md): reported timing, usage and publication observations.
- [Agent source selection](advanced/agent-source-workflow.md): inspect and expand source selectively; fewer working-file bytes do not establish model-cost savings.

Work checkpoints and supported-skill evidence are agent reports. Capture receipts identify
observed actions and source; they are not independent verification or human approval.

## Reuse workflows and track commitments

- [Supported skills](design/supported-skills.md): experimental `pr-gate` v1 and its self-reported evidence.
- [All repositories](design/all-repositories.md): experimental cross-repository home page for a web sign-in.
- [Local workflow library](advanced/workflow-library.md): versioned authoring, pinned imports, simulation and explicit export.
- [Productive actions](advanced/productive-workflow-actions.md) and [pipeline rules](advanced/pipeline-rules.md): bounded artifacts and operator-local post-merge workflows, separate from native admission.
- [Intent review](advanced/intent-review.md): advisory assessment and correction tracking for Git Work.
- [Living Promises](advanced/living-promises.md): owner-authorized commitments and advisory agent reports.
- [Slack capability map](slack-capabilities.md): optional coordination messages and current qualification limits.

## Native workflow alpha

- [Architecture and trust boundaries](architecture.md)
- [Claude Code workflow loop](advanced/claude-code-workflows.md)
- [Community workspace pilot](advanced/community-workspace.md)
- [GitHub merge queues](advanced/github-merge-queue.md): restricted single-entry group support; stacked groups and live qualification remain open.
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
