# agentscm

```
npx agentscm downloads the release binary from GitHub Releases and checks its SHA-256 checksum before it runs; no install scripts. Usage reporting is off until you run `agentscm telemetry on`.
```

## Usage

```sh
npx agentscm              # starts the setup guide (`agentscm quickstart`)
npx agentscm --yes        # passes flags to `agentscm quickstart`
npx agentscm --version    # prints the version (`--help` also passes through)
npx agentscm uninstall    # runs any other subcommand as given
npx agentscm tray         # optional macOS status-bar companion
```

The binary is kept in `~/.agentscm/bin` (`%USERPROFILE%\.agentscm\bin` on Windows). The checksum is always verified; provenance is verified when the GitHub CLI (`gh`) is installed and signed in, and otherwise the script says it was not checked. The notice above and that one-line result are printed when a version is first downloaded (not before `--help`); later runs start AgentSCM directly, and `AGENTSCM_INSTALL_VERBOSE=1` prints them on every run. Supported: Linux and macOS on x64 and arm64, and Windows on x64 (a subset of commands). Elsewhere, use Docker.

Details, manual verification and Docker: [docs/install.md](https://github.com/agentscm/agentscm/blob/main/docs/install.md). Source and license (FSL-1.1-ALv2): https://github.com/agentscm/agentscm

The tray uses the same verified binary; there is no separate app install. It is an unsigned,
unnotarized macOS alpha and does not update itself. **Start at Login** is off until enabled in
the tray menu.
