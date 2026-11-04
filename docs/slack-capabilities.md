---
title: "Slack capability matrix"
---

# Slack capability matrix

This maps the optional Slack adapter to the AgentSCM capabilities it uses. The implemented surface is exercised locally with signed synthetic requests and a fake Slack API; Slack API access, workspace installation, scopes and rollout status are unverified here.

The adapter is private: it sends direct messages about four Hub events and accepts one kind of write, a holder answering a handoff for their own agent. Narrow app-mention queries read the shared deterministic status and answer in the requester’s private App Home. Optional bounded reminders require explicit configuration.

| AgentSCM capability | Slack fit | Status | Boundary |
|---|---|---|---|
| Operator setup: `hub slack manifest/project/install/setup/doctor`; `hub::slack::setup`; `quickstart --slack-config-file` | Shared setup commands for an installation agent and operator; manual app settings remain available | Implemented | Requires official Slack CLI authorization and operator-supplied private credentials. Callback echo proves reachability only; live delivery and buttons need separate acceptance. No Hub setup UI or Marketplace listing. |
| Hub event log: `edit_repeated`, `handoff_decline`, `task_overlap`, `coordination_requested` | Direct message to each person involved, as it happens | Implemented | Only these four kinds; `coordination_requested` only for a target whose lead agent is paused or stale. Other overlap notes, refusals, pull request notices and granted handoffs stay between agents. History from before installation is never sent. |
| Handoffs: `agentscm_ask`, `agentscm_handoff` | **Hand it over** / **Keep it** buttons on the holder's message | Implemented | Answers the oldest open request for that file addressed to the holder's agent, only for the person bound to that agent's owner; recorded as given from Slack. No requests are filed from Slack. |
| Work history, generated briefing, workflows, activity: `/v1/hub/standup/*`, `/v1/hub/repos/{repo}/activity`, MCP `agentscm_work_*`, `agentscm_coordination_*`, `agentscm_workflow_status` | Open Hub link | Hub UI only | No Slack views or forms: they were refresh-driven copies of the Hub UI. |
| Shared status: `/v1/hub/standup/status` | `@AgentSCM status`, `needs` or `proceed`, optionally the configured repository | Experimental | Same named recipients/actions/evidence as the shared projection; private App Home only, exact binding authorization at enqueue and delivery, no general chatbot. `app_mentions:read` plus `app_mention` subscription require reinstall/reauthorization; direct-message queries unsupported. |
| Outstanding actions | Default-off reminders, optional authorized escalation and confirmed resolution | Experimental | Bounded cadence/quotas/retries and local SQLite journal; obsolete actions suppressed. Quiet tasks alone do not alert. Unknown absence is not resolution; directed-request resolution requires an exact unchanged retained request/current response. Fake API qualification only. |
| Workspace administration, execution, approval, native landing, source transfer, forge administration | Open Hub link | Hub UI only | Browser session with CSRF protection; Slack never performs these. |
| Slack channels, canvases, Lists, Work Objects, agents | None | Not used | Posting work to shared channels discloses it beyond the people involved; no channel history or broad message-reading scopes are requested; only app mentions are received. |
| Retention | Zero-copy for Slack content | Boundary | The journal keeps a cursor and queued message bodies for at most a day after sending; no raw Slack payloads or conversation content. Normalized status jobs expire after one day, resolved/inactive-binding episode history after seven days. |
