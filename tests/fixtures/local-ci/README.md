# Local CI version 1 conformance fixtures

These synthetic `Requirements` documents model command shapes inspected in existing repositories. Repositories, workflow bytes, runner pins, dependency identities and resource sizes are invented test data. They contain no adopter source and do not qualify or execute a real script, mobile device or Actions runtime.

| Fixture | Shape | Canonical requirements digest |
|---|---|---|
| `spicy-style.json` | One explicit reviewed Bash script, one-hour deadline, six declared workers | `1fc385bdf442f869740db038992150f73b8fd68629e07c8f4cab9602c3e98328` |
| `mobile-style.json` | Separate Android and iOS Bun commands, two-hour deadlines, explicit host profiles | `3d5736489b1bf0a1c7f334727b35057f335af3cc1eb895cacec06dfea9a075ff` |

The workflow commitments cover the exact synthetic byte strings `synthetic reviewed script\n` and `synthetic mobile commands\n`, respectively, where `\n` denotes one newline byte. The integration test loads these bytes through a synthetic source reader, resolves the full inventory and verifies complete and missing-cell reports. The digest is SHA-256 of `agentscm.local-ci.requirements.v1`, one zero byte and the schema's canonical JSON bytes. Pretty-printing these fixture files is not canonicalization.

See the [contract and migration guide](../../../docs/advanced/local-ci-plans.md) for schema rules, the inspected source revisions, internal script-cache limits and unqualified runtime behavior. Changing these vectors requires deliberate review of the contract and their expected digest assertions.
