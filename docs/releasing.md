---
title: "Releasing"
---

# Releasing

For maintainers. Plain SemVer versions publish as normal GitHub releases and are marked Latest. Suffixed versions remain prereleases and are not marked Latest. This publication status does not change the documented product limitations. Release-plz prepares version changes in a release PR; merging that PR creates the version tag. The `v*` tag triggers artifact builds and publishing, only from the public `agentscm/agentscm` repository (the workflow skips any other repository, and build-provenance attestations need a public one). Check the complete Release workflow, including npm publication, before treating a release as installable.

## Version automation and prerequisites

[Release PR](../.github/workflows/release-plz.yml) runs on pushes to `main` and manual dispatch on `main`. It uses pinned release-plz in Git-only mode with [release-plz.toml](../release-plz.toml); no crates.io publishing occurs. Squash-merge feature and fix PRs using their Conventional PR title as the commit subject. Fixes and performance changes request patches, features request minors, and breaking changes request a minor increment while the major version is zero, otherwise a major increment, following Cargo semantics.

Version numbers belong in manifests, changelogs, tags and release notes, never in PR titles, bodies or comments. Automation determines versions from Cargo and existing Git tags. It prepares `Cargo.toml`, `Cargo.lock` and `CHANGELOG.md`, then compiles [release-files](../tools/release-files.rs) to synchronize `packages/npm/package.json` and `ui/dev-seed/mint/Cargo.lock` without updating dependencies. It opens one release PR at a time, with a fixed title and body and the `ci-extended` label, and skips preparing another while a release-plz branch has an open PR. The candidate stays fixed while reviewed. If intervening main commits change the release scope, close the candidate and dispatch preparation again; review the current merge result before merging. It never force-pushes a candidate under review. Each attempt uses a distinct branch; a failed attempt before PR creation may leave an unused branch for manual cleanup, while a retry checks for an already-open candidate before creating another.

Merging an ordinary feature PR does not approve publication. With `release_always = false`, release-plz creates a tag only for a merged release-plz branch. The tag starts the existing artifact workflow, which creates the GitHub release only after its qualification jobs succeed. A candidate's version and release notes are reviewed in its file diff.

