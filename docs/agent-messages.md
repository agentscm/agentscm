---
title: "Messages and continuation notices"
---

# Messages and continuation notices

AgentSCM delivers a bounded pointer to a session that already exists. Supported hooks and MCP calls can carry it on the session's next ordinary call. A qualified native background waiter can attempt idle delivery; installing a plugin or creating Work does not establish that the host can wake a chat. Delivery, a queued native turn, recipient acknowledgement and task completion are separate facts.

The local engine does not start replacement conversations, run a model, resume an ordinary closed chat or take over its Work automatically. Closed Work remains available through the existing recovery commands. Optional remote or explicitly self-hosted Hub pages retain human message controls; the local release does not include a dashboard.

## Human messages and consent

Only the Hub's own signed-in browser session can send a human message, with its CSRF protection. Agent tokens and script bearer sessions cannot send one. The recipient must be the person's own live writing token and named session; a Work-specific message must name its lead.

A message notice is not authority. Call `agentscm_hub_message` with the **source message id**, `session` and `status: accepted` before acting. Its authenticated reply supplies the stored note and current consent mode:

| Mode | Behavior after verification |
| --- | --- |
| Manual, the default | Show the note and wait for the person's yes in that session. Answer `declined` if they refuse. |
| Auto | Continue only within the task's existing rules and permissions. |
| Ignore | Queued messages are held and acceptance is refused. |

Modes can be set globally or for one repository. Acceptance rechecks current mode in the same transaction as the receipt. Identical acknowledgement retries return the retained result with `newly_acknowledged=false`; a repeated receipt does not authorize another work effect. The source domain retains `queued`, `delivered`, `accepted`, `declined`, `done` and `withdrawn`. `accepted` is not `done`.

Human messages retain their existing bounded settings storage, at most 50 per person and repository. The delivery ledger stores source references and transport facts, not another copy of the note or task authority. Agent, forge and repository text remains untrusted.

## Coordination and declared dependencies

Coordination pointers name an existing request or directed response. Read `agentscm_coordination_inbox`, acknowledge the current sequence with `agentscm_coordination_ack`, and reply through the existing split/join/handover/decline operations. Acknowledging a pointer grants no edit consent and does not release a hold.

Dependency pointers follow durable Work declarations, including the exact repository, PR and head where declared. Read every page of `agentscm_work_dependency_notices`, then use `agentscm_work_dependency_ack` with the delivery id and current Work generation, declaration sequence and semantic version. An old notice, a completed Work or an arbitrary green PR does not prove the declared prerequisite succeeded.

The existing waiter, eligible native/MCP calls, resident observation and authorized owner-status reads use the same bounded refresh. It observes stored forge evidence and creates a logical notice when condition meaning changes; it does not fetch fresh forge data itself. Polling timestamps and heartbeats do not create new logical versions. A baseline is not a new success event. A change from A to B and back to A creates two versions. Pagination or source-coverage limits remain explicit.

Refresh never renews session activity or a Work lease, offers a notice, acknowledges a cursor, or advertises native wake capability. Without a resident, waiter or owner-status read, no background refresh is guaranteed; next-call recovery and existing lease expiry remain available.

## Transport evidence and retry

Protocol-1 cooperation is negotiated against the current Hub. Native session/incarnation metadata comes from the local hook/MCP transport, outside model-supplied tool arguments. An existing MCP process pins its initial association and cannot adopt a newer incarnation when the same native session id resumes. Legacy or ambiguous clients remain unqualified for process-exit observation.

A permitted managed credential renewal keeps ordinary MCP calls working with the new credential.
The existing process then permanently omits native incarnation headers for that route; it cannot
transfer the old credential's lifecycle identity or adopt a later start nonce. Coverage remains
legacy/unknown until a fresh native start and MCP process establish their own association. Renewal
does not clear terminal uncertainty, authorize automatic enqueue or restore an enqueue budget.

One semantic source version has one logical delivery id. Its recipient identity is independent of incarnation; changing incarnation or rotating a credential does not grant a new automatic enqueue budget. Each attempt is fenced by current incarnation and Work/domain generation.

| Fact | What it establishes |
| --- | --- |
| Pending notice | A current source version awaits its recipient. |
| Offered pointer | A hook/MCP result or native transport attempted delivery; the model may not have read it. |
| Possibly enqueued or unknown | An external native queue may have accepted the attempt. It must not be blindly retried. |
| Transport reported | The native adapter reported its outcome. This does not prove the chat resumed or the model acknowledged it. |
| Recipient acknowledgement | The authenticated recipient reviewed this current source version. It grants no additional execution or merge authority. |

