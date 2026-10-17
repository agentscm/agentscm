# AgentSCM portable evidence verifier

This standalone Rust package implements the AgentSCM portable evidence profile
v1. It parses closed JSON schemas, produces the profile's canonical JSON,
verifies Ed25519 signatures with `ed25519-dalek`, evaluates registry and clock
state, and assesses supplied artifact commitments. Its outputs are descriptive
facts. Registry selection and authentication remain caller responsibilities.

The package has no AgentSCM workspace or first-party crate dependency. It does
not sign, discover keys, use credentials, access a network, dispatch work, or
make policy-admission decisions. Files are the only CLI inputs.

## Library interface

The primary call is:

```rust,ignore
pub fn verify(
    envelope_bytes: &[u8],
    registry_bytes: &[u8],
    clock: Clock,
    supplied_artifacts: &[SuppliedArtifact],
) -> Result<VerificationReport, VerifyError>
```

`VerificationReport` keeps signature integrity, registry binding, current key
trust, evidence freshness, the signed execution assertion, and artifact status
as separate dimensions. Its fields and the fields of `VerifiedReceipt` are
opaque; read-only accessors expose results without allowing callers to construct
or mutate verification state. A verified receipt appears only for a strictly
verified signature and contains the canonical payload plus the canonical
envelope hash. Its payload digest is captured from the already validated,
byte-limited canonical payload. It does not upgrade any other dimension.

`assess_independence` implements the optional common-control utility from the
profile. It uses the full validated registry graph and returns groups and
diagnostics; it is not an admission decision.

## CLI

```text
agentscm-portable-verifier verify \
  --envelope envelope.json \
  --registry registry.json \
  --clock-known 2000 \
  --artifact-full record terminal-record.json \
  --artifact-redacted stdout
```

Use `--clock-uncertain` when no trustworthy numeric clock is available. Artifact
options are `--artifact-full ID PATH`, `--artifact-partial ID PATH`,
`--artifact-redacted ID`, and `--artifact-unavailable ID`.

Top-level `--help`, `-h`, `--version`, and `-V` reject trailing arguments.

The CLI reads every file through a byte-limited reader rather than trusting file
metadata. Envelope and registry inputs are each limited to 262,144 bytes; full
and partial artifacts share a 67,108,864-byte limit. Successful standard output
is one bounded JSON summary. It reports hashes and artifact assessments without
echoing envelope, payload, registry, or artifact bytes. Failures write one JSON
object with a fixed error code and bounded message to standard error.

## Protocol and conformance material

The normative profile and vectors are packaged under
`protocol/evidence-v1/`. The integration conformance suite covers every signed
fixture, canonical payload/envelope/registry bytes and hashes, framed signature
messages, report and artifact cases, structural and registry negatives, the
primitive self-check, strict public-key classifications, and common-control
independence. Focused negatives also require closed decoding for empty
internally tagged dependency, common-control, and clock states.

Process-level CLI cases use the packaged vectors to cover a verified report
with a supplied artifact, a descriptive invalid-signature report that still
exits successfully, bounded JSON errors for malformed and unknown nested input,
the raw JSON byte cap, and the absence of source or artifact content in summary
output. They use uniquely named temporary directories and remove only the files
they create.

Rust 1.96 and edition 2024 are required. The dependency graph is pinned by
`Cargo.lock`; the intended validation commands after a build slot is available
are:

```text
cargo fmt --check
cargo test --locked --offline
cargo clippy --all-targets --locked --offline -- -D warnings
```

No compiled or test-pass claim is made by this source package alone.

## Packaging

`Cargo.toml` explicitly includes the protocol files, lockfile, SBOM, notices,
dependency inventory, and upstream license files with their original and shipped
hash inventory. `DEPENDENCIES.md` distinguishes the active feature tree from
Cargo's complete lockfile closure.

The license inventory describes notice bytes. It is not an archive or binary
checksum manifest. Cargo may normalize source-package metadata; distribution
must hash the final artifact and verify its extracted inventory separately.
