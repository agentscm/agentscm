---
title: "Installed policy assessment"
---

# Installed policy assessment

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

The `policy` library evaluates bounded, operator-installed requirements against
supplied current records. It returns `satisfied`, `not_satisfied`, or
`indeterminate`, with typed issues and hashes of its inputs. Its basis is always
`supplied_records_only`. A result is ordinary descriptive data. It grants no
dispatch, approval, integration, signing, or retention permission, and the CLI
and authority do not yet use it as an admission gate.

The first profile uses unconditional exact pins. Every installed check specifies
its requirement ID, check kind, recipe, toolchain and environment digests,
permitted issuers, minimum independent issuer count, required scope and artifacts,
maximum age, and required execution basis. The policy also specifies required
dependency revisions and evidence bases, complete-inventory requirements, and a
minimum retention interval. The installed repository, target and epoch must
match the review context. All installed checks remain required when the
candidate omits them. Unknown or mismatched candidate declarations produce
issues rather than reducing the installed minimum.

This exact-pin profile does not derive arbitrary per-revision recipes from a
general policy language. A trusted recipe-template installation and derivation
flow remains required for that use case. Candidate-authored templates and
waivers have no special authority here.

## Current records and their owner

Evaluation requires a review context, issuer registry, complete current-attempt
inventory, current dependency snapshot, installed qualification catalogue,
explicit clock, and [verified evidence reports](evidence.md). Snapshot decoding
and canonicalization are bounded to 256 KiB; unknown fields, duplicate entries,
unsupported versions and malformed identities fail. Decoding authenticates none
of these administrative records.

The durable authority must authenticate their installation and obtain current
records under its repository, change, context, policy, authorization, publication
and retention fences. It must validate native collector-record semantics before
publishing a current attempt's subject and terminal-record hash. This library
checks a signed record's artifact bytes and compares that hash with the supplied
current attempt; it does not independently execute or interpret every native
collector format. A hash match alone establishes no execution truth.

Each selected receipt must match the installed subject, requirement, policy,
current registry, attempt ID, run generation, request, limits, terminal record,
and canonical evidence-envelope hash. Current failure, cancelled or recovered
state, stale verification, incomplete inventory, unavailable artifacts, missing
retention, expired evidence and uncertain time cannot satisfy the requirement.
Verification reports must use the exact evaluation clock and registry. An old
signature cannot bypass a current key revocation.

The pure attempt planner defines pending, running and terminal transitions with
expected version and run-generation fences. Every accepted transition advances
the version. Claim, cancellation and recovery advance the run generation;
completion preserves the claimed run generation so its receipt still identifies
the executed work. Completion needs a live lease and known clock; cancellation and
indeterminate recovery can stop unsafe work with an uncertain clock. Plans are
neither serialized capabilities nor durable transactions. The existing built-in
source check advances its lifecycle generation on terminal publication; adapting
that older record format requires an explicit authority mapping or migration.
Its terminal generation cannot be treated as the claimed run identity used by
this planner. Persistence, unique
dispatch reservations, aggregate budgets, cancellation races, lost-reply
recovery and late-finisher rejection remain the authority's responsibility.

## Issuers and qualification

Registry keys have algorithm-bound SHA-256 IDs derived from their public bytes.
Aliases cannot register the same key twice. Issuer trust domains and known
common-control roots form connected components across the complete registry,
including unselected intermediaries. Selected issuers in one component count
once. Unknown control cannot establish independence. These installed
administrative facts do not prove actual organizational independence.

Source-integrity requirements accept a controller-observed assertion only when
all other checks succeed. Command and model requirements additionally need the
exact qualification assertion and an installed, current qualification reference
permitting the issuer. Missing, mismatched, revoked, expired and not-yet-valid
qualification references are separate issues. The authority must validate and
retain the underlying qualification proof. A signed fixture, worker assertion,
or unqualified execution cannot satisfy a qualified-execution requirement.
This version performs no live executor or model qualification.

## Dependencies, coverage and availability

Required dependency identities must have the expected current immutable
revision. Snapshot generations contribute to their digest, so restoring the
same revision under a different generation invalidates an old snapshot binding.
Declared, resolved, observed, unknown and unavailable states remain distinct.
An observed minimum additionally requires a selected signed receipt containing
the exact observed dependency revision and a matching full artifact commitment
for its observation evidence, with an explicit expiry covering the minimum
retention interval. This is bounded declared coverage, not universal
dependency discovery.

Required scope must be present, output must be untruncated, and complete-coverage
requirements need complete inventories, matching counts and no omissions. Every
required artifact, including the terminal `record`, needs full matching bytes
and an explicit expiry covering the minimum retention interval. The signed
receipt must also cover that interval. A future expiry is a signed commitment;
the authority still has to acquire and enforce the real retention lease.

Unknown required facts produce an indeterminate result; known violations
produce a negative result. Every issue requires escalation in this first
profile. A successful synthetic assessment establishes only consistency of its
supplied records. Authenticated transactional admission, durable recovery,
independent portable verification and live qualification remain separate gates.
