---
title: "Try the scripted demo"
---

# Try the scripted demo

From a Git repository with at least two tracked files, run:

```sh
npx agentscm demo
```

The command starts a throwaway Hub on loopback and simulates three agents using your repository's
tracked filenames. One agent's contested edit is refused once; its retry passes. It prints a local
status-page link and attempts to open it in your browser. No account, Docker or forge is needed,
and the demo does not change repository files. Press Ctrl-C to stop and remove the temporary Hub
data.

This is a scripted scenario using AgentSCM's real local hook endpoint. It does not launch Claude
Code, Codex or other coding agents, and it does not test real edits or model behavior. The values
shown in the status page are demonstration data based on your repository's name and file paths.

To connect your coding agents to a Hub, follow [install and setup](install.md). For a longer
Docker-based Hub walkthrough that sends scripted hook calls from two containers, see
[Two-container Hub walkthrough](two-agent-demo.md).
