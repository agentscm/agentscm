---
title: "Replaying same-file changes against Rust regions"
---

# Replaying same-file changes against Rust regions

Could two Works reserve different parts of a file? The offline experiment for [#689](https://github.com/agentscm/agentscm/issues/689) compares the final edits in three pairs of pull requests. The motivating case in [#618](https://github.com/agentscm/agentscm/issues/618) provides a collision the experiment must preserve.

## Reproduce the comparison

From a checkout with the pinned Git objects, run the historical test explicitly:

```sh
cargo test --locked --test integration reservation_replay::replay_pinned_reservation_history -- --ignored --nocapture
```

The test reads each change against its own parent, then maps both changes onto a common base. This matters when intervening commits shifted line numbers or when the second PR landed after the first. The locked test dependencies include the Rust parser. The default test suite exercises the comparison on synthetic source files, including input with missing or ambiguous evidence.

## Cases

| Pull requests | Shared paths and question |
|---|---|
| [#609](https://github.com/agentscm/agentscm/pull/609) and [#615](https://github.com/agentscm/agentscm/pull/615) | Both edited `async fn hook` in `src/hook_cli.rs`. Issue #618 reported blocking on that file and `src/hub/hook.rs`. The final diffs also shared `tests/integration/agent_gates.rs` and `docs/agents.md`. |
| [#625](https://github.com/agentscm/agentscm/pull/625) and [#632](https://github.com/agentscm/agentscm/pull/632) | The only shared path was `src/hub/mcp.rs`. One changed `tool_list`, the other changed `picker_call` and `workflow_status`. Their open PR windows were disjoint. |
| [#602](https://github.com/agentscm/agentscm/pull/602) and [#607](https://github.com/agentscm/agentscm/pull/607) | The only shared path was `src/hub/mcp.rs`. One changed `tool_list`. The other changed `mcp` and inserted `setup_failure` between siblings. Their PR windows overlapped for 38 minutes and 29 seconds. |

## Recorded refusals

The repository's 14-day Hub digest ending November 24, 2026 at 15:07:50 UTC reported 13 refused edits and 13 repeated edits with complete collision counts. The digest groups collisions and supplies no source revisions. The underlying `edit_denied` event records paths and holders. Neither source identifies the exact edits both Works would have claimed at the time. See the [digest field definitions](../work-insights.md#digest-stale-work-and-repeated-collisions).

Of the 13 refusals in the digest, zero could be linked to both edits from the evidence available for this replay. All 13 therefore have an unknown outcome under finer reservations. The 13 repeats are a separate event type. A replay with revision-linked refusal records should count a potential avoided refusal only when every blocking path separates, keep unlinked or partly mapped refusals in the denominator, and report known collisions separately. The three selected PR pairs cannot supply an avoidance rate for this sample.

## Replay results

The squash-commit replay classified one pair as overlapping and two as separate. For #609/#615 it found a common function in `src/hook_cli.rs`. The other three shared paths remained unknown. For #625/#632 all changes in the shared file mapped to separate function bodies. For #602/#607 the new top-level function mapped to the gap between `hub_failure` and `append_actionable_status` on the common base.

The scanner parses source with `syn` 2.0.119. It resolves edits inside functions in modules and simple inherent methods by their qualified names, then compares intervals in the same base blob. A complete new top-level function can claim a whitespace-only gap bounded by the same two functions across revisions. A duplicate name, an unsupported structural edit or a parse failure leaves that hunk unknown. A known common interval suffices to establish overlap even when other hunks are unknown. Separation requires positive attribution for every hunk on every shared path.

This replay measures the regions covered by landed changes. Original tool calls, intermediate edits and intended reservations may differ. Changes to different functions can still interact through behavior or conflict at merge. The experiment supplies examples for a future trial and leaves the live reservation policy at whole files.
