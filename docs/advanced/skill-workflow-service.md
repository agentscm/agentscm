# Skill workflow read service

> **Experimental and trusted-local.** This surface projects retained local workflow state. It does not authenticate an executor or assess admission.

`agentscm serve` exposes one read-only route for a retained skill workflow:

```text
GET /v1/skill-workflows/{run_id}
```

The server always reads its operator-selected `--data-dir`. The request cannot choose a data directory, repository, source revision or hub token. Query fields are rejected. A read never creates a workflow journal, advances or retries a step, recompiles a plan, changes policy, or joins hub identity to native authority. A missing journal or run returns 404 and remains absent.

The response contains the runtime's retained `RunView` under `run`, including its fixed `admission: "not_assessed"`, plus a separate `present_applicability` object. Historical success, failure, blocking and omission remain historical observations. They do not become current authorization.

Present applicability has the fixed basis `current_authority_repository_policy_epoch_and_immutable_revision`. `current` means only that the current read-only authority has the same repository and policy epoch recorded by the run and that the immutable source revision can be loaded within a separate 64 KiB encoded-read budget. `stale` reports a repository or policy-epoch mismatch. `unavailable` means those bounded facts could not be read, including when the revision observation exceeds its budget. This is not a plan recompile, resumability result, skill-content revalidation, executor authentication, check admission, approval, landing decision or proof that an external capability ran.

The projection preserves generic runtime diagnostics and exact native source identifiers. A retained step detail that embeds a source-read operating-system error is replaced with an explicit redaction marker before it reaches HTTP. Journal projection failures produce a sanitized 500 while the full error stays in the service log. Failures while observing present authority or source are represented by `unavailable` with a bounded reason and no internal detail. JSON output is limited to 4 MiB and uses the service's shared `--max-concurrency` work slots. Responses use `no-store`, `nosniff`, no-referrer and a restrictive content security policy.

The native service binds `127.0.0.1:7331` by default and has no authentication. `--public` permits a non-loopback bind and therefore makes this retained run data visible to that network unless an operator-managed authenticating, TLS-terminating proxy restricts it. The container command passes `--public` because it listens on `0.0.0.0` inside the container; the documented Docker and Compose publishing binds the host side to loopback. Publishing the container port more broadly also publishes this endpoint.

Read-only access makes no workflow-row, execution, approval or integration changes and does not
initialize an absent journal or native store. SQLite may create or update WAL/SHM sidecars for
existing databases, including the native authority. Reads can fail if required sidecar housekeeping
is unavailable. This is logical read-only access, rather than a zero-filesystem-write guarantee.
