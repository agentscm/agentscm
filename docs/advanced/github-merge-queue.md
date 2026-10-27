# GitHub merge queues

> **Experimental (alpha).** Unix only, GitHub App connections only. Tested only against the in-process fake GitHub and synthetic fixtures; never run against a live merge queue. The `merge_group` payload field names and whether GitHub accepts a commit status on a group commit as a required check are taken from GitHub's documentation and its published webhook schema, not from a live capture. Do not require the context on a queue-protected branch until the [live qualification](#live-qualification) below has passed.

The Hub's opt-in workflow status `agentscm/workflow/<target>` (see [workflow status for candidate pull requests](../hub.md#workflow-status-for-candidate-pull-requests)) is also posted on merge queue groups, so requiring it no longer has to block queued merges.

## What a group needs to pass

GitHub builds each queue entry as a commit on `gh-readonly-queue/<base>/pr-<number>-<head sha>`: the base (or the entries ahead of it) combined with the queued pull request. That commit can differ from every pull request head, so a head `success` is never copied onto it. The Hub posts `success` on a group commit only when all of these hold, read again from GitHub and the pinned native store:

- the queue branch still points at the group commit;
- the queued pull request is open, not merged, within the connected repository, and its live head equals the head the queue branch names, which equals the published candidate commit;
- the group's base is the candidate's target and the group's base commit is the candidate baseline;
- the group commit is built on that base: its first parent is the event's base commit (the candidate baseline) and any second parent is the queued pull request head. A commit with no parent, or with more than two, fails. Fake-tested only, like the rest of this page;
- the pinned native store currently qualifies the candidate for the profile's pinned requirement (as for a pull request head);
- the group commit's tree equals the native revision's tree.

The last two together are the exact-source rule: the bytes GitHub would land are the bytes the store qualified. Otherwise the group gets:

| Situation | Status |
| --- | --- |
| Not (yet) qualified | `pending: awaiting native qualification` |
| Built on other queued entries (its base is another group the Hub saw) | `pending: unsupported: no evidence covers this group's combined source` |
| Target advanced since the candidate baseline | `failure` |
| Group tree differs from the qualified revision | `failure` |
| Pull request head changed, closed, fork, other base | `failure` |
| Policy, requirement or context changed (stale) | `failure` |
| Store or qualification unreadable | `error` |
| Queued pull request has no candidate publication, or is already merged | nothing posted |

