# Install

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

Choose a version from [Releases](https://github.com/counterbranch/agentscm/releases). In the examples
below, replace `<version>` with that version without its leading `v`. Container images are
versioned; alpha releases do not publish a `latest` image tag. For reproducible installs, keep the
selected version or image digest in your deployment configuration.

These docs describe the branch or tag you are viewing. A command present on `main` may not be in
a published binary yet. Use the documentation at the matching release tag, or
[build from source](../CONTRIBUTING.md#set-up) to try unreleased features.

## Choose local or hosted

The first question the setup guide asks is where AgentSCM should run:

- **Local, on this machine (recommended for the alpha).** The hub runs on your computer and your agents use it. Run `npx agentscm --local` to skip the question. Without a terminal (CI, or an agent's shell), add `--yes --github-login YOUR_LOGIN`.
- **Hosted, a shared hub on a server for a team.** Not supported in the alpha: it will likely break and is not for production. Quickstart does not set up a server. `npx agentscm --hosted` prints the [hosted guide](hosted.md) for you to work through with your coding agent; once the team's hub runs, `npx agentscm --hosted --hub-url https://hub.example.com` connects this machine to it.

`install.sh` passes the same flags: `curl -fsSL .../install.sh | sh -s -- --local`.

### If you are an agent installing AgentSCM for someone

1. Ask the person: "Do you want to run AgentSCM locally on this machine (recommended for the alpha), or set up a hosted hub for a team (unsupported in the alpha)?"
2. Local: run `npx agentscm --local` in their terminal so they can answer its prompts. If you have no terminal, ask for their GitHub login and run `npx agentscm --local --yes --github-login LOGIN`.
3. Hosted: follow [hosted.md](hosted.md) step by step with them. If their team's hub already runs, ask for its URL and run `npx agentscm --hosted --hub-url URL`.

Three ways to get AgentSCM. The first two download the release binary, check it, keep it in `~/.agentscm/bin` (`%USERPROFILE%\.agentscm\bin` on Windows) and start the setup guide (`agentscm quickstart`).

The download and verification steps do not send product telemetry. After verification, the installers invoke the binary to record one installation indicator only when telemetry was already configured and permitted for this user. Pass `--no-telemetry`, set `AGENTSCM_TELEMETRY=off`, or set `DO_NOT_TRACK=1` before installing to suppress it. On first use, a build containing the staged capability follows the [setup choice and telemetry controls](telemetry.md). No event can be sent until an HTTPS endpoint and data-handling profile are configured.

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

Use this as an explicit fallback on a supported Linux AMD64 or ARM64 Docker daemon. A macOS or Windows host needs a Linux VM, such as Docker Desktop. The release workflow builds both image architectures; use a release that publishes the matching architecture. An unsupported host architecture still needs a supported Docker daemon or an existing remote hub. The image's default command serves on all interfaces inside the container, so when you publish a port, bind it to loopback: `-p 127.0.0.1:7331:7331` (`7332` for the hub). See [deployment](deployment.md) for running the hub.

## What is verified

- Checksum, always. `npx agentscm` checks the binary against checksums shipped inside the npm package, which is published with npm provenance. Those checksums are written in the release workflow from the build artifacts and attested with them, never copied from the GitHub release, so a release asset replaced after publication does not match them. `install.sh` fetches `SHA256SUMS` from the same release as the binary, so without `gh` it catches corruption but does not prove authenticity.
- Build provenance, only when the GitHub CLI (`gh`) is installed and signed in (`gh auth status` succeeds). The binary must be attested by this repository's `release.yml` for the matching tag, or it is deleted and not run. If `gh` is missing or not signed in, the installer says why and that it checked the checksum only.
- To make a missing or signed-out `gh` an error, set `AGENTSCM_REQUIRE_PROVENANCE=1`.

Successful installer verification prints one summary line. To include the verification
command's details, set `AGENTSCM_INSTALL_VERBOSE=1`. Verification failures always show
diagnostics and stop installation.

Checks run before anything is installed or linked.

## Where files go

On Linux and macOS, `~/.agentscm/bin/agentscm-<version>` is the binary, and `~/.agentscm/bin/agentscm` is a symlink to the latest installed one.

On Windows, `%USERPROFILE%\.agentscm\bin\agentscm-<version>.exe` is the binary, and `agentscm.exe` in the same directory is a copy of the latest installed one, not a symlink (symlinks need extra privileges on Windows). If `agentscm.exe` is running when you install a newer version, it cannot be replaced; the installer warns, keeps the old copy and runs the new versioned binary.

Quickstart prints the installed executable's full path, so it works before `agentscm`
is on PATH. On macOS or Linux, use `export PATH="$HOME/.agentscm/bin:$PATH"` in the
current shell, or add that line to your shell's startup file for future terminals.
You can also keep using `npx agentscm <subcommand>`.

Remove them with `agentscm uninstall`.

For an optional customer-operated Slack app, follow [Slack setup](slack.md). The same preparation commands support an installation agent, the official Slack CLI and manual app settings. A prepared private bundle can be supplied to `quickstart --hub local --slack-config-file ABSOLUTE_PATH`; installing the AgentSCM binary alone does not connect Slack.

## Platforms

Linux and macOS, x86_64 and arm64. Linux binaries are static (musl). Windows, x86_64 only (see [Windows](#windows)); Windows on arm64 may run the x64 binary under emulation, which is untested. For other hosts, check whether a local Linux AMD64 or ARM64 Docker daemon is supported. Docker availability and emulation depend on the host; they are not a universal fallback.

The CLI platform and the managed hub runtime are separate. Quickstart defaults to a native user service on Apple Silicon macOS. Linux, Intel macOS and Windows currently use an explicitly selected Docker hub or an existing hub; installing a native CLI does not install a native managed service on those hosts. See the [setup guide](setup-guide.md).

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
