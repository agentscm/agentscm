# AgentSCM portable evidence verifier profile v1

Status: normative input for an independent portable verifier implementation.

This profile defines byte-level parsing, canonicalization, signature verification, registry evaluation, time evaluation, and artifact assessment for version 1 portable evidence. It is intentionally sufficient to implement a verifier without reading or copying an existing parser. A conforming verifier MUST reuse a reviewed Ed25519 implementation; it MUST NOT implement Ed25519 arithmetic itself. The implementation package must pin its dependency graph and retain applicable licenses, notices, and an SBOM. Those packaging requirements are outside this data-format profile.

The verifier is descriptive. It MUST NOT perform policy admission, authorize execution, dispatch work, grant signing authority, or treat a successful signature as proof that execution occurred. Registry authentication and installation are caller responsibilities.

The words MUST, MUST NOT, SHOULD, and MAY are normative.

## 1. Inputs and outputs

A verification call has four inputs:

1. `envelope_bytes`: raw UTF-8 JSON bytes for one evidence envelope.
2. `registry`: one already selected issuer-registry JSON value. The verifier validates and canonicalizes it, but selection does not authenticate it.
3. `clock`: either `known(unix_ms)` or `uncertain`.
4. `supplied_artifacts`: zero or more pairs of an artifact ID and one of `full(bytes)`, `partial(bytes)`, `redacted`, or `unavailable`.

A structurally invalid envelope, registry, clock, or supplied-artifact list is an error and produces no verification report. A structurally valid but untrusted, stale, expired, unavailable, or cryptographically invalid input produces a report with independent status dimensions.

The report contains:

- `integrity`: `verified`, `unknown_key`, `invalid_public_key`, `non_canonical_public_key`, `weak_public_key`, or `invalid_signature`.
- `registry_binding`: `current` or `stale`.
- `current_issuer_trust`: `trusted`, `unknown_issuer`, `unknown_key`, `future_issued`, `not_yet_valid`, `expired`, `revoked`, or `clock_uncertain`.
- `freshness`: `fresh`, `future_issued`, `expired`, or `clock_uncertain`.
- `execution_claim`: `controller_observed_assertion`, `worker_assertion`, `unqualified`, or `fixture`.
- `artifacts`: assessments sorted by artifact ID.
- `evaluated_clock`: the exact supplied clock.
- `evaluated_registry_sha256`: the lowercase SHA-256 of the canonical registry bytes.
- `envelope_sha256`: the lowercase SHA-256 of the canonical envelope bytes.
- a verified receipt only when `integrity == verified`. It contains the canonicalized payload and `envelope_sha256`.

Every fixture in `vectors.json` uses an `execution_basis` of `fixture` or `unqualified`. No vector is evidence of qualified execution.

## 2. JSON decoding

Raw envelope and raw registry inputs MUST be at most 262144 bytes each. The limit applies before JSON parsing and includes whitespace.

Inputs MUST be valid RFC 8259 JSON encoded as UTF-8. Each object defined by this profile is closed: unknown object members MUST be rejected. Duplicate object member names MUST be rejected, including duplicate members whose values happen to be equal. Required members MUST be present. The optional members listed below MAY be absent or explicitly `null`, with the normalization rules below. JSON numbers used by integer fields MUST use integer token syntax: only an optional minus sign followed by decimal integer digits is syntactically eligible. Because every profile integer is unsigned, a minus sign is then rejected. Decimal points and exponent markers are always rejected, including `1.0` and `1e0` even though they are mathematically integral. The resulting digit sequence must fit the declared JSON target type and the narrower semantic bounds below.

String limits are measured in UTF-8 bytes, not Unicode scalar values. Equality and duplicate detection are exact, case-sensitive string equality. Sorting is ascending Rust-string order, equivalently lexicographic unsigned UTF-8 byte order for valid UTF-8.

### 2.1 Common scalar rules

`identifier` is 1 through 128 UTF-8 bytes, but every byte MUST be ASCII and one of `A-Z`, `a-z`, `0-9`, `-`, `_`, `.`, `:`, or `/`.

`text` is 1 through 2048 UTF-8 bytes and MUST NOT contain U+0000. Other Unicode and control characters are permitted and are escaped by canonical JSON as specified below.

`digest32` is exactly 64 lowercase hexadecimal characters. `signature64` is exactly 128 lowercase hexadecimal characters. Uppercase hexadecimal is invalid.

Unless a narrower type is stated, every `u64` field MUST also be at most 9223372036854775807. A field described as positive MUST additionally be greater than zero. A field described as non-negative MAY be zero.

## 3. Evidence schema

Field order in this section is the canonical JSON member order.

### 3.1 Envelope

```text
EvidenceEnvelope {
  version: u32,
  algorithm: "ed25519",
  payload: EvidencePayload,
  signature_hex: signature64
}
```

`version` MUST equal 1. No other signature algorithm is accepted.

### 3.2 Payload

```text
EvidencePayload {
  version: u32,
  requirement_id: identifier,
  check_kind: CheckKind,
  subject: CheckSubject,
  installed_policy_sha256: digest32,
  installed_policy_epoch: non-negative signed-64 u64,
  issuer_registry_sha256: digest32,
  dependency_snapshot_sha256?: digest32,
  attempt_id: identifier,
  run_generation: positive signed-64 u64,
  request_sha256: digest32,
  limits_sha256: digest32,
  terminal_record_sha256: digest32,
  issuer_id: identifier,
  key_id: digest32,
  issued_at_ms: non-negative signed-64 u64,
  expires_at_ms: positive signed-64 u64,
  terminal_state: TerminalState,
  recovery_state: RecoveryState,
  outcome: EvidenceOutcome,
  execution_basis: ExecutionBasis,
  qualification_claim?: QualificationClaim,
  coverage: CoverageClaim,
  dependencies: [DependencyClaim],
  artifacts: [ArtifactRecord],
  output: OutputCommitment
}
```

The optional payload members `dependency_snapshot_sha256` and `qualification_claim` accept either an absent input member or an explicit JSON `null`; both decode to absence and are omitted from canonical JSON. They MUST NOT be serialized as `null`. `expires_at_ms` MUST be greater than `issued_at_ms`.

`CheckKind` is exactly one of `source_paths/v1`, `command/v1`, or `model_review/v1`.

`TerminalState` is exactly one of `success`, `failed`, `incomplete`, `cancelled`, or `indeterminate`.

`RecoveryState` is exactly one of `not_required`, `recovered`, `cancelled`, or `uncertain`.

`EvidenceOutcome` is exactly one of `pass`, `fail`, `incomplete`, or `unavailable`.

`ExecutionBasis` is exactly one of `controller_observed`, `worker_asserted`, `unqualified`, or `fixture`.

If `recovery_state == uncertain`, `terminal_state` MUST be `incomplete` or `indeterminate`. `outcome == pass` if and only if `terminal_state == success`. No other terminal/outcome combinations are prohibited.

### 3.3 Check subject and change ID

```text
CheckSubject {
  repository: identifier,
  change: ChangeId,
  change_version: positive signed-64 u64,
  context_digest: digest32,
  revision: digest32,
  recipe_sha256: digest32,
  toolchain_sha256: digest32,
  environment_sha256: digest32
}

ChangeId {
  repository: identifier,
  id: identifier
}
```

`subject.repository` MUST equal `subject.change.repository`.

### 3.4 Qualification, coverage, dependencies, artifacts, and output

```text
QualificationClaim {
  kind: identifier,
  profile: identifier,
  pin_sha256: digest32
}

CoverageClaim {
  scope: [text],
  expected_count: u32,
  observed_count: u32,
  inventory_complete: boolean,
  omissions: [text]
}
```

`scope` and `omissions` each contain at most 128 entries and MUST have no duplicates. `observed_count` MUST be no greater than `expected_count`. When `inventory_complete` is true, the counts MUST be equal and `omissions` MUST be empty. Canonicalization sorts `scope` and `omissions` independently.

```text
DependencyClaim {
  identity: identifier,
  state: DependencyState
}
```

There are at most 128 dependency claims, with unique `identity` values. Canonicalization sorts claims by `identity`. `DependencyState` is an internally tagged object. Its canonical member order and forms are:

```json
{"state":"declared"}
{"state":"resolved","revision":"<digest32>"}
{"state":"observed","revision":"<digest32-or-null>","evidence_digest":"<digest32>"}
{"state":"unknown"}
{"state":"unavailable","reason":"<text>"}
```

For `observed`, a missing input `revision` and an explicit JSON `null` both decode as absence, but canonical JSON MUST contain `"revision":null` when absent. This differs deliberately from the omit-on-absence option fields. No other state members are optional.

