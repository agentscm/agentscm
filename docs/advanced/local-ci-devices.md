---
title: "Trusted local CI devices"
---

# Trusted local CI devices

> **Experimental (alpha).** Administrator enrollment and a library check exist; nothing consumes the check in production yet. Tested with synthetic keys, an in-process fake forge and one host only.

A trusted local CI device is an Ed25519 key that an administrator has authorized to sign local CI results for one repository on behalf of one person. This page describes the enrollment and authorization state and its limits. It is the device half of [immutable local CI plans](local-ci-plans.md); signing, execution and evidence verification are separate work.

## What is implemented

- **Key and request commands (Unix, hidden, experimental).** `agentscm local-ci device key-generate --key ABS_PATH` creates a raw Ed25519 key (mode 0600, one link, owner-checked, no symlink leaf) and prints only the public key and key ID. `agentscm local-ci device enroll-request --key ABS_PATH --device-id lcd_... --repository HOST/OWNER/NAME --owner-provider P --owner-host H --owner-person ID --generation N` signs an enrollment statement with that key and prints `{"statement", "signature"}` for an administrator. Neither prints private key bytes. Both refuse a key path inside a Git checkout, found by walking the key's own ancestors and the current directory's ancestors for a `.git` entry. A directory that cannot be read is an error, not "no checkout".
- **Administrator enrollment, rotation and revocation.** `POST /v1/hub/admin/local-ci/devices`, `.../{id}/rotate` and `.../{id}/revoke`, and `GET /v1/hub/admin/local-ci/devices?repository=HOST/OWNER/NAME` (see [Hub routes](../hub.md#routes)). Each needs an administrator session. `agentscm hub local-ci-device enroll|rotate|revoke|list` is a thin wrapper that sends the saved `hub login` session to these routes.
- **Owner route.** `GET /v1/hub/local-ci/devices?repository=HOST/OWNER/NAME` lists the signed-in person's own devices on that repository (id, repository label, generation, digest, revocation time). It needs a Hub session or computer credential of that person; an agent token, a read-only token and no credential are refused. An unknown repository gives an empty list, so the route does not reveal which repositories are connected. It changes nothing.
- **Owner identity.** The authorization names the Hub user's identity-provider identity (provider, host, person ID), not a Hub computer. Enrollment requires an enabled Hub user and a current role on the repository, which the Hub asks the forge for; an uncertain answer enrolls nothing.
- **Repository scope, expiry and key possession.** An authorization is for one connected repository, spelled `host/owner/name`, and one check kind. The enrolled key has a `not_after`. The candidate key signs the statement, which proves possession; it does not prove anything else about the device.
- **Generation checks.** Enrollment is generation 1. Rotation and revocation must name the stored generation and produce the next one; a stale generation is refused. A repeated `request_id` with the same content returns the stored result, and with other content is refused; the exact replay is answered before the expiry, owner and role checks, so a retry after a lost response does not need the forge. The repository is still looked up first, because the request fingerprint includes its id, so a retry after a rename or disconnect is refused as an unknown repository. The answer is the device as stored now: a rotation retried after a later revocation returns the revoked device. A key ID is enrolled once ever.
- **Refusals.** 400: `invalid_request` (malformed body or query), `invalid_request_id`, `invalid_expiry` (not in the future, or out of range), `enrollment_invalid` (the statement or signature does not match the repository, owner, device and generation). 404: `unknown_repository`, `unknown_owner`, `unknown_device`. 409: `stale_generation` (with `current_generation`), `device_exists`, `duplicate_key`, `request_conflict`, `device_revoked`, `owner_disabled`, `owner_wrong_forge` (the owner signed in on another host than the repository's), `owner_role_missing`, `repository_renamed`. 503: `role_check_unavailable`.
- **Audit events.** `local_ci_device_enrolled`, `local_ci_device_rotated` and `local_ci_device_revoked` name the administrator who acted, the repository, the generation and the new key IDs, never key material.
- **A library check.** `agentscm::hub::local_ci_devices::authorize_offline_result` decides whether a claimed device, key and generation are current: not revoked, not expired, not rotated away, owner still enabled with a role, and repository label unchanged. Nothing calls it in production yet; issue [#273](https://github.com/agentscm/agentscm/issues/273) will.

Not implemented: a result submission endpoint, any forge publication of device-signed results, a signing service, job execution, and evidence verification.

## Assurance

The assurance is **trusted-device / worker-asserted**. A valid signature means the holder of the enrolled key signed those bytes. It is not proof that anything executed, that it ran as described, or that the clock in the claim is right. Stronger hardware or runtime assurance is out of scope.

## Relationship to Hub computers

A CI device is separate from a Hub computer credential. Revoking a Hub computer does not revoke a CI device key, and revoking a CI device does not sign a computer out. A lost laptop needs both revocations.

## Key handling

Keep the key outside candidate source and outside job execution. `key_location_refused` (`src/local_ci/device.rs`) checks a path against a checkout root, declared mounts and declared environment values, lexically on canonicalized paths. Today the CLI passes only the Git checkouts it finds. No execution profile calls it with mounts or environment yet (issue [#271](https://github.com/agentscm/agentscm/issues/271)). Other processes of the same user on the host can read the key file, and host compromise is an explicit trust limit.

## Platform key storage

No platform keystore is integrated. Evidence version 1 is Ed25519-only (`SignatureAlgorithm` in `src/evidence/contracts.rs`), so a keystore would have to sign Ed25519. Whether a given platform keystore supports Ed25519 has not been checked here and is unverified. A non-exportable key would protect the key bytes but would still not establish that execution happened.

## Rotation

Rotation replaces the key and takes effect immediately. There is no grace period: `issued_at_ms` is asserted by the worker, so "signed before the rotation" cannot be proven. Results outstanding at rotation must be re-run or re-signed with the new key.

A device holds at most 8 keys. At that limit a rotation drops the oldest public key and keeps only its key ID, so retained evidence signed by that key would then verify as an unknown key. Retained evidence that must stay verifiable needs its own copy of the public key.

## Revocation

Revocation sets the generation and a revocation time. Retained evidence stays immutable and is historical. No result-acceptance path exists yet, so these are requirements on issue [#273](https://github.com/agentscm/agentscm/issues/273): it must accept no new result from a revoked device, and checks that depended on the device must stay blocked or Unknown. The library check already refuses a revoked device or rotated key whatever the clock says. A forge success published earlier is not withdrawn by this work; reconciling it belongs to issue [#273](https://github.com/agentscm/agentscm/issues/273) or [#179](https://github.com/agentscm/agentscm/issues/179), and this work provides only the event and the generation they need.

## Repository rename

An authorization stores the repository label at enrollment. After a rename the label no longer matches, so the authorization stops matching. This fails closed. Rotation is refused (`repository_renamed`) while the name differs, because a new key would be unusable too. Re-enrolling under the new name needs a new device ID and a new key: the old device ID exists, and a key ID is enrolled once ever. Renaming the repository back restores the authorization.

## Owner role threshold

The role check reuses the sign-in rule for Hub computers, and the owner's host must be the repository's host. The threshold depends on the forge and the repository's visibility:

- **GitHub, private repository:** any permission above none passes, including read.
- **GitHub, public repository:** triage or higher. Read, or no role, ends authorization.
- **GitLab, private project:** Reporter or higher.
- **GitLab, internal or public project:** Developer or higher. A member who is not active counts as having no role.

Demoting an owner from write to read on a private GitHub repository therefore does not end authorization, but the same demotion on a public one does, as does any fall below the thresholds above on GitLab. The owner being disabled also ends it. Issue [#273](https://github.com/agentscm/agentscm/issues/273) or a maintainer may tighten the threshold; this work states it and does not change it.

## Enrollment statement

An enrollment statement carries no timestamp and no Hub audience. It has no freshness check and is not bound to one Hub, so a statement can be submitted later, or to another Hub that connects the same repository and owner. The administrator gate, the owner and role checks and the generation and one-time key ID rules limit misuse; the statement proves only key possession.

## Administrator scope

"Administrator" means a user named by `AGENTSCM_ADMINS`. Unifying that with the workspace Administrator role belongs to issue [#20](https://github.com/agentscm/agentscm/issues/20).

## Qualification

The tests are synthetic and same-host: store conformance, an in-process fake forge with Hub routes, and the CLI. The conformance suite covers SQLite and also PostgreSQL when `AGENTSCM_TEST_DATABASE_URL` is set; without it the PostgreSQL cases are skipped. Enrollment and revocation across two physical machines is not demonstrated and is left to milestone qualification ([#275](https://github.com/agentscm/agentscm/issues/275)). Live forge role checks and real platform keystores are not qualified.
