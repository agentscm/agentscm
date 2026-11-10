---
title: "Agent tokens for a local Hub without a forge"
---

# Agent tokens for a local Hub without a forge

A Hub normally gives an agent its token through a forge sign-in: `agentscm hub login`, then
`agentscm hub token create` or `agentscm client setup`. A Hub you run on your own computer to try
AgentSCM, with no GitHub App or GitLab connection, has no one to sign in, so it cannot hand out
tokens that way. `agentscm hub local-token` mints one straight into the Hub's own database instead.

```sh
export AGENTSCM_DATABASE_URL=sqlite://$HOME/agentscm-hub/hub.sqlite   # the database `hub serve` uses
agentscm hub local-token --repo acme/widgets --login maya --name claude-a > a.token
agentscm hub local-token --repo acme/widgets --login sam --name claude-b > b.token
chmod 600 a.token b.token
```

- **Operator only.** It works on the database file, so only someone who can read and write the
  Hub's database can run it. There is no HTTP route for it, and it never contacts a forge.
- **Printed once.** The token goes to standard output and nowhere else; the Hub stores only its
  digest. Keep it in a file only you can read.
- **Local people.** `--login` becomes a Hub user on host `localhost`, so it never matches a forge
  account, even if the Hub is connected to a forge later. The same login is always the same user,
  so two tokens with one login belong to one person.
- **One repository.** The token works only in `--repo`. It expires after `--hours` (default 24,
  at most 720).
- **Not for a shared Hub.** On a Hub that people sign in to, use `agentscm hub token create` or
  `agentscm client setup`, which tie each token to the person's forge account.

Point an agent's hooks at the Hub with the token file, as in [work intents](intents.md):

```sh
AGENTSCM_HUB_TOKEN_FILE=a.token agentscm hook claude --hub-url http://127.0.0.1:7332 --repo acme/widgets
```

To see two agents meet on one file without a forge or any agent at all, try the
[scripted demo](scripted-demo.md) first.
