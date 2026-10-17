# Dependency inventory

`Cargo.lock` is the authoritative exact-version graph. The direct dependencies
are pinned to `ed25519-dalek 2.2.0`, `serde 1.0.228`, `serde_json 1.0.151`, and
`sha2 0.10.9`.

`ed25519-dalek` disables default features and enables only `std` and `zeroize`.
The active graph was inspected with `cargo tree --locked --offline --edges all`.
It contains the strict Ed25519 primitive and its selected curve/hash support,
Serde derive and JSON support, and their build dependencies. It contains no
network, key-discovery, signing, credential, daemon, AgentSCM, or AWS crate.

Cargo's lockfile also retains optional packages from dependency manifests even
when their features are not selected on the inspected host. The notice and SBOM
therefore inventory the full 39-package third-party lockfile closure rather than
only the active feature tree. This conservative inventory avoids silently
dropping platform-specific or optional locked packages from distribution review.

No dependency was downloaded or installed while preparing this package. Lock
resolution and tree inspection used the existing local Cargo registry in
offline mode.