A group combining two or more pull requests therefore never succeeds. Qualifying a combined source needs core evidence for a group subject, which the native store does not produce yet (see [contract](#integration-contract)). With a required context, configure the queue to build one entry at a time (`max_entries_to_build: 1`, or merge limits of 1) or such entries wait until the queue's check timeout and are removed.

## Lifecycle, supersession and recovery

- A `checks_requested` delivery records the group (`hub_merge_groups`, named migration `merge_groups`) and judges it at once. A `destroyed` delivery ends it; a group delivered as destroyed first is recorded as ended and a later `checks_requested` for the same commit never revives it. Duplicate deliveries are deduplicated by delivery id and are idempotent anyway.
- The queue branch is the only witness of a current group. Every pass reads it first: a branch that is gone ends the group (`merge group branch is gone`), one that points elsewhere ends it (`merge group was rebuilt`). A rebuilt, re-enqueued or reordered group is a new commit with its own row, so an earlier result is bound to a commit GitHub no longer tests and cannot satisfy or overwrite the new one.
- Each poll rechecks up to 20 current groups of the repository, oldest first. That is the recovery path after a restart, a missed delivery, a lost reply, or a policy or qualification change. More than 20 current groups are covered over several passes.
- Posts use the same journal discipline as pull request checks: a compare-and-set reservation that also requires the group to be current, then the post, then completion. A lost reply leaves the row `attempting` (`posting: "uncertain"`) and the next pass posts the then-desired status, since the newest status of a context wins on GitHub. A definite refusal (403, 404, 422) is recorded as `refused` and not repeated until the desired status changes, except when it would leave a success showing. Before a `success` the Hub re-reads the native qualification and the queue branch. Rate limiting (`Limited`) and credential failures (`Auth`) stop the pass and put the connection in the existing backoff.
- A merged pull request's group is left alone: the group commit may now be the target tip.
- When a queue branch is gone, the Hub reads the queued pull request once. If it is merged and its merge commit is the group commit itself, the group ended `landed`; merged as any other commit, `pull request merged as another commit`; otherwise `merge group branch is gone`. The projection's `outcome` is `landed` only in the first case, `merged_reported` when GitHub's destroy delivery said `merged`, `not_landed` for `dequeued`, `invalidated` or a rebuilt group, and `unknown` otherwise. A closed pull request or an agent's message is never an accepted outcome.
- Turning the profile's `publish_forge_status` off retires a success that may still show on a current group with `not applicable: forge check disabled`.
- Ended groups are deleted 30 days after they end.
- Cost per current group per pass: one queue branch read, one pull request read with its base ref and head commit (commits are cached), one group commit read (cached), one rules read and one status page read, plus a post when the desired status changed. Per opted-in target per pass: one installation read with the App's JWT (not counted against the installation budget) and one queue branch listing. With 20 groups that is up to about 140 requests a pass; budget the poll interval against the installation's 5000 requests per hour.

## Delivery readiness

GitHub delivers `merge_group` only to GitHub Apps with at least read access to merge queues, never to repository webhooks. Each poll records, per opted-in target, in memory (`null` until the first poll after a start):

- `delivery`: `subscribed` (the installation has the permission and the event), `permission_missing`, `event_not_subscribed`, `personal_token_unsupported` (a personal-token connection can never receive groups) or `unknown` (the installation could not be read).
- `undelivered`: pull request numbers of current queue branches no delivery has named, from one listing of up to 100 `gh-readonly-queue/<target>/` branches; `null` when the listing was refused or truncated. Without a delivery the Hub does not know a group's base commit, so it cannot judge or post on it; the queue entry waits until GitHub redelivers or its check timeout removes it. Redeliver from the App's advanced settings.

Authority is rechecked each time: deliveries need a valid `X-Hub-Signature-256`, a connection id from the URL, and a repository id that maps to that connection; anything else is ignored without a write. A repository removed from the installation or a revoked credential stops all reads and posts for it.

## Required context and source

The Hub never creates or changes rulesets or branch protection. It reports what it can read for each current group:

- `ruleset_requirement`, from `GET /repos/{owner}/{repo}/rules/branches/{base}`: `required_from_this_app` (a ruleset requires the context with the connection's App as `integration_id`), `required_any_source`, `required_from_another_source`, `not_required` or `unknown` (refused, truncated or unreadable). The weakest matching rule wins. Classic branch protection is not read (it needs administration access), so `not_required` means only that no ruleset requires it.
- `status_source`, from the group commit's newest page of statuses: `hub` (the newest status of the context was posted by the App's bot account, or by the account a personal token acts as), `another_account`, `absent` or `unknown`.

Only `required_from_this_app` means GitHub itself ignores the same context from other accounts. With `required_any_source`, anyone with `statuses: write` can post the context; `another_account` shows that someone did, and the Hub does not fight it by reposting. A personal-token connection has no App identity, so it can never be the pinned source.

Permissions: the App needs `statuses: write`, `pull_requests` read, `contents` read and `merge_queues` read. New Apps from the Hub's manifest request it and subscribe to `merge_group`. An existing App must add the permission and the event in its settings, and each installation must accept the new permission; `delivery` reports which is missing.

## Not covered

- Combined multi-entry groups (pending, see above).
- GitLab merged-result pipelines and merge trains. The Hub does not post the status on their commits, by decision: see [the GitLab paragraph in the Hub guide](../hub.md#workflow-status-for-candidate-pull-requests). GitLab behavior is unchanged.
- The experimental [local validation attestations](local-validation.md) post `agentscm/local-validation`, a separate, worker-asserted status. It does not use this path, and a green local-validation status satisfies nothing here.
- Approvals and review requests stay GitHub actions. This lane does not request reviewers, approve, enqueue, merge or bypass a failed check. Provider account identity is not decision authority: an agent can act through a human-named account, so no login, account id or signature is promoted to a proven human decision (see the contract below).
- Automatic review bots: a draft pull request does not guarantee a third-party reviewer stays idle; some integrations review drafts. The Hub cannot read other integrations' configuration, so their behavior is `unknown` to it, and it never changes them. To qualify an installation: open a synthetic draft pull request, record which bots comment, mark it ready, and record again.

## Integration contract

Version `merge-queue-contract/v1` on branch `feat/github-merge-queue-checks`. Logical fields; existing types are reused.

Forge subject (`MergeGroup`, `forge_check::merge_group::Projection`): connection host and immutable repository id (the Hub repository row, never owner/name alone); subject kind `merge_group` (pull request heads keep the existing `forge_check` view); group commit, base ref and base commit; queued pull request number and the head the queue names; `stacked_on` (pull request numbers of queue groups the Hub saw beneath it, nearest first, at most 10); `lifecycle` (`current` or `ended`) with `ended_reason`; `observed_at`, `updated_at`. One group with two pull requests appears as two rows, each with its own pull request and `stacked_on`, never as one pull request's source.

Assessment: today the only assessment is the existing native qualification (`NativeState`: receipt, requirement digest, revision tree) of one candidate, judged by `evaluate_group`. Outcome (`Verdict`), applicability (lifecycle and the stacked rule) and the exact subject (`binding_digest`, domain `agentscm-forge-check-merge-group-binding/v1`, covering group commit, tree, base and native bindings) are separate fields. A missing group assessment is `pending`, never the pull request head's result. A core assessment of a combined group source needs, from the core: the evaluated subject (group commit and tree), policy, recipe and setup identities, outcome, current applicability, coverage and assurance basis as separate fields, and whether it may satisfy this configured context.

Publication result: `context`, `group_sha`, `state`, `description`, `binding_digest`, `posting` (`pending` = nothing reserved, `uncertain`, `posted`, `refused`), `ruleset_requirement`, `status_source`, and the accepted mapping `outcome`. Consumers read this projection (`forge_check::merge_group::group_projection`, `pull_projections`, `queue_readiness`, and the publication API's `forge_check.merge_queue`), never a status description or a pull request comment.

Approval observation for the core's protected human decision path: a GitHub review supplies only the account login, account type (`User` or `Bot`) and `performed_via_github_app` when set. Who acted through the account is `unknown` and stays so. This lane adds no consumer of these fields.

## Live qualification

Not done. GitHub refused a merge queue ruleset (`422 Invalid rule 'merge_queue'`) on a private repository of an organization on the Team plan, so a live run needs a public organization repository or a plan that offers merge queues on private repositories. It also needs a GitHub App installed with the permissions above and a Hub reachable for webhooks:

1. Require `agentscm/workflow/main` from the App in a ruleset and enable the queue with one entry built at a time.
2. Qualify a candidate, open its pull request and enqueue it: the group commit must get `success` from the App and merge.
3. Enqueue a second candidate, then push to the target so GitHub rebuilds the group: the old group must end and the new one be judged afresh.
4. Post the same context from another account on a group: `status_source` must report `another_account`.
5. Capture the `merge_group` payloads (synthetic data only) and compare their fields with the parser.
