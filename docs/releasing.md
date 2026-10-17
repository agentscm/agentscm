# Releasing

For maintainers. Releases are alpha prereleases. Nothing is published except by pushing a `v*` tag, and only from the public `counterbranch/agentscm` repository (the workflow skips any other repository, and build-provenance attestations need a public one).

1. One time, before the first tag: add a tag ruleset so only maintainers can create `v*` tags (the workflow cannot tell who pushed a tag); create the `npm` environment with required reviewers; on npmjs.com, add `counterbranch/agentscm`, workflow `release.yml` and environment `npm` as the package's trusted publisher, with `npm publish` allowed (no token is stored); turn on immutable releases in the repository settings, so a published release's assets cannot be replaced while the `npm` job waits for approval.
2. On a branch, set `version` in `Cargo.toml` and `Cargo.lock`, and give `CHANGELOG.md` an entry headed `## <version> (<date>)` with its known limits. Open a pull request; wait for CI.
3. After it merges, check `main` is green and that Dependabot and `cargo deny` have nothing open for the release.
4. Tag the merge commit: `git tag -s v<version> -m v<version> && git push origin v<version>`. The tag must equal the `Cargo.toml` version, be on `main`, and have a CHANGELOG entry, or the workflow stops before pushing anything.
5. `release.yml` builds the image, runs `.github/docker-smoke.sh`, pushes `ghcr.io/<owner>/agentscm:<version>` by digest (no `latest` during alpha), attests build provenance, and creates a GitHub prerelease from the CHANGELOG entry. A release publishes five binaries (Linux x86_64 and aarch64 musl, macOS arm64 and x86_64, Windows x86_64 msvc as `agentscm-x86_64-pc-windows-msvc.exe`), an attested `SHA256SUMS` (the binaries only) and `install.sh`, `LICENSE`, `NOTICE` and `THIRD_PARTY_NOTICES.md` (the binaries embed third-party code), and the npm package `agentscm` under the dist-tag `latest` (every release is a prerelease, so `npx agentscm` needs one; go back to `next` for prereleases after the first stable release). The npm job waits for approval on the `npm` environment; for a tag containing `-dry` it skips the publish, and for any other tag it publishes with npm trusted publishing, and the job fails if npm does not accept it, so a green release always has the package published. See [install](install.md) for what users run.
6. Set the package to public once in the GHCR package settings and link it to the repository; confirm an anonymous pull works.
7. Verify: `gh attestation verify oci://ghcr.io/<owner>/agentscm@<digest> --repo <owner>/agentscm --signer-workflow <owner>/agentscm/.github/workflows/release.yml --source-ref refs/tags/v<version> --deny-self-hosted-runners` (after `docker login ghcr.io` if the package is still private), and run the quickstart against the pulled image.
8. A dry run is a tag containing `-dry` (for example `v0.2.0-dry1`) whose version in `Cargo.toml` matches (`0.2.0-dry1`). It runs the whole workflow, including pushing the image and creating the prerelease, but skips the npm publish. Use it to exercise the workflow before a real tag, then delete the tag, the release and the image tag. The Windows binary build is first exercised by such a tag, so run a dry run before a real release that changes the Windows build. The Windows and macOS binaries are not code-signed or notarized; see [install](install.md).
9. A bad release is superseded by a new version, never edited: delete the tag and release only if nothing has pulled the image.

## Third-party software in the container image

The image is based on Debian (debian:trixie-slim, pinned by digest in the
Dockerfile) and adds Debian's `git` package. These packages are distributed
under their own licenses, including the GNU General Public License for `git`.
Each package's copyright and license file is kept in the image under
/usr/share/doc/<package>/copyright.

Source code for the Debian packages in the image is available from Debian at
https://sources.debian.org/ and https://snapshot.debian.org/ for the exact
package versions. To list them, run:

    docker run --rm --entrypoint dpkg-query ghcr.io/counterbranch/agentscm:<version> -W

On request, for three years after we publish an image, we will provide the
corresponding source for its GPL-licensed components. Email
support@counterbranch.com and name the image tag and digest.

AgentSCM's own license, NOTICE and Rust dependency notices are in the image at
/usr/share/doc/agentscm/.
