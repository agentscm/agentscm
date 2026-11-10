---
title: "Agent tokens for a local Hub without a forge"
---

# Agent tokens for a local Hub without a forge

A Hub normally gives an agent its token through a forge sign-in: `agentscm hub login`, then
`agentscm hub token create` or `agentscm client setup`. A Hub you run on your own computer to try
AgentSCM, with no GitHub App or GitLab connection, has no one to sign in, so it cannot hand out
tokens that way. `agentscm hub local-token` mints one straight into the Hub's own database instead.

Start the Hub once with `agentscm hub serve` from the same release first, so its database exists.
Then, in a shell that keeps new files private:

```sh
umask 077
export AGENTSCM_DATABASE_URL=sqlite://$HOME/agentscm-hub/hub.sqlite   # the database `hub serve` uses
agentscm hub local-token --repo acme/widgets --login maya --name claude-a > ~/agentscm-hub/a.token
agentscm hub local-token --repo acme/widgets --login sam --name claude-b > ~/agentscm-hub/b.token
```

- **Operator only.** It works on the database file, so only someone who can read and write the
  Hub's database can run it, and that person can mint a token for any login. There is no HTTP
  route for it, and it never contacts a forge.
- **Never changes the database's shape.** It refuses a database that does not exist yet, or one
  from another release that would need migrating, instead of creating or upgrading it. Run it with
  the same release as the Hub.
- **Local Hubs only.** It refuses a Hub that has a forge connection or anyone who signed in through
  a forge. There, use `agentscm hub token create` or `agentscm client setup`, which tie each token
  to a person's forge account.
- **Printed once.** The token goes to standard output and nowhere else; the Hub stores only its
  digest. Keep it in a file only you can read, as `umask 077` above does.
- **Local people.** `--login` (lowercased) becomes a Hub user on host `localhost`. It never matches
  a forge account or a forge entry in `AGENTSCM_ADMINS`, so a local person is never a Hub admin.
  The same login is always the same user.
- **No sign-in checks.** A local token skips what a forge sign-in checks: `AGENTSCM_ALLOWED_USERS`
  and the person's role on the forge repository.
- **One repository.** The token works only in `--repo`. It expires after `--hours` (default 24,
  at most 720).

Give the token file to an agent's hooks with an absolute path, as in [work intents](intents.md):

```sh
AGENTSCM_HUB_TOKEN_FILE=$HOME/agentscm-hub/a.token agentscm hook claude --hub-url http://127.0.0.1:7332 --repo acme/widgets
```

For the AgentSCM MCP tools, add `http://127.0.0.1:7332/mcp` as a remote MCP server with the header `Authorization: Bearer <token>`, as [supported agents](agents.md) describes.

To see two agents meet on one file without a forge or any agent at all, try the
[scripted demo](scripted-demo.md) first.
