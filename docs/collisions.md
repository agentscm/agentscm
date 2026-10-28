# Early collision warnings (Git only)

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

`agentscm collisions` tells you, before a merge, which in-flight pieces of work in an existing Git repository change the same files. It is read-only, needs no migration and does not use the AgentSCM data directory. Status: implemented for local Git repositories; not a lock, not a prevention guarantee and not part of the native store.

**Run it only on repositories you trust.** It runs `git status` and `git merge-tree`, which execute any clean filters and custom merge drivers configured in the repository (the same as `git status` or `git merge` in your shell). Hooks and fsmonitor are turned off, but filters and merge drivers are not.

```bash
agentscm collisions                                # the repository in the current directory
agentscm collisions --git-dir ~/dev/app --worktrees
```

Options:

- `--git-dir <dir>`: a working directory of the repository. Default: the current directory.
- `--base <ref>`: what "in flight" is measured against. Default `origin/HEAD`, then `main`.
- `--refs <ref>...`: check only these refs (anything that resolves to a commit). Default: every local and remote branch with commits not in the base.
- `--active-days <n>`: without `--refs`, skip branches whose tip was committed more than `n` days ago. Default 14; `0` checks every branch. The report counts them in `inactive_refs_excluded`.
- `--include-merged`: also scan branches whose changes the base already has through a squash or rebase merge. By default they are skipped, because a branch that has already landed cannot collide with new work; the report counts them in `merged_refs_excluded`, and the text report says "N branches skipped: already merged". Branches merged by ancestry (plain merge or fast-forward) never appear, with or without this flag.
- `--worktrees`: also check each worktree's uncommitted changes (staged, unstaged and untracked), found with `git worktree list`. This is the many-agents-on-one-machine case.
- `--git <path>`: absolute Git executable. Default: the first `git` on `PATH`, then `/usr/bin/git`. Git 2.43 or newer is required; older Git is reported as an error, not guessed around.

- `--json` / `--text`: choose the output. On a terminal the default is a short text report; when stdout is not a terminal (a pipe, a script, the container) the default is JSON, as before. `--json` always prints the JSON below; `--text` forces the text report.

## The text report

```text
Checked 2 branches/worktrees against main · 1 pair touches the same files · 1 would conflict in git

Would conflict (git merge-tree):
  feat-invoices  ↔  feat-rounding   billing.rs

Same file is a warning, not proof of conflict. Uncommitted changes are never checked for conflicts. Add --worktrees to include uncommitted work, --json for the full report.
```

It lists conflicting pairs first (with the files that conflict), then pairs that only touch the same files (with the files both changed), up to 10 of each and 3 paths per pair, then the files that appear in the most pairs. It says when the scan was cut short or refs were skipped. Names are shown without control, invisible or bidirectional characters, with `"` shown as `'`, and cut at 300 characters. The numbers are the same as in the JSON report.

## What it reports (JSON)

One JSON value on stdout, `{"ok": true, "collisions": {...}}`. The exit status is nonzero only on errors, never because collisions were found.

```json
{
  "ok": true,
  "collisions": {
    "profile": "agentscm-git-collisions/v1",
    "git_version": "git version 2.50.1 (Apple Git-155)",
    "base": {"name": "main", "commit": "<oid>"},
    "active_days": 14,
    "inactive_refs_excluded": 0,
    "merged_refs_excluded": 0,
    "items": [
      {"id": "feat-a", "kind": "ref", "commit": "<oid>", "refs": ["refs/heads/feat-a"], "worktree": null, "paths_total": 3},
      {"id": "worktree:/work/app-2", "kind": "worktree", "commit": "<HEAD oid>", "refs": [], "worktree": "/work/app-2", "paths_total": 2}
    ],
    "collisions": [
      {
        "level": "conflict",
        "items": ["feat-c", "feat-d"],
        "shared_paths_total": 1,
        "shared_paths": ["h"],
        "git_merge": {"result": "conflict", "conflicted_paths_total": 1, "conflicted_paths": ["h"]}
      },
      {
        "level": "same-file",
        "items": ["feat-a", "worktree:/work/app-2"],
        "shared_paths_total": 1,
        "shared_paths": ["f"],
        "git_merge": {"result": "not-evaluated", "reason": "uncommitted changes have no commit to merge"}
      }
    ],
    "truncated": null,
    "skipped": [{"name": "feat-x", "reason": "..."}],
    "notes": ["..."]
  }
}
```

