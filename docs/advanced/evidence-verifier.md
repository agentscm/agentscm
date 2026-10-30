---
title: "Portable evidence verifier"
---

# Portable evidence verifier

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

`tools/portable-verifier` is a separate Rust package for inspecting portable
evidence without linking the AgentSCM engine. It implements the
[version 1 profile](../../protocol/evidence-v1/profile.md) and its fixed synthetic
[conformance vectors](../../protocol/evidence-v1/vectors.json). The package retains
its own locked dependencies, SBOM and upstream notices. It shares the reviewed
Ed25519 primitive with the engine; it does not provide a second cryptographic
implementation.

Build and validate the two packages separately with the pinned toolchain:

```sh
cargo test --locked --test integration evidence_conformance::
cargo test --manifest-path tools/portable-verifier/Cargo.toml --locked
cargo clippy --manifest-path tools/portable-verifier/Cargo.toml --locked --all-targets -- -D warnings
cargo build --manifest-path tools/portable-verifier/Cargo.toml --locked --release
```

The engine suite and standalone suite consume identical vectors. The engine
suite also checks that the packaged profile and vectors match the canonical
copies. Passing these fixtures establishes compatibility for their exercised
format cases; it does not establish execution qualification, authenticated
registry installation or admission authority.

Run the standalone executable against explicitly selected local files:

```sh
agentscm-portable-verifier verify \
  --envelope envelope.json \
  --registry registry.json \
  --clock-known 2000 \
  --artifact-full record terminal-record.json
```

Use `--clock-uncertain` when a reliable numeric evaluation time is unavailable.
Full and partial artifacts take a local path; redacted and unavailable artifacts
take only their ID. Envelope and registry inputs are each capped at 256 KiB;
full and partial artifact bytes share a 64 MiB cap. Unknown fields, duplicate
members, invalid identities, inconsistent commitments and unsupported versions
reject before a report is returned. No locator is fetched and no key is
discovered automatically.

A successful command returns a descriptive JSON report. Exit status zero means
the inputs produced that report, including reports with an invalid signature,
revoked key, expired evidence or missing artifacts. Consumers must inspect each
required dimension. Structural or I/O errors return nonzero with a bounded JSON
error on standard error. Reports contain hashes and artifact status without
echoing raw payload or artifact bytes.

Signature integrity, current registry binding, issuer trust, freshness, signed
execution assertions and artifact availability remain separate. A verified
signature authenticates the canonical payload under the selected key; registry
authentication remains the caller's responsibility. The tool grants no policy
approval, runtime qualification or permission to execute or integrate.

The standalone source package includes its protocol material, lockfile,
`sbom.cdx.json`, `LICENSE`, `NOTICE`, `THIRD_PARTY_NOTICES.md`, dependency inventory
and upstream `licenses/` directory. The license inventory identifies the original
and shipped notice bytes; it is not an archive or binary checksum manifest.
Cargo may normalize source-package metadata. Distribution inventories must hash
the final artifact and its extracted contents, identifying the verifier executable
and its own materials separately from the main executable's native bundle.
