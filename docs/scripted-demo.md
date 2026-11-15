---
title: "See it in action"
---

# See it in action

Watch three agents work in your repository at the same time, and see what AgentSCM does when two of
them reach for the same file. It takes one command and needs no account, Docker or setup.

From any Git repository with at least two tracked files, run:

```sh
npx agentscm demo
```

## What you'll see

The command starts a temporary Hub on this computer and opens its status page in your browser. If
the browser doesn't open, it prints the link.

Three simulated agents start work on files from your repository. When one of them goes to edit a
file another agent is already working on, AgentSCM refuses that edit once and says who holds the
file. The agent's retry goes through. The status page shows who is working on what.

Press Ctrl-C to stop. The temporary Hub and its data are removed.

## What it doesn't do

- It changes nothing in your repository and needs no GitHub or GitLab connection.
- The agents are scripted. It doesn't start Claude Code, Codex or any other coding agent, and no
  model runs. The demo calls AgentSCM's real hook endpoint, the same one your agents will use, and
  the values on the page are demonstration data built from your repository's name and file paths.

## Next

- [Install AgentSCM](install.md) to connect your own coding agents, or
  [let your coding agent set it up](install.md#let-your-coding-agent-set-it-up).
- [Two-container Hub walkthrough](two-agent-demo.md): a longer demo in Docker, with two containers
  sending hook calls to one Hub.
