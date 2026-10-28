---
title: "Autopsy: how often did your own sessions collide?"
---

# Autopsy: how often did your own sessions collide?

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

`agentscm autopsy` answers one question about the repository you are in: in the last few days, how often did two of your own Claude Code sessions edit the same file while the other still held it? The analysis is read-only and local, and needs no account or hub. If product usage telemetry is configured and enabled, running the command may record one coarse `autopsy` feature outcome for the UTC day; log contents, report results and paths are not included. See [usage telemetry](telemetry.md).

```sh
agentscm autopsy                  # last 7 days, this repository
agentscm autopsy --days 30 --no-paths
```

```text
7 days · 2 sessions · 2 near-misses · worst: billing.rs (2 sessions, 4 min apart) · merge-tree conflicts today: 1
  billing.rs — 1 near-miss
  lib.rs — 1 near-miss

Near-miss = two of your sessions touched the same file while the other still held it (the hub's 20-minute hold, renewed by activity, capped at 30 minutes). It means the hub would have warned, not that anything would have broken.
Reads Claude Code logs only; other agents are not included. Log contents and report data are not uploaded, and file contents in the logs are never kept or printed.
merge-tree check: 1 of 2 near-misses on branches that still exist and differ; it compares today's branch tips, not the files as they were then.
Read: 236 log files from ~/.claude/projects
Repository: /work/app
```

## How it works

1. It reads Claude Code's session logs, `~/.claude/projects/*/*.jsonl` and the logs of each session's subagents (`*/<session>/subagents/*.jsonl`, which count as their parent session), and keeps only the tool calls the Claude Code hook watches (`Edit`, `Write`, `MultiEdit`): the path, the session, the time and the Git branch. Edit contents are never kept or printed. Lines it cannot parse are skipped.
2. It keeps edits to files inside this repository, including its linked worktrees, from the last `--days` days, using the hub's repository-path rule.
3. It replays them in time order through the hub's contested-file rule: a session holds the files it edits for 20 minutes after its last edit, never longer than 30 minutes after its first, and a session's first edit to a file another session holds is a near-miss (the hub warns about a file once per session). Lock files and generated files are never held, as in the hub.
4. Where both sessions' branches still exist locally, it runs the same `git merge-tree` check as `agentscm collisions` and says how many of those near-misses are on a file that conflicts between the two branch tips today.

## Options

- `--days <n>`: days of history, 1 to 90. Default 7.
- `--no-paths`: show `<file>` instead of the repository's file paths.
- `--repo <dir>`: a directory inside the repository. Default: the current directory.
- `--logs <dir>`: where the Claude Code logs are. Default: `~/.claude/projects`.
- `--git <path>`: absolute Git executable. Default: the first `git` on `PATH`.

## Limits

- **Overlap, not breakage.** A near-miss is what the hub would have warned about. Most overlaps merge cleanly; the card counts the ones that did not, where it can check.
- **It can over-count and under-count.** The logs do not say when a session ended, so a session that stopped early is treated as holding its files for the full window (over-count). Edits it cannot see (shell commands, logs that were deleted, a worktree that has since been removed, another machine) are missed (under-count).
- **Claude Code only.** Other agents are not included. A session is found by the file paths it edited, not by its project folder.
- **Edits by shell commands are invisible.** Only the edit tools are read, as in the hub's hooks.
- **The merge-tree check is partial.** It needs both sessions' branches to still exist locally, differ and have commits beyond the base; it compares today's branch tips, not the files as they were when the sessions overlapped. Sessions that shared one branch or checkout cannot be checked this way. On Windows it is not run and the card says so.
- The report is a snapshot of your own logs. It is not a measurement of how often collisions happen to anyone else.
