## What and why

<!-- The change and the reason for it. -->

## How it was tested

<!-- Commands run and results. State anything you did not run. -->

<!-- Same-repository PRs: local validation attestation envelope SHA-256 and the agentscm/local-validation status. If none is available, say so and name the `gh workflow run` dispatch fallback for hosted coverage (a same-repository branch; a fork author dispatches in their own fork or asks for maintainer adoption). -->

## Limits

<!-- Known gaps, unsupported cases, follow-ups. -->

## AI assistance

- [ ] No AI assistance
- [ ] Assisted (a tool helped; I wrote or fully reviewed every line)
- [ ] Agent-authored (an agent wrote the change)

Tool and what I verified myself: <!-- required unless "No AI assistance" -->

## Checklist

- [ ] Pre-push checks for this change ran (for Rust changes, `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings` and focused nextest filters), and the attestation or unsigned full-suite result for its selected commands is reported above
- [ ] Tests and docs are updated for behavior changes
- [ ] No secrets, personal data, session links or transcripts
