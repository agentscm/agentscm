# AgentSCM telemetry ingress (staged)

This directory contains a dependency-free Node.js 22 relay for the proposed AgentSCM usage telemetry contract. It is not deployed or configured in the product. It refuses to start unless `AGENTSCM_TELEMETRY_INGRESS_ENABLED=1` and a dedicated `POSTHOG_PROJECT_TOKEN` are both present. The product's production endpoint must remain unset until the separate privacy, notice, retention, deletion, edge-logging, and lawful-sharing work is complete.

The only route is `POST /v1/telemetry/batches`. It accepts at most 64 KiB and 64 strictly validated records, rejects unknown fields, compressed bodies, query strings, and duplicate event IDs within a batch, and applies in-memory global and per-source rate limits. Source IDs are public pseudonyms rather than authentication; callers can invent IDs and events. Do not put Hub, forge, or product credentials in requests.

The production transport is pinned in code to PostHog Cloud EU's batch capture URL; only the test handler accepts a loopback HTTP backend. The relay never forwards inbound headers, IP addresses, or user agents. It disables GeoIP enrichment and person-profile processing on every event. PostHog calls these anonymous events because they have no person profile, but AgentSCM's stable random installation ID is a pseudonym and remains linkable across reporting days. Person-profile-free events support trends, retention, and SQL over `distinct_id`, but not PostHog person cohorts or lifecycle insights. PostHog documents its event UUID as the deduplication identity; the relay derives the same UUID from `event_id` on every retry and also exposes the original ID as `$insert_id`. The relay is stateless and cannot itself prove that a retry was stored only once.

A `202` response means PostHog accepted a well-formed request. PostHog documents that capture acceptance does not guarantee later ingestion or queryability. Quota-limited, malformed, timed-out, and backend-error responses are not acknowledged as success. Routine request bodies, URLs, source IDs, IP addresses, and user agents are not logged by this process; production edge, platform, and exception logging must be inspected separately.

Run local tests with:

```sh
npm test
```

No dependency installation is required.
