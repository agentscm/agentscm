# PostHog analysis recipes

These recipes describe views to create only after a PostHog Cloud EU project and the production ingress are separately approved and configured. The client retains only the first observed outcome for each source, feature, and UTC day and sends one `agentscm usage` indicator. `count` is always `1`; it is not an action total. A later success cannot replace an earlier failure, so the data cannot measure a reliable error rate or eventual success rate.

All events use `$process_person_profile: false`. PostHog calls them anonymous events because they have no person profile, but the stable random installation ID is pseudonymous and linkable across reporting days. Use trends, retention insights, or SQL over `distinct_id` and event properties. PostHog person cohorts and lifecycle insights do not include these events. The current CLI emits only installation-scoped IDs; the separate Hub namespace is reserved for a future reporter and is currently empty.

## Observed installations

Create a Trends insight for `agentscm usage` with:

- filters: `source_kind = installation`, `feature = installation`, `outcome = succeeded`;
- aggregation: unique `distinct_id`;
- interval: day or week.

Call the result observed reporting installations. Disabled, offline, blocked, dropped, and not-yet-configured installations are absent, so this is not total installs or downloads.

## First-use activation

The client excludes quickstart, setup and uninstall from the successful meaningful work that triggers `first_use`. Define activation as date co-occurrence for one installation ID:

1. a successful `installation` indicator; and
2. a successful `first_use` indicator dated from the same UTC day through seven UTC days later.

Create this as a saved SQL insight over event properties and `distinct_id`, or as a retention view only after confirming its same-day behavior. Every event timestamp is normalized to UTC midnight, so same-day records have no trustworthy order. Do not use a funnel to claim exact sequencing or time-to-value. Report the eligible installation count alongside the date-co-occurrence rate. Delivery loss can look like non-activation.

## Weekly observed return

Create a Retention insight aggregated by unique `distinct_id`. Use successful `first_use` as the start event. Use `agentscm usage` with `source_kind = installation`, `outcome = succeeded`, and `feature` excluding `installation`, `first_use`, and `uninstall` as the return event. Select weekly periods and first-time retention when evaluating activation cohorts.

Do not build a Hub-scoped view yet: the Hub reporter is future work and the current CLI emits installation IDs for all feature categories. A `feature = hub` indicator records use of the Hub-management CLI under an installation ID; it does not prove that a Hub server ran, stayed available, had multiple participants, or completed coordination.

## Feature-day adoption

Create a Trends insight for `agentscm usage`, filtered to `outcome = succeeded`, broken down by `feature`. Use unique `distinct_id` to compare the reporting population using each feature in a day or week. Use event count only to measure reported feature-day indicators; do not label it command executions or completed operations. A separate failure view shows first-observed failed feature-days only, not an error rate, retry count, or eventual outcome.

The schema does not report completed work items, accepted handoffs, resolved scope negotiations, tests, reviews, merges, users, organizations, or time saved. It cannot support claims about completed coordination, productivity, customers, seats, or market-wide usage.

## Coverage checks

For every dashboard or investor export, show the reporting distinct-ID count, date window, source namespace, excluded features/outcomes, and known ingestion gaps. PostHog free-tier or billing-cap drops, relay/backend outages, opt-outs, and blocked egress make coverage incomplete. Validate event UUID deduplication and inspect ingestion warnings before treating retry counts as unique observations.
