# Deployment

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

AgentSCM can run as a native process or an OCI container. The two server commands are different products: `agentscm hub serve` is the authenticated coordination and Community workspace service, while `agentscm serve` is an unauthenticated read-only native source API. Never expose the latter as if it were the Hub. General Hub database, object-store, forge and credential configuration is documented in the [Hub guide](hub.md); this page defines the selected local Community workspace pilot and the boundary of the stock image.

## Selected local Community workspace pilot

The selected deployment is one supervised `agentscm hub serve` native process on the same trusted Unix host as its native stores and operator-enrolled collectors. It uses SQLite, local Hub objects, task-only `agent_task_profiles` and same-host trusted-local collection. Attached Git work uses qualified candidate publication followed by deliberate provider review and merge; native approval/landing and server execution remain separate opt-in workflows. There is exactly one Hub/workspace process, one writer for each native repository data directory and no standby or shared-volume replica.

The supervisor starts the reviewed binary directly with fixed environment and paths. This is the complete Hub command shape; provider variables and credential-file details remain in the [Hub configuration](hub.md#configuration):

```sh
export AGENTSCM_DATABASE_URL=sqlite:///srv/agentscm/hub/hub.sqlite
export AGENTSCM_WORKSPACE_FILE=/etc/agentscm/workspace.json
export AGENTSCM_WORKSPACE_EXECUTION_FILE=/etc/agentscm/execution.json
exec /opt/agentscm/bin/agentscm hub serve \
  --bind 127.0.0.1:7332 \
  --public-url https://hub.example.test \
  --objects /srv/agentscm/hub/objects \
  --secrets /srv/agentscm/hub/secrets
```

The paths and hostname are synthetic. Keep the database, objects, secrets, workspace database, execution queue, native stores and collector state on persistent local storage. The [Community workspace guide](advanced/community-workspace.md#persistent-state-and-credentials) inventories the complete set. Do not mount candidate source over controller state, token files, signing keys or provider credentials.

### Compatible artifact and configuration pinning

Use the same exact released or reviewed locally built `agentscm` artifact bytes for the Hub process and every `agentscm agent-task` operator/collector invocation in this pilot. Record its version, source revision or release identifier, SHA-256 and absolute supervisor path. The alpha does not promise cross-version compatibility among the Hub facade, task schemas, native authority and collector state.

Record the exact reviewed bytes and SHA-256 for `workspace.json`, `execution.json`, each enrollment and collector-profile JSON document, and the supervisor configuration. Also record the operator paths bound by those documents: native data directory, collector state, signing key, token file, Docker executable/socket for a command collector, or Claude Code executable for a review collector. The execution file is strict version 1 and rejects unknown fields. A configured requirement digest, runtime identity or executable digest is part of the selection; changing it is a new reviewed configuration, not an in-place compatibility assumption.

### Supervision and one-writer operation

Configure the supervisor to start only the Hub command above, restart it after an unexpected exit with bounded backoff, send a normal termination signal during planned stops and wait for exit before starting a replacement. Do not supervise `agent-task collect` as a continuously restarted service: collection is an explicitly claimed one-shot effect with durable local state. If its upload fails, use the documented `retry-upload` path for the identical retained transfer rather than running `collect` again.

The workspace execution service takes an exclusive lease on its derived queue. A second process against the same queue is refused, but this is not distributed fencing or high availability. If optional server execution is configured, active queued/running server requests become `Unknown` after restart and are not replayed. Task-only configuration does not start that server worker; its task attempts and collector transfers still require status/recovery inspection rather than inferred replay after interruption.

### Health is not authenticated readiness

`GET /v1/hub/health` is an unauthenticated process/storage probe. It calls only the Hub store's `ping`; an `ok` response does not open the workspace or native stores again, check an execution profile, authenticate a member or agent, probe a collector runtime, or prove that a current subject has qualifying evidence.

There is no combined readiness endpoint. Before admitting daily work, verify each configured repository/profile through an authenticated credential: confirm `whoami`, task `capabilities`, and a task `requirements` or `qualification` response for the intended current subject. Locally inspect the enrolled registration/profile and probe the pinned command or review runtime before dispatch. For landing readiness, require `qualification` to report `qualified: true`, `subject_current: true`, the expected revision and a non-null current receipt, then still use explicit review/approval and integration; native admission rechecks all of it. A successful `agent-task collect` process can retain and upload a completed failed check, so process exit status is never the readiness decision.

Use the health route for supervisor liveness and authenticated task/native reads for readiness. If any exact subject, origin authorization, executor registration, runtime or retained effect is stale, failed or uncertain, keep the profile unavailable for landing and reconcile it explicitly.

### HTTPS and proxy boundary

Keep the Hub bound to an IP-literal loopback address and terminate HTTPS in a separately supervised reverse proxy. Set `--public-url` to the exact externally reachable HTTPS origin used by browsers and forge callbacks. The Hub does not terminate TLS; Secure workspace/browser cookies need HTTPS outside the narrow loopback cases described in the Hub guide.

The proxy must apply connection, request-header and request/write deadlines, bounded request sizes compatible with the task and collector-upload routes, and rate limits. Restrict access to the trusted team. Do not log `Authorization` headers, cookies, OAuth/bootstrap URLs, request bodies or query strings that can contain sensitive setup material. Keep the unauthenticated health probe on an operator-only monitoring path where practical. Forward only to the Hub loopback port; do not publish the stock image's `agentscm serve` port. PostgreSQL TLS is not available in this build, which is one reason this selected pilot uses local SQLite.

### Stock container gap

The stock Dockerfile is not the selected Community workspace runtime. Its default command is `agentscm --data-dir /data serve --bind 0.0.0.0:7331 --public --max-concurrency 1`, which exposes the unauthenticated read-only native source API, not `agentscm hub serve`. Its runtime layer installs Git only. It does not install a Docker CLI or Claude Code, configure a local Docker socket, mount workspace/execution configuration, provision collector keys/tokens/state, or supervise trusted-local collection.

The general Hub Compose profiles exercise coordination Hub storage, but they do not qualify this combined task-only native-store/collector layout. Running the stock image with a replaced entrypoint would still require reviewed mounts, credentials, runtime binaries and host qualification. No container or packaging readiness is claimed for this phase; use the native-process profile above until a container mode is deliberately prepared and tested.

## Container process

The image build toolchain and base images are pinned in the [Dockerfile](../Dockerfile). This section describes only its default read-only native `serve` process. It runs as numeric user and group `10001`. It contains no shell credentials, model configuration, or mounted host container socket. Its default command opens the store, creating missing source directories, and serves on `0.0.0.0:7331` inside the container; the image passes `--public`, which `serve` requires for any non-loopback bind. Container service concurrency is one: a source request can temporarily allocate the store's 64 MiB maximum object before applying the 8 MiB HTTP response cap, and a diff uses bounded in-memory maps. The work slot stays held while its response body is transmitted or retained by the server, so a slow reader cannot start more bounded storage work. Increase concurrency only with a corresponding memory budget and measured workload.

```sh
docker build -t agentscm .
docker run --rm \
  --read-only \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  --tmpfs /tmp:rw,nosuid,nodev,noexec,size=16m \
  -p 127.0.0.1:7331:7331 \
  -v agentscm-data:/data \
  agentscm
```

Compose applies the same loopback publication, read-only root filesystem, dropped capabilities, non-root user, durable named volume, and alpha resource limits:

```sh
docker compose up --build
```

The release workflow publishes `ghcr.io/counterbranch/agentscm:<version>` for each release tag, with a build-provenance attestation and no `latest` tag during alpha (see [verifying images](../SECURITY.md#verifying-images)); you can also build the image yourself as above. CI builds the image and runs a smoke test (`.github/docker-smoke.sh`) on every change. Before a release, refresh base-image digests in a reviewed pull request, inspect the base image's platform manifests and current security advisories, rebuild, and run the native and container smoke tests. Digest pinning fixes the selected base bytes; it does not by itself prove a reproducible application build.

Stop the process and every native, collector, Git/mirror and publication writer before a filesystem copy. The selected Community workspace pilot now has one qualified procedure: a cold same-host restore to the same absolute paths, with every inventoried SQLite database checkpointed and closed, a private path/type/mode/digest manifest, old roots isolated, preflight before constructors run, and post-snapshot revocations reapplied from an independent denial ledger before ingress opens. Follow the exact [operations and recovery checklist](advanced/community-workspace.md#operations-and-recovery). This is synthetic single-host evidence, not online backup, relocation, automatic failover, HA, a live forge or real-runtime qualification. Internal native recovery still refuses retained attachments; Git exports remain source interchange rather than backups of authority, approval or workflow state. Retention leases are not implemented.

## Native process (contributors)

Build with the pinned Rust toolchain, initialize a durable directory, then start the read-only service:

```sh
cargo build --release --locked
./target/release/agentscm --data-dir /srv/agentscm init
./target/release/agentscm --data-dir /srv/agentscm serve --bind 127.0.0.1:7331
```

The `serve` command serves a read-only JSON API under `/v1/*` plus `/health`, and has no web interface. The coordination hub (`agentscm hub serve`) serves a separate web UI at `/ui/`; it is built into the container image and the release binaries (see [the status page](hub.md#status-page)).

The loopback default keeps the unauthenticated API local. `serve` refuses a non-loopback `--bind` unless you also pass `--public`, as `hub serve` does. With `--public`, source reads are exposed to that network, so the service needs an operator-managed authenticated proxy and transport security. The CLI remains available while the service runs. SQLite serializes authority writes; run administrative CLI commands against local durable storage.

## Topology and limits

Run exactly one writable AgentSCM authority for a repository data directory. A Compose replica count above one or several containers sharing `/data` is not a supported availability or write-scaling design. A container packages the process; it does not add database consistency, fencing, failover, authentication, or sandboxing.

The process limits concurrent source operations and raw query size, but it does not set application-level limits for total open connections, request-header bytes, request-read deadlines, or response-write deadlines. Keep the service on loopback. Any explicit network exposure needs an operator-managed proxy that supplies those connection and transport controls as well as authentication and TLS.

Later deployment tiers separate bounded source-read caches and untrusted review workers from a fenced PostgreSQL authority. Those workers may scale independently, but they cannot hold authority credentials or decide current integration eligibility from stale state. That topology, authenticated native multi-user APIs, quotas, collection, online backup and failover qualification are not provided by this profile. Experimental local container checks have their own [execution boundary and qualification limits](advanced/execution.md).