A failure proven to occur **before process launch** can retry the same logical notice after bounded backoff. Once a child process started, nonzero exit, cancellation, missing status or interruption is conservatively unknown unless a qualified host contract proves otherwise. A successful queue command does not establish model uptake. No `codex queue --resume` is used.

A private bounded journal is shared with lifecycle reporting. The waiter records its attempt before external enqueue; the resident can retry the HTTP outcome report but never reruns a native queue command. Unknown schema, corruption or capacity failure preserves pending evidence and reports incomplete coverage. A lost HTTP acknowledgement is retried idempotently.

The same logical pointer can be reoffered on a later ordinary hook/MCP call after source revalidation. This does not enqueue another native turn or permit duplicate work. Stale lead, source, token, incarnation, generation or consent is refused.

## Native capability limits

- **Codex on Unix:** the session's own waiter may invoke `codex queue --thread=<native-id> --message=<pointer>` only with negotiated support and a positively qualified, still-live native host fingerprint checked immediately before enqueue. Qualification needs two native hook events, so a newly started chat with no further event has no idle-delivery guarantee. Busy-turn preemption, automatic resume and replacement sessions are excluded. Actual installed closed-thread behavior must be qualified before idle wake is advertised.
- **Claude Code:** idle wake depends on the host's qualified asynchronous hook contract. Writing stderr or exiting with the wake code is not recipient acknowledgement. Interactive behavior does not prove ordinary headless/print-mode continuation.
- **Copilot CLI:** its asynchronous notification waiter returns a pointer as stdout `additionalContext`. It starts only after a supported notification and is rearmed by a later notification. Idle processing depends on the qualified host notification contract; writing context does not prove recipient uptake. Synchronous hooks never open the long wait.
- **Other clients, desktop/shared hosts, wrappers and ambiguous ancestry:** use the supported next hook/MCP boundary. Generic Node, shell, `env` and `timeout` processes do not qualify as native hosts unless the actual supported executable is positively identified. No raw argv is collected.
- **Windows:** the native waiter currently exits immediately; next-call behavior remains the supported path.

A waiter uses the existing agent token and saved checkout binding, holds the wait route for up to 25 seconds and retries within its bounded lifetime. It exits quietly when its native parent/session ends, its binding changes, consent is Ignore or the route is unavailable. There is no universal latency promise. A seven-day maximum waiter lifetime does not prove seven-day reachability.

A startup waiter allows up to 100 seconds for an earlier waiter to leave, then 15 seconds for
registration and host qualification. A later registration needs a subsequent turn boundary to
rearm delivery. If compaction or an in-process resume reuses a live session and no earlier waiter
is holding its lock, the native hooks provide no shared event identity to distinguish the old
start before registration changes. The waiter can stop on that change; the next Stop hook rearms
it. This startup case has no idle-delivery guarantee. A waiter that has sent a request never follows
a replacement session incarnation.

## Deadlines and recovery

A new dependency semantic notice has a fixed pickup deadline ten minutes after creation. Refresh does not extend it. Existing human message deadlines remain based on observed reachability: two minutes for `wake_now`, `unreachable` or `ended`, five for `wake_soon`, and ten for `next_step`. These labels are transport observations, not promised wake times.

Only the actual recipient's domain/dependency acknowledgement clears the unread condition. Lease renewal, another tool call or queue acceptance cannot do so. A terminal session with unresolved Work produces one owner recovery incident. Current unresolved requests against intentionally retained holds ask the owner immediately on observed terminal state, or after the pickup bound when the recipient is merely unreachable. Sharing one owner does not suppress escalation. The request/target generation deduplicates overlapping recovery paths.

Use the permitted Work recovery preview to resume or release deliberately. A deadline never authorizes automatic takeover, source cleanup, completion or a forge merge. A genuine authorized resume changes the current state; stale pointers remain history.

## Interfaces

Existing human controls use `/v1/hub/session/agent-messages` and its `/mode` route with browser authentication. The native wait route remains `POST /v1/hub/agent-messages/wait`; negotiated clients add `cooperation_protocol: 1` and `incarnation` beside `session` and `agent`. Its response can carry `delivery_id`, `attempt_id`, `incarnation` and pointer `text`. Older servers receive legacy fields only.

Authenticated transport operations use `/v1/hub/cooperation/register`, `/end`, `/state`, `/refresh` and `/transport`. Transport outcome reporting is distinct from MCP source acknowledgement. Read-only, revoked, cross-repository and mismatched native identities cannot mutate delivery or lifecycle state.

Qualification is specific to the recorded source, host version and environment. Synthetic tests and source inspection do not establish live idle wake, desktop/IDE parity, Linux service behavior or successful deployment.
