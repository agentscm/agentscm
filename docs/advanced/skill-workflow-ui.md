# Skill workflow dashboard lookup

> **Experimental (alpha).** This authenticated read surface displays retained local workflow state. It does not execute a workflow or assess admission.

This is an early UX prototype for testing the basic list, run, and step flow.
Pagination, large run histories, filtering/search, and navigation still need
substantial refinement. The current page controls and page-local filters are an
initial implementation, not a finished workflow browsing experience.

The hub separates **Coordination**, **Workflows**, **Activity**, and **Health** into repository
tabs. Workflows starts with a table of up to 16 retained runs, newest-created first. Previous/Next
controls browse older pages; search and status filters apply only to the current page. Open a run
to inspect its dependency stages, then select a step for review findings, coverage, and retained
evidence. Breadcrumbs and browser history support navigation between the list, run, and step.
An older run can also be opened by ID. The browser sends authenticated, same-origin requests to:

```text
GET /v1/hub/skill-workflows
GET /v1/hub/skill-workflows?before={next_cursor}
GET /v1/hub/skill-workflows/{run_id}
```

The request cannot select a native data directory, repository, source revision, or remote URL. The hub uses the repository on the current token and an operator-configured native binding. Read-only hub tokens can use the lookup. Run, resume, cancel, approval, and admission remain outside the dashboard.

The run view separates four kinds of information:

- **Next action** explains whether to fix a failed step, resolve structured review findings, rerun on fresh source, or inspect a stale Change binding.
- **Recorded run** is historical journal state. It preserves `completed_with_omissions`, `failed`, and `blocked` states, command-attempt observations, and bounded Claude review findings. Paths under a step’s **Evidence & tests** tab are retained source observations.
- **Source freshness** and **Change and target freshness** keep current repository/revision/policy facts separate from the exact Change, review-context and protected-target binding. Neither recompile the workflow or prove that the run can resume.
- **Native landing gate** remains **Not assessed**. A successful recorded run does not approve, merge, deploy, or authorize its revision.

The Workflows tab reports an empty run list and an unconfigured or unavailable native binding without
hiding the rest of the hub dashboard. It also distinguishes a missing run, rejected authorization,
malformed input or response, and a network failure. Changing the run ID clears the displayed
result; changing the authenticated token or repository also clears the input and result. An
in-flight request is cancelled when it becomes obsolete.

Responses are treated as untrusted data. The browser limits each response to 4 MiB and validates
versioned fields, known states, string sizes, at most 16 recent summaries, at most 32 steps, at most
32 source observations per step and bounded structured review fields before rendering. Cursors and creation sequences use canonical decimal strings to avoid JavaScript integer rounding. Unexpected
or partial data is shown as malformed rather than filled with optimistic defaults.
