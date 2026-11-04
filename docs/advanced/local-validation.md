---
title: "Local validation attestations"
---

# Local validation attestations

> **Experimental (alpha).** Unix only, GitHub only, trusted-local. One maintainer key is enrolled. The verifier workflow was qualified before merge by running it under `act` against live GitHub on pull request #86: a signed failure (`failure` on that head), then a fix and a signed pass (`success` on the new head). Tampered, wrong-repository and other-head attestations were checked with `local-validation verify` against the same live comment bodies and gave `pending`. That workflow has since been replaced by `agentscm local-validation reconcile`, whose adversarial cases are tested against a fake GitHub and real local bare Git transports; the authenticated GitHub command boundary is inspected with a test-only local transport reroute. Individual live public-PR observations do not qualify private acquisition or every event and recovery route. Contributors without an enrolled key or local PostgreSQL and S3 request maintainer help and `ci-extended` diagnostic coverage; hosted tests do not satisfy the required attestation status.

A maintainer can run the repository's gate on their own machine and attach a signed, Git-bound record of the result to a pull request. A separate verifier workflow checks that record and publishes the `agentscm/local-validation` commit status. For pull requests from branches in this repository, this attestation is the expected evidence for the test gate: the expensive hosted suites no longer run on ordinary updates. See [Hosted CI and required checks](#hosted-ci-and-required-checks).

## What exists

