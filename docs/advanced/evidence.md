# Portable evidence

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

AgentSCM's portable evidence format carries one bounded, detached Ed25519
signature over a complete check-attempt claim. It is a pure interchange and
verification layer. It does not persist attempts, discover keys, qualify an
executor, grant dispatch, or authorize integration.

Version 1 binds the exact check subject and requirement, check kind, installed
policy digest and epoch, issuer-registry digest, optional dependency-snapshot
digest, attempt and run generation, request, limits, and terminal-record digests,
issuer and key, validity times, typed terminal/recovery/outcome claims, execution basis,
optional qualification assertion, coverage, dependencies, artifact byte
commitments, and output byte commitments. Supported check kinds are
`source_paths/v1`, `command/v1`, and `model_review/v1`.

The canonical signature message is:

```text
u64be(domain length)
|| "agentscm.portable-evidence.v1"
|| u64be(canonical payload JSON length)
|| canonical payload JSON
```

Struct field order is the declaration order. Set-like lists are sorted before
serialization; duplicate entries are rejected. The input and canonical output
caps are 256 KiB. The format permits at most 64 artifact records, 128 dependency
claims, 128 coverage-scope entries, and 128 coverage omissions. Identifiers,
free text, lowercase hexadecimal lengths, and signed-64-bit-compatible times
are validated. Identifiers use the same 1–128 byte ASCII grammar as installed
policy and issuer records: letters, digits, `-`, `_`, `.`, `:`, and `/`.
Unknown fields and unsupported versions or check kinds fail.

Signing requires an explicit `ed25519_dalek::SigningKey`. The library does not
read key files, environment variables, operating-system key stores, worker
state, or random sources. The payload key ID must match the algorithm-bound
fingerprint of the supplied public key.

Verification takes the encoded envelope, a caller-authenticated installed
issuer registry, an explicit `EvaluationClock`, and bounded caller-supplied
artifact bytes or availability states. It rejects malformed, noncanonical, and
weak public keys and uses strict Ed25519 verification. Known expired or revoked
keys can still establish cryptographic integrity while the separate current
issuer-trust result rejects their use. Unknown keys cannot establish integrity.
An uncertain clock never reports current trust or freshness.

The read-only verification report binds the clock and current registry digest
used for evaluation and exposes separate results for signature integrity,
registry staleness, current issuer trust, freshness, execution assertion, and
artifact availability. Its canonical envelope SHA-256 is available even when
the signature is invalid or the key is unknown, so callers can identify the
exact rejected input. A `VerifiedReceipt` is available only after successful
strict verification, has no deserialize implementation or public constructor,
and provides immutable access to its payload.

Artifact availability comes from the supplied bytes or explicit caller state,
constrained by the signed availability claim. Full and partial inputs compare
both byte length and SHA-256 against the signed commitment. Missing bytes do not
become full merely because a signer claimed full availability. Redacted,
unavailable, expired, and hash-mismatched artifacts remain distinct.

Every payload includes a `record` artifact commitment matching the terminal
record digest. If `stdout` or `stderr` artifact records are present, their hashes and byte counts must equal the
corresponding output commitment. A verified receipt also exposes the SHA-256 of
the complete canonical signed envelope for exact selection and retention.

A valid signature proves only that the holder of the selected private key
signed those bytes. It does not prove execution, honesty, isolation, coverage,
qualification, retention, current authorization, or admission. In particular,
signed fixture and unqualified execution bases remain fixture and unqualified
claims. Admission must recompute current policy, registry, dependency, clock,
qualification-catalog, retention, source, and authority facts transactionally.
See [installed policy assessment](policy.md) for the conditional evaluator and
the durable authority's responsibilities.

The separately developed [portable verifier](evidence-verifier.md) implements
the version 1 profile without linking the engine. Both packages consume the same
fixed conformance vectors. Their format checks cover only the exercised cases;
independent client/server exchange, archival compatibility and the remaining stable-protocol
requirements still need separate evidence.
