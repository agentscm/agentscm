# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

People working with coding agents who need to catch up on shared work, inspect a change and its evidence, and decide what needs attention next.

## Product Purpose

AgentSCM combines source control with a coordination Hub. Its web views connect work ownership, intent, source, observed actions and retained evidence so people can resume work or review a change without reconstructing every agent session. The primary questions are: who owns the work, what changed, what ran against which source, what remains unknown, and what decision comes next?

## Operating Context

The main path works alongside existing Git repositories, coding agents and GitHub or GitLab. Durable work and live file activity are separate views. The shared coordination dashboard reads Hub records; `agentscm open` also connects the browser to selected checkouts and private evidence on this computer. Local settings and workflow-library writes require the connected page's own authorization. Experimental native workflow execution and landing authority remain a separate opt-in path; Git and forge merges retain their existing authority.

## Capabilities and Constraints

- The local repository explorer reads committed Git files, history and diffs without fetching or changing the checkout. Displayed source, local working-state observations and action-bound or tested source remain distinct.
- Configured change capture retains bounded visible agent events and action receipts, with a concise intent/source/decision/gaps/next-action handoff. Capture coverage depends on the agent adapter; it is not a complete account of every session.
- Metadata-only capture is the default. Locally retained conversation bodies use a private spool; per-checkout sharing is explicit, uses best-effort client redaction, and limits shared reads to the author and Hub admins. It does not guarantee secret-free content.
- Execution outcome, verification result, source applicability, evidence availability and coverage are separate facts. An observed action or successful process exit alone does not prove that a revision was tested or approved.
- The local workflow library retains immutable skill and workflow versions, previews bounded pinned imports and simulates declarations. Productive pipeline execution requires the separate operator-local export and setup; productive artifacts do not qualify native admission.
- Supported `pr-gate` skill gates retain self-reported evidence. Living Promises retain owner-authorized commitments and advisory reports; neither surface independently verifies fulfillment or substitutes for human approval.
- Work records contain a lead, participants, declared scope, next action and agent-reported checkpoints.
- Claim freshness and reported work state are separate. Stale and unknown claims remain claimed.
- Current work and historical changes share one bounded, fixed-watermark traversal. Page counts and filters describe only the current page.
- The work overview is a separate repository-wide, bounded snapshot. It returns unfinished work, recent outcomes, and server-computed declared-scope overlaps with explicit completeness flags.
- Retained-history compaction makes a requested recent window incomplete when the removed events cannot be placed against that time range; the interface keeps that uncertainty visible.
- An empty overlap list is an all-clear only when the server marks the overlap slice complete. Declared-scope overlap is a coordination signal, not proof of a source conflict.
- Scope conflicts can create durable directed requests for the affected leads. Leads receive, acknowledge and reply through authenticated MCP or HTTP flows; the Hub dashboard only reads the queue.
- Request status distinguishes pending receipt, pending replies, responses, resolution and withdrawal. Detection remains separate because an overlap does not itself prove a request was delivered.
- The visible overview and the fresh first page of directed requests refresh about every 15 seconds. Paging holds a fixed request watermark until the user explicitly returns to a fresh first page; refresh failures keep and label the last loaded data.
- Detected and per-target overlap reasons are bounded. When more path pairs exist, the view says they were omitted and points people to the declared scopes rather than presenting the visible reasons as exhaustive.
- Request receipt and response histories are also bounded. The queue marks omitted history, relies on the server's request status, and avoids inferring an outstanding receipt or reply from a clipped projection.
- Directed requests use compact summaries with full returned paths, receipts, responses and notes available by expansion. Detected overlaps, other attention signals and recent decisions are collapsed secondary evidence with visible counts, preserving access without making the default view a long feed.
- Reading in the dashboard never acknowledges a briefing or mutates work.
- Checkpoints do not prove execution, review, acceptance or landing.
- The current implementation is alpha; real-client and production deployment qualification are separate.

## Brand Commitments

Preserve AgentSCM's name and logo. Use plain, direct language that makes ownership and reported progress understandable.

## Product Principles

- Show the task and agent before technical identifiers.
- Lead a review or handoff with intent, source, evidence gaps and the next decision; make detailed evidence available on demand.
- Keep long histories out of the default view.
- Make uncertainty visible without treating stale work as available.
- Explain why work needs coordination and identify both involved agents and their declared scopes.
- Keep directed requests and replies durable and inspectable without making a dashboard read acknowledge or answer them.
- Reveal technical detail when requested.
