---
title: "PR context"
---

# PR context

Dashboard → Where includes a read-only PR context panel. Choose an observed open
PR to inspect its retained head, other PRs sharing exact changed paths, Work with
overlapping declared scopes, explicitly PR-linked supported workflow records, and
bounded provider evidence when its identity and permissions can be confirmed.
Links open the existing Work, Skills, Inbox and Catch up views.

The same projection is available to agents as `agentscm_pr_context` with a positive
`number` and optional `expected_head` SHA. The token determines the repository.
HTTP clients use `GET /v1/hub/repos/{repository}/pr-context?number=42`, with
`expected_head` as an optional query parameter. Both transports use the same service.

An expected head is a caller-supplied comparison, not provider authority. The
answer compares it with the retained provider observation. The dashboard captures
the PR list head when its panel opens; it is not a previous-session baseline. It does not contact
the forge on demand. “Refresh context” reads the Hub again; it does not trigger
a provider sync or rerun checks. The UI labels observations older than 15 minutes,
missing timestamps, or times over one minute in the future as older or uncertain.
This display heuristic does not establish provider freshness.

## What the answer establishes

- PR metadata is a retained provider observation, with the time the provider read
  started. The provider may have changed since then.
- PR overlap uses exact retained paths. Work overlap uses the existing declared
  scope matcher. Both are advisory; they do not prove a conflict or link Work to
  a PR. Quiet assignments remain assigned.
- Workflow records are selected by an explicit PR number within the token's
  repository, never by branch name alone. Their head must match the retained PR
  head for applicability. They remain self-reported, even when the recorded
  workflow says a gate is done.
- GitHub App synchronization inspects actual installation grants separately for
  reviews, check runs and commit statuses. A `pull_requests:write` grant includes
  review reads. Missing checks permission does not imply missing review access.
  The Hub does not request new permissions. PAT evidence is unavailable here.
- Provider evidence is retained for the exact repository ID, PR number, head and
  base revision. A different or unknown revision hides that snapshot. Each item
  has its provider ID, source commit, available source URL and provider timestamp;
  the snapshot records when collection began. Reviews on another commit remain
  explicitly different. Check runs use the provider's latest-by-name view;
  attempt history and tested-merge identity remain unknown.
- Authorization is the requesting Hub token's existing repository scope. Token
  revocation is checked on reads. Installation grants are separate observations;
  live requester access on GitHub is **not** revalidated on each context read.
  A known unavailable sync or unconfirmed latest grant hides earlier evidence.
  A stored failure floor survives health recovery and restart: evidence observed
  at or before that failure remains hidden until that PR is collected again.
  Local dirty source remains unobserved. Missing facts never mean passing checks.
- Catch up uses the existing review baseline and acknowledgement flow. Reading
  context never creates or acknowledges that baseline.

The result reads independent sources, not one atomic provider snapshot. It uses
retained open PRs, the bounded Work overview and the newest 25 supported workflow
runs. It returns at most eight entries per list and four path witnesses per entry,
with omissions. Each existing overlap/workflow list has an 8 KiB budget. Text summaries are shortened to 240 characters with an ellipsis.
The serialized response cannot exceed 48 KiB. A missing PR is `not_observed`,
not a claim that the PR does not exist. It retains coverage flags, including
known last-sync gaps and whether the retained PR scan reached its limit.

Provider collection runs under the existing repository sync lock, including when
its PR list returns 304. It rotates through at most two open PRs per round. Each
selected PR first completes its identity reread, observation update and notices;
provider requests use the existing ten-second HTTP client limit. A separate
five-second deadline then bounds only the read-only evidence collection. A timeout
or ordinary failed evidence read replaces that PR's evidence without hiding peers
or refreshing the retained grant observation. Known authorization failures and
failed grant inspections hide repository evidence until a successful inspection;
previously failed PR snapshots remain unavailable until their own successful read.
Health is the retained last-known observation, with no age-based expiry or live
requester-access check. Generic timeouts or failed PR reads may leave older
confirmed grants in place; they do not establish that those grants are current.
Revocation detection depends on explicit authorization failure or failed grant
inspection, rather than interpreting a generic transport failure as revocation. Reviews read the first and, when the provider identifies it, last page of 16 rows,
then retain the newest eight observed reviews. Unread middle pages remain explicit.
Checks and statuses each read one page of at most 16 rows and retain at most eight. Malformed rows, omitted items, pagination,
missing grants and failed reads remain explicit. The evidence snapshot is bounded
to 16 KiB. A refresh reads retained data; it cannot make provider evidence current.

## Explicit Work relationships

Work detail can open context for its explicitly recorded PR. A branch name or
shared path never creates this relationship. The current Work lead sets one
relationship using stable repository ID, PR number and a full lowercase head SHA.
It is a caller assertion, separate from the provider's observed head. Unknown or
name-only repository identity cannot create a link. A Work takeover preserves the
recorded link and its original provenance; only the current lead can update it.

Use `agentscm_work_pr_link` (MCP) or `agentscm work pr-link WORK_ID` (CLI) to read it.
Supply `expected_updated_seq` to compare the Work version; the UI refuses a changed
Work snapshot until refreshed. The optional `expected_head` compares only with the
recorded link. `agentscm_work_pr_link_set` or `agentscm work pr-link-set --request -`
accepts this strict request shape:

```json
{
  "work_id": "wrk_00000000000000000000000000000001",
  "operation_id": "link-pr-42",
  "generation": 1,
  "session": "example-session",
  "expected_updated_seq": 123,
  "link": {"repository_id": 42, "number": 7, "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
}
```

Use the actual current Work generation, session and update sequence. Reuse the
operation ID when retrying an uncertain response. Explicit `link:null` removes the
relationship; omitting the field is invalid. A new operation which would leave the relationship
unchanged is rejected without adding a false change event; an exact stored retry
still returns its receipt. Read-only tokens cannot change links.
Relationship changes appear as distinct durable Work events in the existing
Catch up flow. Catch up retains a fixed delivery; a link to the current Work view
is not a claim that it is the historical relation. Reading never acknowledges it.

This is explanatory context, not a verification result or a merge gate. The
existing GitHub App permissions and merge authority are unchanged.

## Local preview

In `ui/`, run `npm ci`, then `MOCK=context npm run dev`. Open the printed loopback
URL with `#token=synthetic-preview`, then Dashboard → Where. The visible banner
identifies synthetic data. PR #42 shows context and overlap; #43 shows a context head
different from the PR list and evidence for that earlier head; #44 simulates an
HTTP 403 denial of the context request (not a diagnosis of App permissions); #45 shows a reported
blocker. Drill-down stays in the existing application. Forge links use
`example.invalid` and are deliberately not live.

The fixture preview does not qualify live GitHub ingestion, permission upgrades,
hosted verification or deployment.