```text
ArtifactRecord {
  artifact_id: identifier,
  media_type: text,
  sha256: digest32,
  bytes: non-negative signed-64 u64,
  claimed_availability: ArtifactAvailability,
  expires_at_ms?: positive signed-64 u64
}
```

`ArtifactAvailability` is exactly one of `full`, `partial`, `redacted`, or `unavailable`. There are at most 64 artifact records, with unique `artifact_id` values. Canonicalization sorts them by `artifact_id`. An artifact expiry, when present, MUST be greater than the evidence `issued_at_ms`. The optional expiry is omitted from canonical JSON when absent.

Artifact `expires_at_ms` accepts either an absent input member or explicit JSON `null`; both decode to absence and are omitted from canonical JSON.

An artifact whose ID is exactly `record` is mandatory. Its `sha256` MUST equal `terminal_record_sha256`. An artifact named `stdout`, if present, MUST have the same `sha256` and `bytes` as the corresponding output fields. The same rule applies to an artifact named `stderr`. `stdout` and `stderr` artifacts are not otherwise required.

```text
OutputCommitment {
  stdout_sha256: digest32,
  stdout_bytes: non-negative signed-64 u64,
  stderr_sha256: digest32,
  stderr_bytes: non-negative signed-64 u64,
  truncated: boolean
}
```

## 4. Canonical JSON

Canonical payload, envelope, and registry encodings are compact UTF-8 JSON with:

- exactly the member order defined by the schemas in this profile;
- no insignificant whitespace;
- arrays retained in input order except for the explicitly sorted set-like arrays;
- lowercase literals `true`, `false`, and `null`;
- base-10 integers with no leading zeroes;
- strings surrounded by double quotes; `\"` for quotation mark, `\\` for backslash, `\b`, `\t`, `\n`, `\f`, and `\r` for those five controls, and lowercase `\u00xx` for the remaining U+0000 through U+001F controls; and
- all other valid Unicode encoded directly as UTF-8, without ASCII-only escaping. Solidus `/`, `<`, `>`, `&`, U+2028, and U+2029 are not specially escaped.

Canonical evidence payload and envelope bytes MUST each be at most 262144 bytes. Canonical registry bytes MUST be at most 262144 bytes. Validation occurs before hashing or signing.

Canonical payload sorting is limited to:

- `coverage.scope` ascending;
- `coverage.omissions` ascending;
- `dependencies` ascending by `identity`; and
- `artifacts` ascending by `artifact_id`.

Canonical envelope bytes contain the canonicalized payload. The signature is not recalculated during canonicalization. `envelope_sha256` is SHA-256 over the complete canonical envelope, including `signature_hex`.

## 5. Issuer registry

### 5.1 Schema and limits

```text
IssuerRegistrySnapshot {
  version: u32,
  repository: identifier,
  epoch: non-negative signed-64 u64,
  issuers: [IssuerRecord]
}

IssuerRecord {
  issuer_id: identifier,
  trust_domain: identifier,
  common_control: CommonControl,
  allowed_check_kinds: [identifier],
  keys: [IssuerKeyRecord]
}
```

Registry `version` MUST equal 1. There are at most 128 issuers, and issuer IDs MUST be unique. Across the complete registry, key IDs MUST be unique and public-key byte strings MUST be unique. This rejects issuer aliases that reuse a key even if their issuer IDs, trust domains, or control roots differ.

There are at most 64 allowed check kinds per issuer, with no duplicates. There are at most 8 keys per issuer. Both arrays may be empty. Allowed check kinds are identifiers and are not restricted to the three evidence check kinds because registry metadata may describe other systems.

`CommonControl` is an internally tagged object in one of these canonical forms:

```json
{"state":"known","roots":["<identifier>"]}
{"state":"unknown"}
```

A known-control record has 1 through 16 unique roots. An unknown-control record has no `roots` member.

```text
IssuerKeyRecord {
  key_id: digest32,
  public_key_hex: 32 bytes of lowercase hexadecimal,
  not_before_ms: non-negative signed-64 u64,
  not_after_ms: positive signed-64 u64,
  revoked_at_ms?: non-negative signed-64 u64
}
```

`not_after_ms` MUST be greater than `not_before_ms`. No ordering constraint is placed on `revoked_at_ms`; current trust evaluation below gives it meaning. `revoked_at_ms` accepts either an absent input member or explicit JSON `null`; both decode to absence and are omitted from canonical JSON.

Each `key_id` MUST equal lowercase SHA-256 of:

```text
UTF8("agentscm.ed25519-key/v1") || 0x00 || public_key_bytes
```

This derivation validates only encoding, length, and identifier binding. Cryptographic key validation happens during integrity verification.

Registry canonicalization sorts:

- known common-control roots ascending;
- allowed check kinds ascending;
- keys ascending by `key_id`; and
- issuers ascending by `issuer_id`.

No other arrays are sorted. The registry digest is lowercase SHA-256 of canonical registry bytes.

### 5.2 Common-control independence utility

Registry parsing and hashing MUST retain all common-control data. A verifier MAY expose the following pure utility, but its result is not policy admission.

Given at most 128 selected issuer IDs and one identifier-valued check kind:

1. Reject an invalid selected ID or check-kind identifier.
2. Build an undirected control graph across every issuer in the registry, including unselected issuers. Two issuers share control when their `trust_domain` values are equal, or when both have known control and their root sets intersect. Connected components are transitive.
3. Process selected IDs in order. A repeated selected ID is recorded as a duplicate and is otherwise ignored after its first occurrence. An unknown ID is recorded as unknown. A known issuer is recorded as disallowed if its allowed-check-kind set lacks the requested kind. It is recorded as unknown-control if its control value is unknown.
4. Only known issuers with known control that allow the kind are eligible. Group eligible selected issuers by the full-registry connected component.
5. Sort IDs within groups, sort groups lexicographically, and sort each diagnostic list. Deduplicate the duplicate-ID diagnostic list.

The independent count is the number of eligible groups. Shared key IDs or key bytes never reach this utility because they invalidate the registry.

## 6. Signature construction and strict verification

The signature domain is the 29 ASCII bytes `agentscm.portable-evidence.v1`.

The signed message is exactly:

```text
u64be(29)
|| UTF8("agentscm.portable-evidence.v1")
|| u64be(length(canonical_payload_bytes))
|| canonical_payload_bytes
```

Both lengths are unsigned 64-bit big-endian integers. No NUL terminator, newline, JSON wrapper, digest substitution, or key ID is added to this framing. The payload already carries its `key_id` and registry binding. The key ID domain is separate and includes a NUL byte as stated above.

The signature is Ed25519 over the full framed message and is encoded as 64 lowercase hexadecimal bytes. A signing operation MUST first validate the payload, derive the supplied signing key's public key and key ID, and reject a payload whose `key_id` differs.

A verifier MUST use a reviewed Ed25519 primitive with strict verification behavior equivalent to RFC 8032 Ed25519 verification plus the following ordered public-key checks:

1. Resolve an exact issuer ID and key ID in the validated registry. If either lookup fails, integrity is `unknown_key`; do not attempt verification.
2. Decode exactly 32 lowercase-hex public-key bytes.
3. Treat the high bit of byte 31 as the Edwards x-sign bit. With that bit cleared, interpret the remaining bytes as a little-endian y-coordinate. If y is greater than or equal to `2^255 - 19`, integrity is `non_canonical_public_key`.
4. Ask the reviewed primitive to decode the verifying key. Failure is `invalid_public_key`.
5. Reject small-order/weak verifying keys as `weak_public_key`.
6. Decode the signature and strictly verify it over the framed message. Any failure, including non-canonical signature scalar behavior rejected by the primitive, is `invalid_signature`.
7. Otherwise integrity is `verified` and a verified receipt may be constructed.

The portable implementation MUST NOT copy curve arithmetic from another implementation. Its locked manifest and notices must make the reused primitive reviewable. `vectors.json` contains real Ed25519 signatures produced and verified by the recorded library, but those checks are not Rust interoperability results.

## 7. Verification semantics

### 7.1 Repository and registry binding

After envelope and registry validation, `payload.subject.repository` MUST equal `registry.repository`; a mismatch is an error.

Compute the current registry digest. `registry_binding` is `current` when it equals `payload.issuer_registry_sha256`, otherwise `stale`. Staleness does not suppress signature verification, current trust evaluation, freshness, or artifact assessment.

### 7.2 Clock and current issuer trust

A known clock MUST be a non-negative signed-64 value. An uncertain clock has no numeric value.

If the issuer ID is absent, current trust is `unknown_issuer`. Else if the key ID is absent for that issuer, it is `unknown_key`. These two results take precedence over clock uncertainty. For a known issuer and key with an uncertain clock, current trust is `clock_uncertain`.

