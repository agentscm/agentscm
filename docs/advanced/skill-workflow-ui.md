# Skill workflow dashboard lookup

> **Experimental (alpha).** This authenticated read surface displays retained local workflow state. It does not execute a workflow or assess admission.

The hub dashboard includes a compact **Skill workflow run** card. Paste a run ID and select **Lookup** to read the retained run mapped to the token's repository. The browser sends an authenticated, same-origin request to:

```text
GET /v1/hub/skill-workflows/{run_id}
```

The request cannot select a native data directory, repository, source revision, or remote URL. The hub uses the repository on the current token and an operator-configured native binding. Read-only hub tokens can use the lookup. Run, resume, cancel, approval, and admission remain outside the dashboard.

The card separates three kinds of information:

- **Recorded run** is historical journal state. It preserves `completed_with_omissions`, `failed`, and `blocked` states and lists each recorded step. Paths under **Source checks** are retained source observations.
- **Present applicability** reports whether the recorded repository, policy epoch, and immutable revision still match the current bound native authority. A current binding does not recompile the workflow or prove that the run can resume.
- **Admission** remains **Not assessed**. A successful recorded run does not approve, merge, deploy, or authorize its revision.

The card reports an unconfigured or unavailable native binding without hiding the rest of the hub dashboard. It also distinguishes a missing run, rejected authorization, malformed input or response, and a network failure. Changing the run ID clears the displayed result; changing the authenticated token or repository also clears the input and result. An in-flight request is cancelled when it becomes obsolete.

Responses are treated as untrusted data. The browser limits the response to 4 MiB and validates versioned fields, known states, string sizes, at most 32 steps, and at most 32 source observations per step before rendering. Unexpected or partial data is shown as malformed rather than filled with optimistic defaults.