- An item is a branch tip (several refs at the same commit are one item) or a worktree with uncommitted changes. Branch paths are `git diff base...tip`; worktree paths come from `git status`. Renames count as both paths. Lists are capped at 50 paths, with the full count in `*_total`.
- `same-file` (warning): two items changed the same path (or one adds a file where the other has a directory of that name). **This is not proof that Git would conflict**; edits to different parts of a file usually merge cleanly.
- `conflict`: `git merge-tree --write-tree` between the two branch tips reported a conflict. `git_merge.result` is `clean`, `conflict`, `not-evaluated` (a side is uncommitted) or `error` (with a reason).
- Items on one line of history (one branch contains the other) and a worktree on or above a branch's tip (that branch's own author) are not reported against each other.
- Items that could not be analysed (for example no merge base with `--base`) are listed in `skipped`, not dropped silently.
- Items and collisions are sorted deterministically; `conflict` entries come first.
- `truncated` is `null` when the scan finished. The CLI never truncates. The service's `GET /v1/collisions` stops after 60 seconds (`"deadline"`; items it had no time to list are in `skipped`) or before its JSON would exceed 4 MiB (`"output"`), and returns the collisions found so far, in scan order, so a cut can drop a `conflict` while keeping an earlier `same-file`. A truncated report is not "nothing more found"; run the CLI for the full one. If the deadline passes before the branch or worktree list is read, the request fails instead.

## Limits

