# Work insights

> **Alpha: not for production.** The API and storage schema may change without migration.

Work insights provide a bounded, repository-scoped view of reported work duration, coordination signals, usage reports and stored publication observations. They are available to the same repository readers as the Hub dashboard. The API defaults to a seven-day window and returns at most 20 work items per page.

The projections keep different kinds of evidence separate:

- **Reported work state** comes from stand-up checkpoints. “Reported finished” means an agent reported a finish; it does not mean the work was accepted, merged or landed.
- **Usage** is a set of retained reports. Coverage is `unknown` when there are none and `partial` when at least one exists. A report does not prove that every attempt was captured.
- **Publication state and checks** are the last retained controller observations attached through a claimed source association. Check records keep the latest result per pull-request context; superseded heads’ checks are not retained. They do not establish current approval, qualification, acceptance or landing.

## Optional completion reporting

Automatic completion observations are **off by default**, independently for each saved checkout
connection. From a connected checkout:

```sh
agentscm client reporting status
agentscm client reporting on
agentscm client reporting off
```

Pass `--root /path/to/clone` to select another checkout. After changing this setting, a running
local MCP proxy stops on its next request without forwarding it; restart the proxy before
continuing. Native hooks read the setting on each invocation.

When enabled, the managed native session-end hook and the Hub MCP `agentscm_done` call attempt
to record one observation for the caller's current work. A reported `finish` checkpoint through
the opted-in Hub HTTP or MCP transport also records it. Retries and repeated completion signals
for the same token, session, work item and work generation share one observation. This is not
one report per tool call. A new work generation can produce another observation.

The observation contains the registered agent label, `other` phase, `unknown` outcome,
`unknown` provider/model, and no token counts, cost or source commit. A stopped session does not
prove success. No prompt, response or transcript is read to fill missing values. Explicit usage
reports remain available when the toggle is off and may coexist with a completion observation;
the observation count is not an attempt count.

A completion-only record changes usage coverage from `unknown` to `partial`, adds one missing-cost
observation and an unknown-provider/model token group. It supplies no measured tokens or cost.
The current summary does not separately count completion-only records, so do not use observation
or coverage counts to compare measured usage between clients with different reporting settings.

Reporting adds no agent-facing messages. Errors do not block releasing or finishing work and
are logged by the Hub without report contents. It is best effort: crashes, missing completion
calls, no matching work, expired credentials or storage limits can leave gaps. It does not
automatically scan or backfill old sessions. An opted-in retry of an earlier finish can create
the missing observation while that same generation is still finished. Switching reporting off
does not delete retained observations.

All Hub MCP clients, including Grok and custom orchestrators, can use this path by calling
`agentscm_done` or submitting a finish checkpoint. Direct HTTP/MCP callers opt in with exactly
one `x-agentscm-completion-reporting: true` request header; absent or other values are off.
The local proxy supplies it from the checkout setting. The header applies to completion calls
on `/mcp`, `/v1/hub/hook/{agent}` and `/v1/hub/standup/checkpoint`. It is a caller preference,
not a repository-wide enforcement policy. The separate native workspace task MCP is unaffected.

## Reporting usage

An agent can send the same strict usage record through the Hub's `agentscm_work_usage` MCP tool
(including through the local client proxy) or the universal
`agentscm work usage --request FILE|-` CLI command. The separate native workspace task MCP does not
expose Hub work tools. This is the provider-neutral path for Claude Code, Codex CLI, Gemini CLI,
Cursor, Copilot, any other Hub MCP client, and Grok when it is configured as a Hub MCP client.
AgentSCM does not claim automatic session-usage capture for those clients.

```json
{
  "work_id": "wrk_example",
  "observation_id": "attempt-3",
  "generation": 1,
  "session": "agent-session",
  "phase": "implementation",
  "outcome": "succeeded",
  "agent": "codex",
  "provider": "unknown",
  "model": "unknown",
  "usage_basis": "reported",
  "source_commit": null,
  "input_tokens": 1200,
  "output_tokens": 300,
  "measurement_scope": "request",
  "cost": null
}
```