- `agentscm local-validation run|show|post|verify|reconcile`: see the [CLI reference](../reference/cli.md#local-validation-attestations). `run` refuses a dirty tree, runs `.github/local-validation/recipe.json` with an allowlisted environment (see the trust boundary), retains each command's output as `<sha>.cmd<N>.stdout|stderr` in the state directory, and signs a portable-evidence-v1 envelope plus a `record` artifact bound to the repository, commit, tree, base ref and merge-base. Failed runs are signed too.
- Repository configuration in `.github/local-validation/`: `recipe.json` (the Rust gate plus path-selected routine checks from [contributor testing](../contributor-testing.md#routine-local-checks)), `policy.json` (repository ID, recipe digest, required PostgreSQL and S3 services, 30-day maximum age, epoch) and `registry.json` (issuer keys). A test fails when `policy.json` drifts from `recipe.json`.
- Optional recipe fields for non-Rust repositories: `dependency_files` (relative paths, 1 to 16, no duplicates, only letters, digits and `-_.:/`; each is digested at the commit and bound into the signed evidence as a dependency, and a missing file fails `run` before any command runs) and `toolchain_probes` (argv lists, 1 to 15, each recorded as its first output line or `<argv>: unavailable`). When absent they default to `Cargo.lock`, `rust-toolchain.toml` and `rustc -V`, `cargo -V`, `cargo nextest --version`; `git --version` is always appended. Declaring either field changes the recipe digest, so the policy must pin the new digest.
- Optional recipe command field `paths`: a list of 1 to 64 Git pathspecs, evaluated relative to `--repo` against `git diff-tree -r --no-renames` between the merge-base and the head. A command is required only when the change touches a matching path; absent means always required, and an empty list is rejected. Matching is Git's default wildcard matching, so `*` also matches `/` and `*.md` matches Markdown at any depth. The only long magic allowed is `:(glob|literal|exclude|top)`; short magic, `attr` and `icase` are rejected. Declaring `paths` changes the recipe digest, so the policy must pin the new digest. `run` runs only the required commands, in recipe order, and records the rest in the record's `not_required`; the coverage list reports them as `not required: <id>`, separate from `skipped: <id>` (commands not run after a failure). `verify` and `reconcile` recompute the required set from Git and fail unless every required command ran in order and `not_required` equals the remaining recipe ids exactly, so a forged "not required" claim fails.
- Trust inputs: `--config-dir` (default `.github/local-validation`) and `--verifier-workflow` (default `.github/workflows/local-validation.yml`), `--config-dir` relative to `--repo` and `--verifier-workflow` relative to the repository root, on `run`, `verify` and `reconcile`. A change to either requires every command regardless of `paths` and is never exempt. This is built in and not configurable in the policy.
- Optional policy field `exempt_paths`: a list of 1 to 64 Git pathspecs with the same syntax and limits as `paths`. Absent means nothing is exempt. Declaring it changes the policy digest. See `reconcile` below.
- `agentscm local-validation reconcile` is the verifier. It reads `policy.json`, `recipe.json` and `registry.json` from `--config-dir` (default `.github/local-validation`, relative to the default-branch checkout `--repo`, no `..`), and the API token from the environment variable named by `--token-env` (default `GH_TOKEN`). It lists every open pull request whose base is `--base`; one with a marked comment from `OWNER`, `MEMBER` or `COLLABORATOR`, a marked comment whose enrolled signature authenticates this repository/head/base under the current policy, or an existing `agentscm/local-validation` status on its head is opted in, and the rest are left alone, except for the exempt-path check below. It sets `pending` on every opted-in head first, so a crash can leave `pending` but not a stale `success`. It then fetches each head from the direct `remote.origin.url` as Git data, verifies the signed comments and, immediately before each write, re-reads the open pull requests and the same eligible comments (id and update time); if the head, base or comments changed it writes nothing and a newer event owns the verdict. It posts `success`, `failure`, `error` or `pending` on exactly the verified head SHA. If a head cannot be fetched (for example an unreachable origin, insufficient opt-in credentials or a lagging `refs/pull/N/head`), nothing more is written for it, it stays `pending`, the PR and head are logged and the run exits 1. A head shared by several open pull requests gets `pending` with a reason saying it is unsupported. A deleted attestation comment, or an edit that removes the marker, leaves no applicable attestation and so replaces the old verdict with `pending`. An incomplete listing of pull requests, comments or statuses fails closed: nothing is written for that read. It exits 0 when every pull request was handled or left pending for a newer run, 1 on any error, unread input or auth or rate-limit stop, and 2 when the policy's repository is not `--github-repo`, authenticated fetch uses a nonproduction API or noncanonical origin, the origin transport is unsafe, `--config-dir` or `--verifier-workflow` is unsafe, or `--config-dir` is unreadable at `HEAD` or dirty (nothing written).
- Every anonymous or authenticated reconciliation fetch runs through one per-run private bare Git directory with an empty template and empty private home. System, global, include and checkout transport configuration are excluded; credential helpers, prompts, redirects, hooks, fsmonitor, submodules, maintenance, lazy fetching and executable transports are disabled. The explicit validated origin URL is used for the base, pull ref and final default-branch configuration freshness fetch, with objects written to the checkout's resolved object directory. After a base fetch succeeds, a separate bounded credential-free command updates the checkout's `origin/<base>` ref. Pull code is never checked out or executed.
- Anonymous fetch is the default. It passes no API token to Git and accepts credential-free HTTP(S), `file` and local-path origins; userinfo, SSH, Git and external-command transports are refused. `--github-authenticated-fetch` is an explicit private-repository opt-in and works only with the exact API URL `https://api.github.com`, a policy repository matching `github.com/OWNER/NAME`, and one direct origin exactly `https://github.com/OWNER/NAME` with an optional `.git` suffix. It reuses the selected API token as an `Authorization: Basic base64(x-access-token:TOKEN)` extra header only in each fetch child's runtime Git environment, scoped to the canonical repository URL. The token and encoded header are absent from argv, persistent Git configuration and surfaced transport diagnostics.
- When the policy sets `exempt_paths`, `reconcile` also checks every open pull request on `--base`, opted in or not, against Git: one fetch per pull request, so cost grows with the number of open pull requests. If the change from the merge-base to the head is non-empty, touches no trust input and every changed path matches an exempt pathspec, it posts `success` with the description `not required: only exempt paths changed`, with no attestation, no opt-in and no `max_age_seconds`. This wins over any attestation verdict for that head. An empty diff, a mixed change or a head that cannot be read is never exempt, and a head shared by several open pull requests is not exempted. Other unmarked heads still get no status. If an unmarked head cannot be fetched or read, nothing is written for it and a warning is logged; it does not affect the exit code, because an unmarked head has not opted in. An opted-in head that cannot be fetched still exits 1.
- `.github/workflows/local-validation.yml` is a thin caller of the reusable `.github/workflows/local-validation-reconcile.yml`, pinned to an exact published release and full commit SHA, which installs that provenance-checked binary and runs `reconcile`. The verifier does not pick up a change to its own code until the pin moves. It runs on `pull_request_target` (opened, synchronize, reopened, and edited only when the base branch changed) so that a head that changes only exempt paths gets its status promptly, on a PR comment that starts with the attestation marker from an `OWNER`, `MEMBER` or `COLLABORATOR` (created, edited or deleted; an edit that removes the marker also runs it; other commenters are ignored and the signature still decides), on pushes to `main` that touch `.github/local-validation/**` or the workflow itself, and daily. There is no manual dispatch. All runs that pass the trust filter share one repository-wide job-level concurrency group (`local-validation`, queued rather than cancelled; job-level so that ignored comments never enter it and cannot cancel the queued run) and every run reconciles all open pull requests on the default branch. Runs are full sweeps because GitHub keeps only the newest queued run per concurrency group, so a per-PR run could be superseded by another PR's run and leave a stale `success`. Its first step, `Invalidate stale successes`, runs before checkout, toolchain and build and uses only `gh` to set to `pending` every open default-branch pull request head whose latest `agentscm/local-validation` status is `success`, so a setup failure cannot leave a stale `success`. It reads every page of each head's status. A head whose status read fails is treated as unknown and set to `pending` anyway. A failed or unexpected write for one head is reported with `::error::` and the SHA, the step continues with the next head and still exits 0 so that setup and `reconcile` run (reconcile retries and reports its own errors). It leaves heads with no such status (not opted in), `pending`, `failure` and `error` alone. A listing failure or an invalid head SHA fails the step before any status is written. The 5-minute step timeout can also stop it partway, leaving later heads unchanged and skipping `reconcile`. It never checks out, builds or runs PR code.
- Status count: every run (each trusted attestation comment event, each pull request opened, synchronize, reopened or retarget event from any author, each push touching verifier inputs, and the daily run) writes two statuses (`pending`, then the verdict) on every opted-in head, plus one earlier `pending` from the invalidation step on a head whose latest verdict is `success`, so up to three. GitHub keeps at most 1000 statuses per SHA and context, so a head reaches the cap after about 333 runs (about 500 if the head never holds a `success` when a run starts). After that the `pending` write is refused, that head is not re-verified, the run exits 1 and the earlier verdict remains on the head, which may be stale. Because pull request events from any author start a sweep, an untrusted author can drive status writes toward that cap on opted-in heads; a head with an exempt success is opted in through its status. This is a limit, not handled.

## Compact comment display and compatibility

`local-validation show` renders a compact comment with the pinned AgentSCM icon and “AgentSCM · Local validation” heading, reported outcome, observed/expected check counts, omission count, repository and 12-character commit visible at first. Command results, coverage and environment, source details and signed machine-readable evidence are four closed disclosure sections in that order, so supporting Markdown renderers show them collapsed by default. The coverage section lists every signed omission, service availability, platform and toolchain. Missing services are environment facts; the renderer does not infer incomplete coverage without the repository policy.

The summary and disclosure markup are display only. The exact signed envelope and record bytes remain in their named fences, and the first-line marker still commits to the envelope digest. `verify` extracts those exact bytes while ignoring surrounding Markdown, so the compact layout does not change signatures, verification or adoption of legacy flat comments.

To replace an older flat presentation, rerun `local-validation post` for the same retained attestation. On GitHub it edits the posting user's existing comment with the same marker in place instead of creating another comment. Posting and the hosted verifier workflow are GitHub-only. The Markdown carrier and standalone verifier are forge-neutral, but no GitLab posting or verifier integration exists, and GitLab rendering or sanitization of this presentation has not been qualified.

## Compatibility of path-scoped fields

Recipes and policies that do not use `paths` or `exempt_paths` serialize as before and keep their digests. Decoding is strict, so an older AgentSCM binary rejects a recipe, policy or record that uses the new fields (`paths`, `exempt_paths`, `not_required`); an adopter that pins an AgentSCM version must upgrade it before using them. Adding `exempt_paths` to the policy or `paths` to the recipe changes the policy or recipe digest, so outstanding attestations stop matching.

## Trust boundary

- A valid signature proves only that the holder of a registered key signed bytes bound to that commit, tree and base. The execution basis is `worker_asserted`: nothing proves the commands ran as recorded.
- The signing user account and host are trusted. Recipe commands run as the signing user with `HOME` passed, so attested code can read `~/.config/gh`, the signing key, cargo configuration such as `.cargo/config.toml` aliases and anything else that user can read. The environment allowlist only avoids passing tokens through environment variables; it is not a sandbox. Never attest code you would not run as that user, and never attest third-party or fork code on a signing host.
- Anyone with `statuses: write` can post the same status context. The repository's required check is bound to the GitHub Actions integration, but this is not cryptographic proof that the status writer ran the verifier. Required-status configuration remains operator-owned.
- Policy, recipe and registry are read from the default branch, not from the PR.
- This repository's policy requires the PostgreSQL and S3 test services (`AGENTSCM_TEST_DATABASE_URL`, `AGENTSCM_TEST_S3_*`). A run without a service the policy requires is recorded and verifies as a failure; an absent service the policy does not require adds no service coverage omission.

## Maintainer workflow

1. Create a key with `agentscm agent-task key-generate` (owner-private file).
2. Enroll the public key in `.github/local-validation/registry.json` by a reviewed pull request. Only enrolled, unrevoked and unexpired keys verify; revoke by setting `revoked_at_ms` in a reviewed change.
3. On a clean checkout of the pushed PR head, start the PostgreSQL and S3 test services and use the commands below. Snapshot the trusted configuration from the fetched default branch even when the PR changes it. `run` has no defaults for the recipe, policy or registry; pass all three. Ctrl-C (SIGINT) cancels the running command's process group and signs a failure that records the command as `cancelled`; a second Ctrl-C exits immediately.
4. Verify the retained envelope against the same trusted configuration, then `local-validation post` it to the PR. Posting refuses if the PR head differs from the attested commit. Do not post statuses by hand.
5. The verifier workflow reports the status. Its first step sets a previous `success` on an open pull request head to `pending` before any setup. If `reconcile` cannot read or write something it exits 1 and leaves that head `pending`, or its previous status if the invalidating `pending` write itself was refused or uncertain; the next comment, push or daily run retries, or re-run the failed job. Report `pending` or `failure` as it is.

`verify` is forge-neutral and also runs standalone against a local Git checkout; its `--recipe` default, `.github/local-validation/recipe.json`, is relative to the current directory. `reconcile` is anonymous by default; private GitHub repositories require the explicit authenticated-fetch flag and canonical origin described above. The standalone `evidence-verifier` tool has not been exercised on these envelopes.

### Run, verify and post

In this repository, run `tools/attest-pr.sh [PR_NUMBER]` on a clean checkout of the pushed head. It does the steps below using the release the verifier pins, and starts disposable PostgreSQL and S3 containers from the images in `.github/workflows/ci.yml` when the service variables are unset. The issuer defaults to the signed-in GitHub login. The key is the file named by `AGENTSCM_LV_KEY`, or else `$AGENTSCM_LV_DIR/$ISSUER.ed25519`, or else `$AGENTSCM_LV_DIR/$ISSUER.key`. The binary refuses a symlinked or hard-linked key, or one whose folder is not owner-private, so point `AGENTSCM_LV_KEY` at the key rather than linking or copying it. With no key it exits 0 without signing or posting; any other failure exits nonzero and posts nothing. Tools the recipe selects for the change must be on `PATH`.

Only one attestation runs at a time for a user account on a machine, across worktrees, because concurrent full suites fail timing-sensitive tests. Before it installs the pinned release, starts containers or runs the recipe, the script takes the lock directory `~/.agentscm/attest-pr.lock`, which holds the holder's pid. A second run prints `waiting for <pid> (<command>)` and waits. A lock whose pid is no longer running is reclaimed, so a crashed or killed run does not block the next one. To keep another full suite from overlapping an attestation, run it under the same lock: `tools/attest-pr.sh --with-lock cargo nextest run --locked`. A process killed in the moment between creating the lock and writing its pid leaves a lock that is never reclaimed; remove it, and `attest-pr.lock.reclaim` if present, by hand.

When the recipe does not pass, the script posts nothing and exits 1. It prints where `run` kept the signed failure record and envelope in the state directory, which commands failed with the paths of their retained `<sha>.cmd<N>.stdout` and `.stderr` output, and the `local-validation show` command that renders the summary.

The manual equivalent follows. This example uses the enrolled `boorad` issuer and conventional key location; set `AGENTSCM_LV_DIR` to another absolute private directory when needed. It requires an installed compatible `agentscm`, `gh`, `jq`, the recipe's tools and the service variables already exported. The example never prints private key bytes. Missing prerequisites, services, a dirty checkout or an unpushed head stop the example rather than silently skipping attestation.

```sh
(
set -eu
umask 077
cd "$(git rev-parse --show-toplevel)"
lv_root=${AGENTSCM_LV_DIR:-$HOME/.config/agentscm/local-validation}
lv_key="$lv_root/boorad.ed25519"
lv_state="$lv_root/agentscm-attestations"
lv_bin=$(command -v agentscm)
test -f "$lv_key"
test -n "${AGENTSCM_TEST_DATABASE_URL:-}"
test -n "${AGENTSCM_TEST_S3_ENDPOINT:-}"
test -n "${AGENTSCM_TEST_S3_BUCKET:-}"
test -n "${AGENTSCM_TEST_S3_ACCESS_KEY_ID:-}"
test -n "${AGENTSCM_TEST_S3_SECRET_ACCESS_KEY:-}"
test -z "$(git status --porcelain)"
lv_head=$(git rev-parse HEAD)
lv_pr=$(gh pr view --json number --jq .number)
test "$(gh pr view "$lv_pr" --json headRefOid --jq .headRefOid)" = "$lv_head"
git fetch origin main
lv_base=$(git rev-parse origin/main)
lv_tmp=$(mktemp -d)
trap 'rm -rf "$lv_tmp"' EXIT
for file in recipe policy registry; do
  git show "$lv_base:.github/local-validation/$file.json" >"$lv_tmp/$file.json"
done
mkdir "$lv_tmp/home"
printf '[core]\n\tautocrlf = false\n' >"$lv_tmp/home/.gitconfig"
lv_cargo=${CARGO_HOME:-$HOME/.cargo}
lv_rustup=${RUSTUP_HOME:-$HOME/.rustup}
HOME="$lv_tmp/home" CARGO_HOME="$lv_cargo" RUSTUP_HOME="$lv_rustup" \
  "$lv_bin" local-validation run --repo . \
  --config-dir .github/local-validation \
  --verifier-workflow .github/workflows/local-validation.yml \
  --recipe "$lv_tmp/recipe.json" --policy "$lv_tmp/policy.json" \
  --registry "$lv_tmp/registry.json" --key-file "$lv_key" --issuer-id boorad \
  --state-dir "$lv_state" --base-ref main --base-rev "$lv_base" >"$lv_tmp/result.json"
test "$(jq -r .outcome "$lv_tmp/result.json")" = pass
lv_sha=$(jq -r .envelope_sha256 "$lv_tmp/result.json")
git fetch origin main
git diff --quiet "$lv_base" origin/main -- .github/local-validation
"$lv_bin" local-validation verify --repo . --head "$lv_head" --base-rev origin/main \
  --repository github.com/agentscm/agentscm \
  --recipe "$lv_tmp/recipe.json" --policy "$lv_tmp/policy.json" \
  --registry "$lv_tmp/registry.json" \
  --envelope "$lv_state/$lv_sha.envelope.json" --record "$lv_state/$lv_sha.record.json"
gh auth token >"$lv_tmp/github-token"
"$lv_bin" local-validation post --state-dir "$lv_state" --attestation "$lv_sha" \
  --github-repo agentscm/agentscm --pr "$lv_pr" --token-file "$lv_tmp/github-token"
)
```

Use absolute key, state, Cargo and Rustup paths. The temporary `HOME` gives recipe fixtures a stable Git line-ending configuration without changing the user's Git configuration. `HOME`, `CARGO_HOME`, `RUSTUP_HOME` and `PATH` are allowlisted; `GIT_CONFIG_GLOBAL` is not. This reduces accidental host-configuration effects but is not isolation from candidate code running as the signing user.

If a PR changes the policy or recipe, its accepted pre-merge attestation still uses the current default branch's configuration. Qualify the candidate recipe separately and retain that result locally; do not post it before its policy lands, because a newer applicable attestation with a mismatched recipe can replace an older success with failure. After merge, new attestations use the newly trusted configuration. A configuration freshness or verification failure requires inspecting the change and rerunning, not changing the signed record.

## Hosted CI and required checks

Routine formatting, host Clippy, release-metadata checks, Linux nextest/doctests with PostgreSQL/S3, dependency audit, offline workflow-security lint, UI checks, ShellCheck and Linux npm installer tests run locally by default. They do not execute on ordinary pull-request updates or pushes to `main`.

- `ci-extended` requests the hosted fallback after maintainer review. For same-repository pull requests it runs on labeling and subsequent updates while the label remains. For fork pull requests it runs once when a maintainer applies the label; remove and re-add it for a later head. Routine path-filtered workflows run only when their paths match. Adding a different label can record skipped jobs on the same head, so inspect the admitted workflow runs rather than treating a skipped check as coverage.
- Manual `workflow_dispatch` on the branch is another fallback for CI, dependency audit, workflow lint, UI, installer and documentation workflows. It does not publish a local-validation attestation or replace the enrolled-signature status. Fork authors without an enrolled key request maintainer-reviewed hosted coverage rather than running fork code on a signing host.
- Windows npm installer tests remain automatic for matching installer paths on pull requests and `main`. Windows build/tests, musl/macOS release-target builds, container checks, Hub end-to-end and live execution keep their existing `main`, `ci-extended` and manual coverage. Platform jobs establish their own coverage; a local host pass does not qualify them.
- Documentation publishing still builds and checks its artifact on matching `main` pushes. Its routine audit and pull-request build/check move to the local recipe, with `ci-extended` and manual build/audit/check fallback; only `main` pushes publish. Release automation and tagged release qualification, PR-title validation and the local-validation verifier remain hosted.

For a pull request without an enrolled key or required services, request maintainer help and `ci-extended` diagnostic coverage. Hosted fallback results remain separate from `agentscm/local-validation`; they never manufacture a signed local success or satisfy its required check. Without an accepted attestation the normal merge path remains blocked, even when hosted tests pass. A maintainer may deliberately review and adopt trusted changes into a same-repository branch and new pull request before signing. Moving code between branches alone establishes no trust; never automatically run unreviewed fork code on a signing host to clear the gate. There is no automatic weekly dependency audit: run the local audits periodically for new advisories even when no source changes select them.

The operator-managed `main` ruleset requires `agentscm/local-validation` and `Conventional PR title`, both bound to the GitHub Actions integration. The existing administrator bypass, signing rules and other protections remain in place. These settings are separate from committed workflow files. A missing, pending, failure or error attestation status does not satisfy the normal merge gate; a successful verifier workflow job alone is insufficient. `CI / static` is opt-in and must not be made a required automatic check. Workflow syntax and local command checks do not establish live GitHub event scheduling, private-repository behavior or measured cost savings.

GitHub author associations can differ by API viewer, so signed-comment discovery does not depend solely on `MEMBER` visibility. The fallback requires current enrolled issuer trust, freshness, policy and artifact bindings and the exact repository, head and base; arbitrary contributor markers, invalid signatures and unrelated records do not opt a head in. It authenticates discovery only: the final verdict still fetches Git and independently checks the tree, merge-base and required commands. Authenticated failures remain eligible so they can replace old successes. The same eligibility rules guard the final comment fingerprint.

The comment-event workflow filter remains limited to recognized forge associations for compute-abuse control. A signed comment whose event itself has a weaker association may wait for an admitted PR, push or daily run. The discovery fallback is fixture-qualified; a verifier still built from an older default-branch revision must receive this fix through the maintainer-controlled merge path before it can recognize that fallback. A green workflow job that writes no attestation status does not satisfy the required check.

## What invalidates an attestation

- A new head commit or tree, or a different merge-base.
- A change to the policy, recipe or registry. The verifier re-runs on pushes to `main` that touch the verifier inputs, and daily, so an aged-out attestation's `success` is replaced. A run writes nothing for a head if the pull request's head, base or trusted attestation comments changed while it ran; the newer run owns the verdict.
- A change to the default branch's verifier configuration directory (`--config-dir`, which must be relative to `--repo`) after the checkout. Before each write `reconcile` re-fetches `origin/<base>` and compares that directory's tree with the checkout's `HEAD`; if they differ it writes nothing for that head, logs that a newer run owns the verdict, counts no error and stops processing further heads (they stay `pending`); if the fetch or a read fails the head stays `pending` and counts as an error. Before any request `reconcile` refuses (exit 2, nothing written) a `--config-dir` that is unsafe, cannot be read at `HEAD`, or has uncommitted or untracked changes, because the guard compares `HEAD`. This covers the configuration only: a change to AgentSCM's own `src/**` verifier code is covered by the queued push run, not by this guard.
- Age beyond the policy maximum.

## Limits

- The first step, `Invalidate stale successes`, resets a `success` to `pending` before checkout, toolchain, build and install, so a failure in any of them cannot leave a stale `success`. Limits: if the listing fails, statuses are unchanged and an earlier `success` can remain visible until the next admitted event or daily run retries. A write refused for one head (for example at the status cap) is reported and leaves that head's `success` in place, but the other heads are still reset and `reconcile` still runs. `failure` and `error` are deliberately not reset. A failed status read sets `pending` anyway, which opts that head in for good, because `reconcile` treats any status in this context as opt-in. A force-push back to an earlier SHA, or a SHA shared with another pull request, can carry a `success`; the step resets those too. Every admitted run now holds `success` heads at `pending` for the whole setup and build time, not only during reconcile, which affects auto-merge and merge queues waiting on that status. The step is tested only against a fake `gh`, not on live GitHub.
- The status is attached to the commit, so PRs that share a head commit (for example one branch opened against two bases) share it; that case is unsupported.
- A base change (`pull_request_target` `edited`) runs the verifier, so the reconciled base is refreshed promptly. A PR retargeted away from `--base` keeps its last status on that head, including an exempt success, which `reconcile` no longer updates.
- Pull requests are read in bounded pages (30 pages of open pull requests, 10 of comments per pull request); a larger listing fails closed with exit 1. Runs process pull requests serially within the 30-minute job and the token rate limit, which is practical for tens of open PRs, not hundreds.
- An aged-out (expired) `success` is replaced by the next run, at the latest the daily one, so within about a day of expiry.

Advancing the base branch tip does not invalidate it; only the merge-base is bound. The base tip is deliberately not bound, and the tests ran on the PR head, not on the merge result. The maintainers accept this limit.

## Coverage omissions

A command skipped by `paths` is listed as `not required: <id>`. Missing PostgreSQL or S3 adds a service omission only when the policy requires that service. Comments preserve all signed omissions, including service omissions in historical attestations, regardless of current policy. Every attestation records these as not covered: ignored tests, Windows, container smoke, hub end-to-end and live execution. Hosted CI remains the way to get platform and runtime coverage; request it as described above.

## Not done

Routine hosted jobs are gated on the `ci-extended` label or manual dispatch, not on the presence of an attestation, so a pull request with neither has no routine hosted test coverage and cannot satisfy the required attestation gate. Required-status enforcement does not prove execution or remove the existing administrator bypass. Fake-provider tests remain the evidence for path-scoped, exempt-path and adversarial reconciliation cases; one successful public-PR signing and verification run does not qualify private acquisition, every event route or all failure recovery behavior.

## Adopting in another repository

Qualify adoption separately in each repository. Synthetic tests cover fake GitHub API behavior, real local bare Git transport, hostile checkout/global Git configuration, canonical-origin refusal, authenticated command construction and secret non-persistence. They do not exercise live private-repository credentials, TLS behavior, HTTP redirects or transport timeouts, so authenticated private fetch remains unqualified against GitHub. Syntax checks and local workflow runners do not qualify GitHub Actions scheduling or concurrency behavior.

1. Add `.github/local-validation/policy.json` with `repository` (`github.com/OWNER/NAME`), `recipe_sha256`, `required_services` set to `{"postgres":false,"s3":false}`, `max_age_seconds` and `epoch`, and `registry.json` with the maintainer's enrolled public key. Create keys and enroll them as in the maintainer workflow above.
2. Add a recipe. For a Bun repository:

```json
{
  "recipe_id": "bun-gate-v1",
  "dependency_files": ["bun.lock", "package.json"],
  "toolchain_probes": [["bun", "--version"]],
  "commands": [{ "id": "test", "argv": ["bun", "test"], "timeout_secs": 600 }]
}
```

Set the policy's recipe digest to the new recipe's digest.

3. Add the caller workflow below. It pins `0.2.1` and its tag's full commit SHA; to use another release, change both to an exact published SemVer and the full commit SHA that contains `.github/workflows/local-validation-reconcile.yml`. Do not use `latest`, a tag, a branch or a version range.

```yaml
name: Local validation

on:
  issue_comment:
    types: [created, edited, deleted]
  pull_request_target:
    types: [opened, synchronize, reopened, edited]
  push:
    branches: [main] # replace when the repository's default branch has another name
    paths:
      - ".github/local-validation/**"
      - ".github/workflows/local-validation.yml"
  schedule:
    - cron: '17 6 * * *'

permissions:
  contents: read
  pull-requests: read
  statuses: write

jobs:
  reconcile:
    uses: agentscm/agentscm/.github/workflows/local-validation-reconcile.yml@5cd629e46a895655a404ccb5baa33e30859dfdb8
    with:
      release-version: 0.2.1
      config-dir: .github/local-validation
      verifier-workflow: .github/workflows/local-validation.yml
      github-authenticated-fetch: false
```

The reusable workflow runs the same `Invalidate stale successes` step first, so a caller gets it only by pinning a reusable-workflow commit SHA that includes it. The caller supplies all triggers; the reusable workflow owns the trusted-comment/base-change filter, repository-wide job concurrency and full sweep. Do not add the same concurrency group to the caller. Each admitted run checks out only the caller's default branch with full history and `persist-credentials: false`; it never checks out, builds or runs a pull request head, recipe, script or artifact. The reusable job has only `contents: read`, `pull-requests: read` and `statuses: write`, accepts no PAT, GitHub App, OIDC or `secrets: inherit` input, and installs the exact npm package outside the caller checkout with lifecycle scripts disabled. The install runs `local-validation reconcile --help`, which reaches the verified binary installer; `npx agentscm --version` would only print the npm package version and is not an installation or provenance check.

For a canonical private GitHub repository, set `github-authenticated-fetch: true`. This does not introduce another credential: the verifier reuses the same job-scoped `github.token` selected for the GitHub API and exposes it to Git only for the isolated canonical HTTPS fetch commands. The default is `false`. Live private acquisition, redirects and timeouts remain unqualified.

If the caller file or configuration directory has another name, update its `push.paths`, pass the same `config-dir` and `verifier-workflow` values above, and pass those exact trust inputs when producing the attestation:

```sh
agentscm local-validation run \
  --config-dir .github/local-validation \
  --verifier-workflow .github/workflows/local-validation.yml \
  --recipe .github/local-validation/recipe.json \
  --policy .github/local-validation/policy.json \
  --registry .github/local-validation/registry.json \
  --key-file KEY --issuer-id ID --state-dir DIR
```

The verifier workflow path is always a trust input; a custom path is not exempt from commands or status reconciliation.

### Source-build fallback

New adopters should use the pinned caller above. This fallback stays only until this repository's pinned verifier has been exercised on real pull requests, and is then due for removal. It preserves the same triggers, trusted-comment/base-change filter, repository-wide job concurrency and full-sweep behavior. The sample's first step, `Invalidate stale successes`, is the same script as in the reusable workflow. The AgentSCM source pin `24cf76705bcbbac284fbcb18646d225f911b39a2` contains `reconcile`, path-scoped recipes, exempt paths and the opt-in authenticated fetch boundary, but not the reusable prebuilt-release workflow. This fallback is not an automatic mode of the reusable workflow and must be removed deliberately after published-release qualification.

```yaml
name: Local validation

on:
  issue_comment:
    types: [created, edited, deleted]
  pull_request_target:
    types: [opened, synchronize, reopened, edited]
  push:
    branches: [main]
    paths:
      - ".github/local-validation/**"
      - ".github/workflows/local-validation.yml"
  schedule:
    - cron: '17 6 * * *'

permissions: {}

jobs:
  reconcile:
    concurrency:
      group: local-validation
      cancel-in-progress: false
    if: ${{ (github.event_name != 'pull_request_target' || github.event.action != 'edited' || github.event.changes.base) && (github.event_name != 'issue_comment' || (github.event.issue.pull_request && (startsWith(github.event.comment.body, '<!-- agentscm-local-validation:v1 ') || (github.event.action == 'edited' && startsWith(github.event.changes.body.from, '<!-- agentscm-local-validation:v1 '))) && contains(fromJSON('["OWNER","MEMBER","COLLABORATOR"]'), github.event.comment.author_association))) }}
    runs-on: ubuntu-latest
    timeout-minutes: 30
    permissions:
      contents: read
      pull-requests: read
      statuses: write
    steps:
      - name: Invalidate stale successes
        shell: bash
        timeout-minutes: 5
        env:
          GH_TOKEN: ${{ github.token }}
          REPO: ${{ github.repository }}
          DEFAULT_BRANCH: ${{ github.event.repository.default_branch || github.ref_name }}
          RUN_URL: ${{ github.server_url }}/${{ github.repository }}/actions/runs/${{ github.run_id }}
        run: |
          set -euo pipefail
          # A listing error or an invalid head SHA fails the step before any write. Pagination is bounded by the step timeout.
          heads=$(gh api -X GET "repos/$REPO/pulls" -f state=open -f base="$DEFAULT_BRANCH" -f per_page=100 --paginate --jq '.[].head.sha' </dev/null)
          # Validate every line before any write; a jq null prints as "null" and fails here.
          if [[ -n "$heads" ]] && printf '%s\n' "$heads" | grep -qvE '^[0-9a-f]{40}$'; then
            echo "unexpected head SHA in the pull request listing" >&2
            exit 1
          fi
          count=0
          failed=0
          for sha in $(printf '%s\n' "$heads" | sort -u); do
            # --paginate reads every page of the combined status, which has at most one entry for our context.
            # Take the first non-empty line; no output means the head never opted in ("none"). A failed read
            # is "unknown": invalidate anyway, because a stale success is worse than an extra pending.
            if out=$(gh api -X GET "repos/$REPO/commits/$sha/status" -f per_page=100 --paginate --jq '.statuses[] | select(.context == "agentscm/local-validation") | .state' </dev/null); then
              state=$(printf '%s\n' "$out" | grep -m1 . || true)
              state=${state:-none}
            else
              echo "::warning::could not read the status of $sha; setting it to pending anyway"
              state=unknown
            fi
            # Only success (or a failed read) can satisfy a required check. "none" means the head never opted
            # in, and writing would opt it in forever. pending, failure and error already block merging.
            if [[ "$state" != success && "$state" != unknown ]]; then
              continue
            fi
            if ! reply=$(gh api -X POST "repos/$REPO/statuses/$sha" -f state=pending -f context=agentscm/local-validation -f description='setup started; re-verifying local validation' -f target_url="$RUN_URL" --jq .state </dev/null); then
              echo "::error::failed to set $sha to pending"
              failed=$((failed + 1))
              continue
            fi
            if [[ "$reply" != pending ]]; then
              echo "::error::unexpected reply writing status for $sha: $reply"
              failed=$((failed + 1))
              continue
            fi
            echo "$sha: $state -> pending"
            count=$((count + 1))
          done
          echo "set $count stale success(es) to pending; $failed failure(s)"
          # Exit 0 even after per-head failures so setup and reconcile still run; reconcile retries and reports its own errors.
          exit 0
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          ref: ${{ github.event.repository.default_branch || github.ref_name }}
          fetch-depth: 0
          persist-credentials: false
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          repository: agentscm/agentscm
          ref: 24cf76705bcbbac284fbcb18646d225f911b39a2
          path: agentscm-src
          persist-credentials: false
      - run: rustup toolchain install
        working-directory: agentscm-src
      - uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
        with:
          workspaces: agentscm-src
      - run: cargo build --locked --release --bin agentscm
        working-directory: agentscm-src
      - name: Reconcile statuses
        env:
          AUTHENTICATED_FETCH: "false"
          BASE: ${{ github.event.repository.default_branch || github.ref_name }}
          GH_TOKEN: ${{ github.token }}
          REPO: ${{ github.repository }}
          RUN_URL: ${{ github.server_url }}/${{ github.repository }}/actions/runs/${{ github.run_id }}
        run: |
          set -euo pipefail
          args=(
            local-validation reconcile
            --github-repo "$REPO"
            --base "$BASE"
            --config-dir .github/local-validation
            --repo "$GITHUB_WORKSPACE"
            --verifier-workflow .github/workflows/local-validation.yml
            --target-url "$RUN_URL"
          )
          if [[ "$AUTHENTICATED_FETCH" == "true" ]]; then
            args+=(--github-authenticated-fetch)
          fi
          # Every admitted run sweeps all open PRs, so the latest queued run is enough.
          agentscm-src/target/release/agentscm "${args[@]}" </dev/null
```

Set `AUTHENTICATED_FETCH` to `true` only for the canonical private GitHub case described above. The fallback builds only the exact pinned AgentSCM source and reads only the caller's default-branch configuration; pull request code is fetched as inert Git data and is never checked out or run.

The job-level concurrency placement is security-relevant. A workflow-level group is entered before the `if:` is evaluated, so an ignored comment run could supersede the pending eligible run and then do nothing. GitHub keeps only the newest pending run in a group even with `cancel-in-progress: false`; full sweeps prevent a run for one pull request from superseding another pull request's only reconciliation.
