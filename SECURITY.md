# Security policy

AgentSCM is alpha software (`0.1.0-alpha.x`), not for production, and has had no security audit. Only the latest release (or, before the first release, the current `main`) receives security fixes, and responses are best effort.

## Reporting a vulnerability

Report privately through [GitHub private vulnerability reporting](https://github.com/counterbranch/agentscm/security/advisories/new). If you cannot use GitHub, email support@counterbranch.com with the subject "AgentSCM security". Please do not open a public issue or pull request for a suspected vulnerability.

Include the version or commit, how to reproduce it, and the impact you expect. We aim to acknowledge a report within 7 days and to agree a fix and disclosure date with you. We follow a 90-day coordinated disclosure window, shorter if a fix ships sooner and longer only by agreement. This is a small team and the project is alpha; these are goals, not a service-level commitment.

## Scope

In scope: the `agentscm` CLI and container image; the HTTP APIs and the coordination hub (sign-in, agent tokens, intents, forge sync); the GitHub App credentials and webhook handling; PostgreSQL and S3 credentials handling; and the isolated command runner.

Out of scope: vulnerabilities in third-party dependencies with no AgentSCM-specific impact (report those upstream), and findings that need an already-trusted operator or an already-compromised host. The local SQLite profile and the CLI trust their operator by design.

## What is not a security boundary

- Command execution runs a declared command in a hardened container. The test corpus in [docs/advanced/execution.md](docs/advanced/execution.md) covers a fixed list of controls on one runtime build and profile. It does not cover kernel or runc escapes, side channels or other runtimes, and the runner is not a sandbox for arbitrary hostile code.
- The unauthenticated read-only API is meant for loopback. Exposing it on a network needs an authenticating, TLS-terminating proxy you operate.

## Instruction files are security-sensitive

`AGENTS.md`, `CLAUDE.md` and the CI workflows steer automated tools that read this repository. Changes to them get the same review as code, and an agent should not edit them without a maintainer's sign-off.

## Verifying images

Release images are published to `ghcr.io/counterbranch/agentscm` with build-provenance attestations. Verify an image by digest before you run it:

```sh
gh attestation verify oci://ghcr.io/counterbranch/agentscm@sha256:<digest> --repo counterbranch/agentscm \
  --signer-workflow counterbranch/agentscm/.github/workflows/release.yml \
  --source-ref refs/tags/v<version> --deny-self-hosted-runners
```

The three extra flags accept only an attestation made by the release workflow for that tag, not one from any other workflow or branch of the repository.

The digest of each release is in its GitHub release notes. You can also build from source with the pinned toolchain and `--locked`.

## Data the hub stores

What the coordination hub keeps about people, tokens and activity, and what it does not, is described in [Data the hub stores](docs/hub.md#data-the-hub-stores).