Use the literal label `unknown` when the client cannot determine its agent, provider or model identity. Do not infer a provider from the agent name: an agent client and its model provider are separate fields. Token counts may be omitted independently. Missing tokens or cost mean unknown, not zero.

Provider-backed clients may also include cache-read and cache-creation input counts, action or
attempt identities, and native provider request or event identities. These are optional facts and
are omitted from legacy canonical records when absent. `measurement_scope` is either `request` or
`session_cumulative`. Insight token and cost totals include request-scoped and legacy records but
exclude cumulative session records, which are retained only as separate evidence. This prevents a
provider's final total from being added to the request records that contributed to it.

`usage_basis` is `reported` for a client-supplied record. `provider_result` means the caller says
the counts came from a structured provider result; the Hub does not verify that claim. A cost, when
present, has a three-letter currency, an integer `amount_micros`, a caller-supplied basis label, and
one of these kinds:

- `reported_charge`: a charge reported by the caller. It is not a verified bill.
- `list_price_estimate`: an estimate under the named rate or price basis.

The Hub does not calculate cost from token counts. It groups costs only when kind, currency, basis, usage basis, agent, provider and model all match.

Each numeric token or cost field is between zero and 1,000,000,000,000 inclusive. Each work item
retains at most 500 immutable usage observations, also subject to the bounded work-payload capacity for the full repository scope
(provider, host, repository name and stable repository ID). Reaching either limit rejects a new observation; existing observations are
not overwritten. `observation_id` makes an exact retry by the same author idempotent. Reusing it
with different content or from another author is rejected. For unfinished work, the author must be
a current participant in the named session and generation. After a reported finish, only the
current lead can report against that same generation. Records are scoped to the authenticated
repository. Stored observations retain the session label and author token ID reference for authorization and retry handling, not the token secret. Insight responses omit these identifiers and do not contain prompts, responses or
transcript contents.

## Reading insights

`GET /v1/hub/standup/insights` accepts `since`, `until`, `limit`, `after` and `through`. The
default cohort is work created in the previous seven days, the maximum window is 90 days, and
`limit` cannot exceed 20. A continuation must repeat the exact `since`, `until` and `through`
returned with the first page and pass its opaque `next_after` as `after`. Those values fix cohort
membership and ordering. They do not freeze each item's metrics: every page projects retained state
at that response's `as_of`. Counts and sums displayed for a page are page-only unless the response
explicitly says otherwise.

`GET /v1/hub/standup/work/{work_id}/insights` returns the bounded detail for one work item. The equivalent read tools are `agentscm_work_insights` for a page and `agentscm_work_insight` for one item.

Duration fields can be `null`. If history is missing, truncated or crosses a retention gap, all timing and coordination-signal fields are unknown. Otherwise, blocked, paused, handover and finished duration are wall time in the reported state. Active duration is counted only through the stored last check-in plus the 15-minute active window; later time is unknown. These values are derived from retained reports and do not measure actual keyboard, tool or model execution time. Failed attempts are shown only when present in retained usage reports.

Both endpoints are read-only. They return projection evidence and never approve, qualify, accept, publish, merge or land work.

The list projection returns at most 20 token groups and 20 cost groups per work item; one-item
detail returns at most 500 of each. Totals and known-report counts still cover all retained reports.
`groups_complete: false` means only the grouped breakdown was truncated.

`publications_complete` describes lookup coverage of retained records, not a complete check
history. It is false when the stable repository identity is unavailable, evidence limits omit
records, or a publication has a pull request whose checks cannot be looked up without a forge
connection. A disconnected repository can still show its stored publications.
`commits_truncated` means there are more than 16 distinct valid commit claims or more than 256 retained versions. See `history_retention_gap` for compacted history.
Only stored checks matching the publication’s exact commit are returned; an empty list does not
mean that commit was never checked.
