# Deployment

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

AgentSCM runs as one OCI container on the host where your agents run, with the agents' checkout mounted into it. The container uses an on-disk source store and `state.sqlite` authority under the selected data directory. Building the native binary is for contributors. This page is about the read-only `serve` API; to run the coordination hub (`agentscm hub serve`), see [running the hub in containers](hub.md#running-in-containers).

## Container process

The image builds with Rust 1.96.0 and runs as numeric user and group `10001`. It contains no shell credentials, model configuration, or mounted host container socket. Its default command opens the store, creating missing source directories, and serves on `0.0.0.0:7331` inside the container; the image passes `--public`, which `serve` requires for any non-loopback bind. Container service concurrency is one: a source request can temporarily allocate the store's 64 MiB maximum object before applying the 8 MiB HTTP response cap, and a diff uses bounded in-memory maps. The work slot stays held while its response body is transmitted or retained by the server, so a slow reader cannot start more bounded storage work. Increase concurrency only with a corresponding memory budget and measured workload.

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

Stop the process before taking a filesystem-level copy of the whole volume unless the backup method provides a consistent SQLite snapshot and source-object view. There is no online backup/restore command, retention leases, or backup verification.

## Native process (contributors)

Build with the pinned Rust toolchain, initialize a durable directory, then start the read-only service:

```sh
cargo build --release --locked
./target/release/agentscm --data-dir /srv/agentscm init
./target/release/agentscm --data-dir /srv/agentscm serve --bind 127.0.0.1:7331
```

The `serve` command serves a read-only JSON API under `/v1/*` plus `/health`, and has no web interface. The coordination hub (`agentscm hub serve`) serves a separate read-only status page at `/ui/`; it is built into the container image and the release binaries (see [the status page](hub.md#status-page)).

The loopback default keeps the unauthenticated API local. `serve` refuses a non-loopback `--bind` unless you also pass `--public`, as `hub serve` does. With `--public`, source reads are exposed to that network, so the service needs an operator-managed authenticated proxy and transport security. The CLI remains available while the service runs. SQLite serializes authority writes; run administrative CLI commands against local durable storage.

## Topology and limits

Run exactly one writable AgentSCM authority for a repository data directory. A Compose replica count above one or several containers sharing `/data` is not a supported availability or write-scaling design. A container packages the process; it does not add database consistency, fencing, failover, authentication, or sandboxing.

The process limits concurrent source operations and raw query size, but it does not set application-level limits for total open connections, request-header bytes, request-read deadlines, or response-write deadlines. Keep the service on loopback. Any explicit network exposure needs an operator-managed proxy that supplies those connection and transport controls as well as authentication and TLS.

Later deployment tiers separate bounded source-read caches and untrusted review workers from a fenced PostgreSQL authority. Those workers may scale independently, but they cannot hold authority credentials or decide current integration eligibility from stale state. That topology, authenticated APIs, hostile-code isolation, quotas, collection, backup/restore, and failover qualification are not provided by this version.