- **Unreported work is invisible.** Only branches the repository already has (run `git fetch` for remote ones) and worktrees `git worktree list` knows about are checked. A teammate's unpushed clone, or uncommitted work in a clone you cannot see, will not appear. No result means "nothing found here", not "no collision".
- It is a snapshot at the moment you run it, with no notifications, intent declarations or locks. Run it often (see the hooks below).
- Same-file is a path-level signal only. Semantic breakage (a caller of a function another branch changed) is not detected.
- Uncommitted changes are never evaluated for Git conflicts.
- Cost grows with the number of items that share paths: each such pair runs one merge simulation. A repository with a hundred stale branches took about 17 seconds to scan in a debug build; narrow with `--refs` or `--base` for hooks.
- It runs through the same hardened Git layer as `git-snapshot` (cleared environment, no hooks, literal pathspecs, bounded output and time) and never takes refs or paths through a shell. Refs are resolved to commit IDs before use and names starting with `-` are rejected. `git merge-tree` writes unreferenced objects into the repository's object database; `git gc` removes them. `git status` in each worktree runs as it would in your shell, so only point this at repositories you trust.
- Per-worktree `git status` honours `.gitignore`, `assume-unchanged` and `skip-worktree`: ignored or hidden changes are not reported.
- The default scope skips refs already merged into the base (by ancestry, or by a squash or rebase merge: every commit has a patch-equal commit in the base, the base holds every file the branch changed exactly as the branch has it, or one base commit made the branch's whole change; `--include-merged` scans the squash and rebase ones anyway) and branches idle for more than `--active-days` (14; the service always uses 14), and stops with an error beyond 200 refs/items (narrow with `--refs` or a smaller `--active-days`). A stale branch can still collide with live work; run with `--active-days 0` to see it. An empty repository, or one with neither `origin/HEAD` nor `main`, needs `--base`.
- Non-UTF-8 file names are shown lossily, so two names that differ only in invalid bytes can look equal.
- In the service, each Git command runs in its own process group, so a timeout also kills merge drivers and clean filters it started. Only a timeout does: a background child left by a command that exits normally, or one that leaves the group (for example with `setsid`), keeps running. The CLI keeps Git in the terminal's process group so Ctrl-C reaches it; its 60-second per-command timeout kills Git but not what Git started. Git's own `--version` probe has no timeout.
- The output budget bounds the report, not the scan: each item's full path list (up to 16 MiB of Git output) is held in memory while pairs are compared.
- `--active-days` uses the tip's committer date, which whoever writes the branch controls; a backdated branch drops out of the default scan (it is still counted in `inactive_refs_excluded`).
- `merge-tree` failures (timeout, unwritable object database) appear as `git_merge.result: "error"` with the pair still reported `same-file`; the command's exit status stays 0.

## Running in a container

The published image runs the same scan, so nothing needs installing. Choose a version from
[Releases](https://github.com/agentscm/agentscm/releases), replace `<version>` below with its
version without the leading `v`, and run from your main checkout:

```sh
docker run --rm --network none --user "$(id -u):$(id -g)" -v "$PWD:/repo" \
  "ghcr.io/agentscm/agentscm:<version>" collisions --git-dir /repo --worktrees
```

- Only run it on repositories you trust: Git runs the repository's configured filters and merge drivers.
- `--user` makes the container the owner of the mount, which Git requires.
- The mount is read-write only because `git merge-tree` writes unreferenced objects (`git gc` removes them). Your files, refs and index are not changed.
- Run it from the main checkout, not from inside a linked worktree: a linked worktree's `.git` file points outside the mounted directory, so Git cannot open it in the container.
- Other linked worktrees outside the mount are listed under `skipped` in the report, so their uncommitted edits are not covered. Mount their common parent directory to include them.

### Serving the report with Compose

`docker compose up` from a checkout of this repository serves `GET /v1/collisions` on `http://127.0.0.1:7331` (see [the HTTP API](api.md#whos-touching-what)). Compose mounts the checkout you ran it from (or `AGENTSCM_REPO=/path/to/repo`) and starts `serve --collisions-repo /repo`. The mount is read-write for the same `git merge-tree` reason.

The service runs as uid 10001, and Git refuses a repository owned by another user, so the request may fail unless that uid owns and can write the checkout; use the `docker run --user` form above otherwise. The container binds `0.0.0.0` inside the container (`compose.yaml`, like the image, passes `--public`), and Compose publishes the port on `127.0.0.1` only. Compose always builds the image from this checkout (the first build takes a few minutes because SQLite is compiled in), so it does not need the published image.

## Telling your agent

The script below prints only the collisions that involve the current checkout (its branch or its uncommitted changes) and always exits 0, so it advises and never blocks. Save it as `agentscm-collisions.sh` and `chmod +x` it. It needs `agentscm` and `jq` on `PATH`.

```sh
#!/bin/sh
# Advisory: print collision warnings that involve this checkout.
top=$(git rev-parse --show-toplevel) || exit 0
branch=$(git symbolic-ref --quiet --short HEAD)
agentscm collisions --git-dir "$top" --worktrees 2>/dev/null | jq -r \
  --arg wt "worktree:$top" --arg br "refs/heads/$branch" '
  .collisions as $c
  | [$c.items[] | select(.id == $wt or any(.refs[]; . == $br)) | .id] as $mine
  | $c.collisions[] | select(any(.items[]; . as $i | $mine | index($i)))
  | "agentscm \(.level): \(.items | join(" and ")) both change \(.shared_paths | join(", ")) (same-file is not proof of conflict)"'
exit 0
```

Two places to run it:

- **Any agent that commits** (Claude Code, Codex, others): call it from `.git/hooks/pre-commit`, e.g. `exec /path/to/agentscm-collisions.sh`. Agents read the output of the commit they run. Hooks are per clone and not committed.
- **Claude Code, every prompt:** a `UserPromptSubmit` hook whose standard output is added to the agent's context. In `.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {"hooks": [{"type": "command", "command": "/path/to/agentscm-collisions.sh"}]}
    ]
  }
}
```

The script itself was exercised against a temporary repository; wiring it into a particular agent's hook system should be checked against that tool's current documentation before relying on it.
