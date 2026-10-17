# Install

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

Three ways to get AgentSCM. The first two download the release binary, check it, keep it in `~/.agentscm/bin` (`%USERPROFILE%\.agentscm\bin` on Windows) and start the setup guide (`agentscm quickstart`).

## npx

```sh
npx agentscm            # setup guide; flags go to `agentscm quickstart`
npx agentscm uninstall  # any other subcommand is passed through as given
```

Needs Node 18 or later. The package has no dependencies and no install scripts. On Windows, run the same command in PowerShell:

```powershell
npx agentscm
```

See [Windows](#windows) for what the Windows build supports.

## Shell script

```sh
curl -fsSL https://github.com/counterbranch/agentscm/releases/download/v<version>/install.sh | sh
```

`install.sh` is POSIX shell only (Linux and macOS); on Windows use `npx agentscm`. The `install.sh` attached to each release is pinned to that release. Run from a clone, it needs `AGENTSCM_VERSION=<version>`. When piped, it reads prompts from `/dev/tty`.

## Docker

```sh
docker run --rm -it ghcr.io/counterbranch/agentscm:<version> --help
```

Use this on platforms without a binary. The image's default command serves on all interfaces inside the container, so when you publish a port, bind it to loopback: `-p 127.0.0.1:7331:7331` (`7332` for the hub). See [deployment](deployment.md) for running the hub.

## What is verified

- Checksum, always. `npx agentscm` checks the binary against checksums shipped inside the npm package, which is published with npm provenance. Those checksums are written in the release workflow from the build artifacts and attested with them, never copied from the GitHub release, so a release asset replaced after publication does not match them. `install.sh` fetches `SHA256SUMS` from the same release as the binary, so without `gh` it catches corruption but does not prove authenticity.
- Build provenance, only when the GitHub CLI (`gh`) is installed and signed in (`gh auth status` succeeds). The binary must be attested by this repository's `release.yml` for the matching tag, or it is deleted and not run. If `gh` is missing or not signed in, the installer says why and that it checked the checksum only.
- To make a missing or signed-out `gh` an error, set `AGENTSCM_REQUIRE_PROVENANCE=1`.

Checks run before anything is installed or linked.

## Where files go

On Linux and macOS, `~/.agentscm/bin/agentscm-<version>` is the binary, and `~/.agentscm/bin/agentscm` is a symlink to the latest installed one.

On Windows, `%USERPROFILE%\.agentscm\bin\agentscm-<version>.exe` is the binary, and `agentscm.exe` in the same directory is a copy of the latest installed one, not a symlink (symlinks need extra privileges on Windows). If `agentscm.exe` is running when you install a newer version, it cannot be replaced; the installer warns, keeps the old copy and runs the new versioned binary.

Remove them with `agentscm uninstall`.

## Platforms

Linux and macOS, x86_64 and arm64. Linux binaries are static (musl). Windows, x86_64 only (see [Windows](#windows)); Windows on arm64 may run the x64 binary under emulation, which is untested. Elsewhere, use Docker.

## Windows

The Windows binary (`x86_64-pc-windows-msvc`) is statically linked to the C runtime, so it needs no Visual C++ redistributable. Install it with `npx agentscm` in PowerShell, or download it by hand as described below.

It supports the setup guide (`agentscm quickstart` and `agentscm uninstall`, without the collision check, which needs the Unix process runner), the command hook that Cursor and Copilot call (`agentscm hook`), `agentscm init-repo`, the hub client and the hub (`agentscm hub serve`, `hub login`, `hub token`), and `agentscm intent`. The native workspace and ledger, `agentscm collisions`, the Git snapshot, export and reconcile bridge, command checks (which need Docker), the team service and the recovery commands are Linux and macOS only.

This is an alpha port. It is built and tested in CI on Windows, but few people have run it by hand. `agentscm uninstall` cannot delete the `agentscm.exe` that is running it; it says so and leaves that file for you to delete.

## Downloaded in a browser

If you download a binary from the release page instead of using `npx agentscm` or `install.sh`, check it first (see below). The binaries are not code-signed or notarized in the alpha, so the operating system warns about them.

- macOS: Gatekeeper blocks a quarantined, unsigned binary. After checking the checksum and attestation, remove the quarantine mark: `xattr -d com.apple.quarantine agentscm`.
- Windows: SmartScreen may show "Windows protected your PC". Choose "More info", then "Run anyway", only after you have checked the checksum and attestation.

`npx agentscm` and `install.sh` verify the checksum, and the provenance when `gh` is installed and signed in, before running anything. A file written by Node or `curl` does not get the mark a browser adds (quarantine on macOS, mark-of-the-web on Windows), so a browser-less install normally does not hit these prompts. Antivirus software can still scan or block a new unsigned executable.

## Verify by hand

On Linux and macOS:

```sh
cd "$(mktemp -d)"
base=https://github.com/counterbranch/agentscm/releases/download/v<version>
curl -fsSLO "$base/SHA256SUMS" -O "$base/agentscm-<target>"
shasum -a 256 -c --ignore-missing SHA256SUMS   # on Linux: sha256sum -c --ignore-missing
gh attestation verify agentscm-<target> --repo counterbranch/agentscm \
  --signer-workflow counterbranch/agentscm/.github/workflows/release.yml \
  --source-ref refs/tags/v<version> --deny-self-hosted-runners
```

On Windows (PowerShell):

```powershell
$base = "https://github.com/counterbranch/agentscm/releases/download/v<version>"
Invoke-WebRequest "$base/SHA256SUMS" -OutFile SHA256SUMS
Invoke-WebRequest "$base/agentscm-x86_64-pc-windows-msvc.exe" -OutFile agentscm-x86_64-pc-windows-msvc.exe
Get-FileHash -Algorithm SHA256 agentscm-x86_64-pc-windows-msvc.exe   # compare with its line in SHA256SUMS
gh attestation verify agentscm-x86_64-pc-windows-msvc.exe --repo counterbranch/agentscm `
  --signer-workflow counterbranch/agentscm/.github/workflows/release.yml `
  --source-ref refs/tags/v<version> --deny-self-hosted-runners
```

The three extra flags are what `npx agentscm` and `install.sh` pass: without them, an attestation from any workflow or branch of the repository would pass.

`<target>` is one of `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`. The Windows asset name ends in `.exe`.

## Publishing to npm (maintainers)

The `npm` job in `release.yml` runs after the GitHub release, in the `npm` environment. Configure required reviewers on that environment. It publishes with npm trusted publishing (OIDC): npm trusts this workflow and environment, so no token is stored. The job copies the attested build's `SHA256SUMS` (from the workflow artifact, not the release), `LICENSE` and `NOTICE` into the package and publishes with `--ignore-scripts` under the `latest` dist-tag (every release is a prerelease, so `npx agentscm` needs one; switch to `next` for prereleases after the first stable release).

A tag containing `-dry` skips the publish and succeeds; for any other tag the job fails if npm does not accept the publish, so a green release always has the package published. A dry-run tag must still equal the `Cargo.toml` version, so use a `-dry` version such as `0.2.0-dry1`. `packages/npm/package.json` `version` must equal the tag version. `NOTICE` must exist at the repository root, or the release stops before anything is pushed.
