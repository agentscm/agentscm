---
title: "Reservations of code blocks within a file"
---

# Reservations of code blocks within a file

> **Decision: not adopted.** Reservations stay whole files. This note records why, and what evidence would reopen the question. Nothing here is implemented.

Reservations are whole files. [Agreed overlaps](work-overlaps.md) ([#324](https://github.com/agentscm/agentscm/issues/324)) let two leads share a named file, and #324 left reservations below file level out "unless a design shows they can stay honest about what was checked". This note evaluates the proposal of [#618](https://github.com/agentscm/agentscm/issues/618): reserve syntax blocks (functions, `impl` items, modules) found with tree-sitter, and fall back to the whole file where no grammar applies.

## The motivating case

On 2026-11-16 the Works for #609 and #611 blocked each other over `src/hub/hook.rs` and `src/hook_cli.rs`. They looked like separate functions. The merged changes (#609, and #615, which fixed #611) say otherwise. Going by the function each diff hunk falls in (Git's guess from hunk headers, so approximate), both changed:

- `fn pascal` in `src/hub/hook.rs`;
- `async fn hook` in `src/hook_cli.rs`;
- `mod tests` in both files.

Function-level blocks would have refused the same pair. A top-level `mod tests` block would collide on almost every change that adds a test, so a useful unit would have to be each test function, nested inside a module.

## Attributing edits

The design rests on the Hub knowing which blocks an edit touched. Today it knows only paths:

- The hook client extracts the files an edit names (`edit_paths`, `patch_paths` and `table_edit_paths` in `src/hook_cli.rs`), and the client connection sends "for an edit the files it names and never what it writes" ([agents](../agents.md)). Edit text never leaves the machine.
- Claude Code `Edit` and `MultiEdit` carry `old_string`/`new_string` fragments, and Codex `apply_patch` carries hunks with a few context lines. Mapping either to a block means reading and parsing the whole file on disk in the hook. `Write` carries the full new text.
- Shell writes are reported after the fact, as changed paths from a Git status snapshot, and never refused. `agentscm_check` takes paths and never refuses. Clients without hooks report nothing.

So the hook client would compute block names and send them, which is a new privacy boundary: identifiers such as `impl Store::finish` leave the machine where today only paths do. The Hub cannot check them either, because it holds no file contents and no Git objects ([#352](https://github.com/agentscm/agentscm/issues/352)).

An edit the hook cannot attribute (a shell write, a formatter run, a parse failure, a hookless client) has to claim the whole file, or block reservations hold only for the edits that happen to be attributable. This is the gap that made [#495](https://github.com/agentscm/agentscm/pull/495) refuse automatic release: the Hub cannot tell "no recorded edit" from "not edited". A holder whose block claim widened silently to the whole file, or never did, would make every block-level "clear" reply unverifiable.

## Block identity

With no Git objects, a block could be named only by a path the client computes, such as `src/hub/store.rs#impl Store::fn finish`. A rename or move reports a new block. Byte or line ranges drift with every edit in the holder's checkout. Each choice either drops the holder's claim on a rename or keeps a stale name that matches nothing. Either way, the reply would say what was checked against a name, not against the code.

## Fallback, manifests and registries

Files without a grammar (Markdown, YAML, shell), and files that fail to parse mid-edit, would stay whole-file.

For manifests and registries (`Cargo.toml`, `package.json`, route tables, barrel files such as `mod.rs` or `index.ts`):

| Treatment | Gives | Leaves unchecked |
|---|---|---|
| Block reservations from TOML or JSON grammars (one block per table or key) | two Works adding different dependencies proceed | the lockfile both changes regenerate, and order-dependent registries |
| Whole-file (today) | one holder at a time | nothing new; agents get the warn-once and agreed overlaps |
| Never held, like lockfiles (`never_hold` in `src/hub/session.rs`) | no refusals | everything; Git reports a textual conflict at merge, and a semantic one is not reported |

They stay whole-file. No recorded case has two Works blocked on a manifest. If one appears, adding the file to `never_hold` is the cheaper change, and it should say what it leaves unchecked, as the [Hub guide](../hub.md) does for lockfiles.

## Honesty

Separate blocks can still conflict: textually when the blocks are adjacent, and in behavior when one block calls another. Checking that stays with Git (`agentscm collisions`, [#118](https://github.com/agentscm/agentscm/issues/118)) and [#352](https://github.com/agentscm/agentscm/issues/352). A block-level reply would have to say "these named blocks did not overlap in reported edits", and a reader would still need the Git check.

## Cost

- **Dependency.** `tree-sitter` plus a grammar per language. The grammars are C sources built with `cc`, they add third-party notices, and the hook is the same binary as the Hub (v0.8.0 release binaries are 63–74 MiB). The size each grammar adds was not measured.
- **Hook latency.** The `PreToolUse` hook has a 5 second timeout (`src/client_plugin.rs`). One parse takes milliseconds, but the hook would read and parse the full file on every edit, and the largest files here are the ones that get contended: `src/hub/store_sqlite.rs` and `src/hub/store_pg.rs` are each over 18,000 lines.
- **Reuse.** The `counterbranch/discovery` repository uses tree-sitter 0.27 with ten grammars (Rust included) for authorization scanning, not block extraction. Its license forbids derivative works and redistribution, so its code cannot be reused. The language mapping it would offer is about 20 lines, so an implementation would depend on the crates directly.
- **Store.** A scope entry kind beyond the exact file and `dir/**` that `spec_key` (`src/hub/intent.rs`) accepts, a changed comparison in `scope_overlap_reasons` (`src/hub/work.rs`), and invalidation of agreed overlaps when a block claim changes. That is a tier L change touching the edit hook and stored reservations.

## Decision

Reservations stay whole files. The motivating case would not have been separated, unattributable edits would leave block claims unverifiable, and the change is a large one across the hook, the store and the dependency tree. Two Works that need the same file keep using what exists:

- the warn-once refusal, which lets a repeat through as "a patch to your own lines" judged from the holder's task text;
- [agreed overlaps](work-overlaps.md) on named files;
- a per-file release of part of a hold (`release_paths`, [stand-up](../standup.md)).

Reopen this when recorded cases show Works blocked on the same file whose changes fell in different nested items (test functions, methods) and whose edits came through attributable edit tools. A follow-up would then start with Rust only, nested items, and an unattributable edit widening the claim to the whole file, and would be planned as its own tier L issue.
