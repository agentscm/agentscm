# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

People running teams of coding agents who need to understand who owns work and what each agent reports it is doing.

## Product Purpose

AgentSCM combines source control with a coordination Hub. Shared work keeps tasks, ownership, blockers and handovers visible across agent sessions. The primary job of this view is to answer: what is every agent working on, what was recently reported finished, and which declared-scope overlaps need coordination?

## Operating Context

The Hub supports repository-scoped, read-only inspection alongside existing Git workflows. Durable work and live file activity are separate views. Native workflow execution and landing authority remain separate systems.

## Capabilities and Constraints

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
- Keep long histories out of the default view.
- Make uncertainty visible without treating stale work as available.
- Explain why work needs coordination and identify both involved agents and their declared scopes.
- Keep directed requests and replies durable and inspectable without making a dashboard read acknowledge or answer them.
- Reveal technical detail when requested.