For a known clock, apply the first matching rule in this order:

1. `revoked` if `revoked_at_ms` exists and is less than or equal to `now_ms`. Current revocation is authoritative even for a receipt issued earlier.
2. `future_issued` if evidence `issued_at_ms > now_ms`.
3. `not_yet_valid` if evidence `issued_at_ms < key.not_before_ms` or `now_ms < key.not_before_ms`.
4. `expired` if evidence `issued_at_ms >= key.not_after_ms` or `now_ms >= key.not_after_ms`.
5. `trusted` otherwise.

This status describes the selected installed registry at evaluation time. It does not establish registry authenticity or signature validity.

### 7.3 Evidence freshness

For an uncertain clock, freshness is `clock_uncertain`. For a known clock:

- `future_issued` when `issued_at_ms > now_ms`;
- otherwise `expired` when `expires_at_ms <= now_ms`;
- otherwise `fresh`.

Freshness is independent of key lifetime, revocation, registry binding, integrity, and artifact status.

### 7.4 Execution claim

Map the signed `execution_basis` without upgrading it:

- `controller_observed` -> `controller_observed_assertion`;
- `worker_asserted` -> `worker_assertion`;
- `unqualified` -> `unqualified`;
- `fixture` -> `fixture`.

Even `controller_observed_assertion` remains a signed claim. It is not execution proof.

### 7.5 Supplied artifact validation

There may be at most 64 supplied artifact entries. IDs are identifiers, MUST be unique, and MUST name an artifact committed by the payload. Sum the byte lengths of all `full` and `partial` supplied values with overflow checking. The total MUST be at most 67108864 bytes. Redacted and unavailable inputs contribute zero bytes.

For each committed artifact, in artifact-ID order:

1. If it has an expiry and a known clock is at or after that expiry, status is `expired` and `commitment_matches` is false.
2. Else if it has an expiry and the clock is uncertain, status is `unavailable` and `commitment_matches` is false.
3. Else assess supplied data. Full or partial bytes match only when both their exact byte length and SHA-256 match the record. A mismatch has status `hash_mismatch`; a matching full input starts as `full`; a matching partial input starts as `partial`; redacted starts as `redacted`; unavailable or absent starts as `unavailable`.
4. A `hash_mismatch` is never hidden by the claimed availability. Otherwise constrain the status: claimed `full` preserves the observed status; claimed `partial` maps observed full or partial to partial; claimed `redacted` maps every non-mismatch status to redacted; claimed `unavailable` maps every non-mismatch status to unavailable.

For step 4, `commitment_matches` retains the byte comparison result even when a claim constrains the displayed status to redacted or unavailable.

## 8. Independent dimensions and authority limits

The verifier MUST calculate report dimensions independently after structural validation. In particular:

- A valid signature can coexist with stale registry binding, current revocation, expired evidence, unqualified or fixture execution basis, missing artifacts, or artifact hash mismatch.
- Invalid signature can coexist with currently trusted registry metadata and fresh timestamps.
- A verified receipt means only that the strict signature matches the canonical payload under a registry-resolved key. It does not mean the registry is current or authenticated, the key is currently trusted, the receipt is fresh, artifacts are available, claims are true, coverage is complete, dependencies are usable, or policy requirements are satisfied.
- Artifact bytes and availability never change signature integrity.
- Registry common-control and allowed-check-kind metadata are parsed and hashed but do not alter integrity. Their policy use belongs to a separate admission layer.

A consumer may establish conditional truth only by explicitly checking every dimension its decision requires against caller-authenticated inputs. The portable verifier MUST expose facts and MUST NOT collapse them into an `accepted`, `approved`, `qualified`, or equivalent policy verdict.

## 9. Conformance vectors

`vectors.json` is part of this profile. Hex fields and canonical JSON text are exact. A conforming implementation should, at minimum:

- reproduce every canonical payload, registry, signature message, signature, and canonical-envelope digest;
- normalize absent and explicit-null option inputs exactly as specified, including the deliberately different `observed.revision` canonical `null` rule;
- accept all positive signed fixtures and match their expected independent dimensions;
- reject every structural negative before producing a report;
- match report cases for tampering, unknown keys, stale or revoked registry snapshots, known and uncertain clocks, and all artifact states; and
- match registry validation, canonicalization, key-alias rejection, and common-control independence cases.

The vector generator contains fixed, published synthetic seeds. They are test material and MUST NOT be used as credentials or production keys.
