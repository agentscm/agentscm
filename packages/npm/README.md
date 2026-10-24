# agentscm

```
npx agentscm downloads the AgentSCM release binary for your platform from
GitHub Releases, verifies its SHA-256 checksum and build provenance, and then
starts the setup guide. It does not run install scripts. After verification it
marks that binary invocation as a verified install; the binary records one
installation indicator only after setup explains pseudonymous product telemetry
and the configured choice permits it. Disable this before any send with
`--no-telemetry`, `AGENTSCM_TELEMETRY=off`, or `DO_NOT_TRACK=1`.
The setup guide previews repository, hook and Hub changes; `agentscm uninstall`
reverses setup. Manage usage reporting with `agentscm telemetry status` and
`agentscm telemetry off`.
```

## Usage

```sh
npx agentscm              # starts the setup guide (`agentscm quickstart`)
npx agentscm --yes        # passes flags to `agentscm quickstart`
npx agentscm --version    # prints the version (`--help` also passes through)
npx agentscm uninstall    # runs any other subcommand as given
npx agentscm tray         # optional macOS status-bar companion
```

The binary is kept in `~/.agentscm/bin` (`%USERPROFILE%\.agentscm\bin` on Windows). The checksum is always verified; provenance is verified when the GitHub CLI (`gh`) is installed and signed in, and otherwise the script says it was not checked. Supported: Linux and macOS on x64 and arm64, and Windows on x64 (a subset of commands). Elsewhere, use Docker.

Details, manual verification and Docker: [docs/install.md](https://github.com/counterbranch/agentscm/blob/main/docs/install.md). Source and license (FSL-1.1-ALv2): https://github.com/counterbranch/agentscm

The tray uses the same verified binary; there is no separate app install. It is an unsigned,
unnotarized macOS alpha and does not update itself. **Start at Login** is off until enabled in
the tray menu.
