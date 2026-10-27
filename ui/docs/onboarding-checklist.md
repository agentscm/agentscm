# New teammate on a clean Mac: one real run

The bar: from the install line to an agent coordinating in a repository in **under 2 minutes**, with
**no further commands** and **one browser click**. The mock walkthrough
(`tests/e2e/onboarding.spec.ts`) covers the Hub pages; this run covers everything the mock can't:
the installer, the menu bar app, GitHub, the agent and the timing.

Run it once per release candidate, on a Mac account that has never had AgentSCM, with a stopwatch.

## Before you start

- A macOS user account with no `~/.config/agentscm`, no `agentscm` on `PATH` and no AgentSCM menu
  bar app. A new macOS user is the cleanest.
- A GitHub account with **Triage or higher** on a repository connected to the Hub (the teammate), and a
  second account with only **Read** on a public connected repository (for step 7).
- A Hub admin signed in to the Hub in another browser (for steps 8–9).
- One coding agent installed (Claude Code or Codex) and a fresh clone of the connected repository.

## The run

Start the stopwatch at step 1. Write down anything that makes you stop and think: that is a bug.

| # | Do | Expect, word for word where quoted | Pass |
|---|----|------------------------------------|------|
| 1 | Open the Hub's sign-in page. Under **Set up this computer**, choose **Copy**. | The command names this Hub. Copy says Copied. | ☐ |
| 2 | In Terminal, `cd` into the clone, paste the command and run it. | Short output: no telemetry notice wall, no JSON. It asks once whether to keep AgentSCM in the menu bar after restarts, and, for Claude Code, "Install the AgentSCM plugin for Claude Code in all your projects? It runs a quick check whenever an agent starts and only contacts a Hub in repos connected to it. [Y/n]" | ☐ |
| 3 | Answer, and let the browser open. | GitHub's sign-in page. **The one click:** Authorize. | ☐ |
| 4 | Look at the tab it returns to. | "This computer is signed in." · "<computer> is signed in to <Hub> as <login> on GitHub. You won't need to do this again." | ☐ |
| 5 | Look at the menu bar. | The AgentSCM icon shows **Connected**. No "Not connected" rows, no `?` rows. | ☐ |
| 6 | Start the agent in the clone. Ask it to edit any file. | One line from the agent: "AgentSCM: coordinating on <owner/repo>". No setup command and no repository files written without asking. The agent appears under Dashboard › Team on the Hub. **Stop the stopwatch.** | ☐ |
| 7 | Repeat steps 1–3 as the **Read-only** account. | The landing page, the agent and the menu bar app all say the same sentence: "<login> has Read on public repository <owner/repo>; ask a repository owner for Triage or higher". There's no "membership" and no JSON anywhere. | ☐ |
| 8 | As the admin, open **Setup › Admin** on the Hub. | **Turned away** lists the Read-only login with that reason and a count. **Connected repositories** lists the repository with 1 computer. | ☐ |
| 9 | As the teammate, open the account menu → **Your computers**. Revoke the computer from step 4. | It's listed with the repository. After Revoke: "<computer> was revoked." Within a minute the menu bar app shows the computer is signed out, and the agent's next edit says so in one line. | ☐ |
| 10 | Restart the Mac (if you answered Yes in step 2). | The menu bar app comes back on its own and shows Connected or what needs attention. | ☐ |

Step 3's single click holds once the Hub's browser-first computer sign-in ships. Until then quickstart asks for a one-time device code from GitHub or GitLab instead; note the extra steps and time them anyway.

**Time (steps 1–6):** ____ s  (pass under 120 s)

## Record

- Release / commit:
- Hub:
- macOS version, agent and version:
- Every place you stopped to think, with a screenshot:
- Every message that differed between the agent, the menu bar app and the Hub page:
