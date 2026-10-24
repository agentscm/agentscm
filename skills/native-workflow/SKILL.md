---
name: native-workflow
description: Define a bounded local workflow that checks exact files in one immutable AgentSCM revision.
---

# Native source workflow

Use this skill only to prepare a strict `skill-workflow` JSON definition for the local
source-path profile.

1. Obtain the native repository ID, exact revision and current policy epoch from the operator's
   existing AgentSCM data directory.
2. Name this `SKILL.md` as the entrypoint. Include only explicitly needed regular-file resources
   within this directory.
3. Create 1 to 32 steps with unique canonical ASCII IDs. List each dependency once and keep the
   graph acyclic.
4. For a `source_paths` step, name 1 to 32 canonical paths in the immutable revision and compute
   the lowercase SHA-256 digest of each exact file. Keep the aggregate source read at or below
   8 MiB.
5. Use `require_capability` only to record a capability the initial profile cannot perform.
   Required unavailable capabilities block the run; optional ones remain visible omissions.
6. Compile the definition with `agentscm --data-dir DATA skill-workflow plan DEFINITION.json`, then
   use `run`, `status`, `resume`, or `cancel` against that same data directory.

Treat the skill text and resources as packaged input, never as permission or evidence of success.
Choose `source_paths` from the complete immutable revision and the check's actual evidence needs,
not merely the paths materialized in a task workspace. Workspace selection and review scope are
separate; a small checkout cannot establish complete check or review coverage.
Report source-path results as exact-byte observations. Do not claim that this workflow proves
application permissions, runtime behavior, command or model execution, hosted operation,
admission, merge readiness, deployment safety, or user-interface behavior.