Install the existing [Counterbranch Releases GitHub App](https://github.com/apps/counterbranch-releases) on this repository and grant Contents, Issues and Pull requests read/write permissions. Store its PEM private key as the repository Actions secret `RELEASE_APP_PRIVATE_KEY`. The workflow pins the app client ID and uses `actions/create-github-app-token` to mint a token restricted to the current repository and those three permissions. The action revokes the token when the job ends; no long-lived `RELEASE_PLEASE_TOKEN` secret is needed.

The app must be allowed to create release tags under the tag ruleset while preserving protections against tag updates and deletion and all branch protections. Require the `Conventional PR title` check on `main` and configure squash merges to use the PR title. The built-in `GITHUB_TOKEN` is unsuitable because its generated PRs and tags do not trigger the downstream workflows.

Committing the workflow does not install the app, provision its key, approve installation permissions or configure tag permissions. If token creation fails, check those prerequisites before rerunning or manually dispatching Release PR. Confirm that a release PR is actually generated; configuration alone does not establish a working release pipeline. Never put a private key or token in source or a PR.

## Prepare and publish a release

1. One time, before the first tag: configure tag creation so only maintainers and Counterbranch Releases can create `v*` tags, with separate update/deletion restrictions that the app cannot bypass (the workflow cannot tell who pushed a tag); create the `npm` environment with required reviewers; on npmjs.com, add `agentscm/agentscm`, workflow `release.yml` and environment `npm` as the package's trusted publisher, with `npm publish` allowed (no token is stored); turn on immutable releases in the repository settings, so a published release's assets cannot be replaced while the `npm` job waits for approval.
2. Before qualifying a candidate, claim the release so two maintainers do not prepare one at once. Open (or reuse) a GitHub issue titled `Release v<version>` as the tracking issue, then start work on it through AgentSCM: the MCP tool `agentscm_start`, or `agentscm work start` with a strict JSON request (see the [CLI reference](reference/cli.md)). Set `source` to that issue (`provider` `github`, `host`, `repository`, and `issue` as the issue number in a string such as `"123"`) and the declared paths to `Cargo.toml`, `Cargo.lock`, `packages/npm/package.json`, `ui/dev-seed/mint/Cargo.lock` and `CHANGELOG.md`. The MCP tool takes `task` (a one-line description), `paths` and `source`; the CLI request takes `title`, `session`, `scope` (the paths) and `source`, and rejects unknown fields. A source is matched across the whole repository, so a second agent that starts the release as lead gets a `claim_held` conflict (helpers and reviewers join instead), and overlapping declared paths produce overlap notices. The `work_id` is assigned by the Hub (`wrk_...`); do not use `request_key` to claim across maintainers, because it is looked up per owner. If someone else already holds the release, stop and coordinate with them rather than tagging. The claim is a coordination step, not a lock: nothing refuses `git push origin v*` while another claim is open.
3. Review the generated release PR. Check the root Cargo manifest and lockfile, npm manifest, dev-seed lockfile and changelog for the intended version and known limits. Run `rustc --edition=2024 tools/release-files.rs -o /tmp/release-files` and `/tmp/release-files --check` from the repository root. Do not update unrelated dependencies. Add `ci-extended` so hosted tests, Windows, musl/macOS builds, container, end-to-end and live execution run on the candidate; wait for those workflow runs, not only the PR checks list.
4. Before merging or tagging, qualify the candidate on clean native hardware. Run the CLI and container tests on Linux amd64 and arm64. On a clean Apple Silicon Mac with isolated ports, container names and volumes, test the staged native macOS binary and locally built `linux/arm64` image through install, first quickstart, Hub health/UI, login/token/hooks, restart, no-op rerun, uninstall and reinstall with retained data. Use the installer-produced paths and checksums; do not disable provenance or checksum enforcement. Record this as local candidate qualification: public npm/GHCR acquisition and GitHub attestations can be checked only after publication. Preserve any existing Hub and its data throughout. After it merges, check `main` is green and that Dependabot and `cargo deny` have nothing open for the release.
5. Merge the qualified release PR through normal review and CI. The next Release PR run creates the version tag; do not also create the same tag manually. Confirm that the tag starts the Release workflow. The tag must equal the `Cargo.toml` version, be on `main`, and have a CHANGELOG entry, or publication fails. If automation fails, investigate and rerun the failed workflow without moving a published tag.
6. `release.yml` builds `linux/amd64` on `ubuntu-24.04` and `linux/arm64` on `ubuntu-24.04-arm`. Each image is loaded and fully exercised by `.github/docker-smoke.sh` on its native runner before any push. The exact tested children are then pushed under commit-specific staging tags, receive build-provenance and SPDX SBOM attestations, and supply immutable digests to a downstream job. That job assembles a candidate index from those two digests, asserts that it contains exactly the tested amd64 and arm64 runnable children, and only then, once every release binary has also been built and checked, creates `ghcr.io/<owner>/agentscm:<version>` and attests the index digest, so a failed image or binary build leaves nothing under the version name. Release runs are serialised across all tags. GitHub keeps only one waiting run per concurrency group, so a tag pushed while another run is queued cancels the queued run (re-run it), and a run waiting for `npm` environment approval, including a dry run, holds the queue until it is approved, rejected or cancelled. No `latest` tag is published during alpha. The workflow creates the GitHub release and attaches qualified artifacts with notes from the CHANGELOG entry. On a retry with an existing release, it uses the existing artifact-upload path. A release publishes five binaries (Linux x86_64 and aarch64 musl, macOS arm64 and x86_64, Windows x86_64 msvc as `agentscm-x86_64-pc-windows-msvc.exe`), an attested `SHA256SUMS` (the binaries only) and `install.sh`, `LICENSE`, `NOTICE` and `THIRD_PARTY_NOTICES.md` (the binaries embed third-party code), and the npm package `agentscm` under the dist-tag `latest` for plain versions and `next` for prerelease versions. Every binary built on matching native hardware runs `--version`; the native Linux and macOS binaries also run `local-validation reconcile --help` and require the authenticated-fetch flag to appear. The Windows binary does not support reconciliation, and macOS x86_64 is cross-compiled on arm64, so neither runs that command. These help checks establish only that the release binary exposes the command surface; they do not perform a reconciliation, exercise GitHub credentials or prove installer provenance. The npm job remains downstream of the verified image index and GitHub release. It waits for approval on the `npm` environment; for a tag containing `-dry` it skips the publish, and for any other tag it publishes with npm trusted publishing. The job fails if npm does not accept it, so a green release always has the package published. See [install](install.md) for what users run.
7. Set the package to public once in the GHCR package settings and link it to the repository; confirm an anonymous pull works.
8. Verify the published index reports exactly `linux/amd64` and `linux/arm64`, pull and run each child on matching native hardware, and repeat the clean Apple Silicon install from the public artifacts. Verify the index attestation with `gh attestation verify oci://ghcr.io/<owner>/agentscm@<digest> --repo <owner>/agentscm --signer-workflow <owner>/agentscm/.github/workflows/release.yml --source-ref refs/tags/v<version> --deny-self-hosted-runners` (after `docker login ghcr.io` if the package is still private). Verify each child digest the same way and verify its SPDX predicate with `--predicate-type https://spdx.dev/Document/v2.3`.
9. The automated path accepts only plain SemVer package versions. The publishing workflow retains its historical suffixed dry-tag handling, but those candidates do not pass the release metadata check and are not a supported main-branch rehearsal. Extended CI qualifies native builds and runtime tests before merge; exact packaging, provenance and npm publication are still verified by the tag-triggered workflow. No fully qualified no-publication end-to-end rehearsal is implemented for this path. The Windows and macOS binaries are not code-signed or notarized; see [install](install.md).
10. A bad release is superseded by a new version, never edited: delete the tag and release only if nothing has pulled the image.

## Publishing identity

The release jobs require the immutable public repository ID, so a namespace rename does not
admit a fork or a repository created later under the former name. Before attesting `install.sh`
and publishing npm, `.github/pin-release-installers.js` embeds the current publishing repository,
exact version and signing workflow identity into the installers and npm repository metadata.
The current and planned organization names are allowlisted as well as the repository ID.
Historical releases, attestations and npm packages retain their original identities.

An organization rename also requires the operator to configure the exact new npm trusted
publisher (organization, repository, `release.yml`, `npm` environment and direct publish
permission) before any new release. Keep the previous publisher during qualification, then
remove it only after validating the new path. Updating source does not update npm settings,
publish a release, or prove historical attestation lookup works after a rename. Qualify an
old exact npm version with required provenance and an uncached binary after cutover; do not
turn off verification to work around a failure.

## Activating the reusable local-validation workflow

The reusable `.github/workflows/local-validation-reconcile.yml` interface must not be adopted before its exact npm release exists. The current source-build verifier remains active until a release contains `reconcile`, path-scoped recipes and exempt paths, the opt-in authenticated GitHub fetch boundary, and the reusable workflow itself.

After npm publication succeeds, qualify the exact version from an empty directory and private home with `AGENTSCM_REQUIRE_PROVENANCE=1` and `AGENTSCM_TELEMETRY=off`: run `npx --yes --ignore-scripts --registry=https://registry.npmjs.org agentscm@VERSION local-validation reconcile --help` with stdin closed, then invoke `~/.agentscm/bin/agentscm-VERSION --version` directly and require the exact reported version. `npx agentscm@VERSION --version` is only the npm launcher's shortcut and does not install or verify the binary.

Only after that acquisition check and the release's required qualification should a follow-up change replace the source-build verifier or the adoption guide's placeholders with the exact SemVer and full reusable-workflow commit SHA. Exercise a public repository first; private GitHub fetch, redirect and timeout behavior remains unqualified until it is run and recorded separately. Do not add an automatic source fallback to the reusable interface, accept a mutable tag/range, pass a PAT, GitHub App secret or OIDC input, or use `secrets: inherit`.

## Third-party software in the container image

The image is based on Debian (debian:trixie-slim, pinned by digest in the
Dockerfile) and adds Debian's `git` package. These packages are distributed
under their own licenses, including the GNU General Public License for `git`.
Each package's copyright and license file is kept in the image under
/usr/share/doc/<package>/copyright.

Source code for the Debian packages in the image is available from Debian at
https://sources.debian.org/ and https://snapshot.debian.org/ for the exact
package versions. To list them, run:

    docker run --rm --entrypoint dpkg-query ghcr.io/agentscm/agentscm:<version> -W

On request, for three years after we publish an image, we will provide the
corresponding source for its GPL-licensed components. Email
support@counterbranch.com and name the image tag and digest.

AgentSCM's own license, NOTICE and Rust dependency notices are in the image at
/usr/share/doc/agentscm/.

The release retains commit-specific `sha-<commit>-amd64`, `sha-<commit>-arm64` and `sha-<commit>-index-candidate` tags for audit and recovery. They are staging identities; consumers should use the versioned index or its digest. Registry retention must preserve every child referenced by a supported release index.
