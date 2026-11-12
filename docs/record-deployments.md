---
title: "Record a deployment from a script"
---

# Record a deployment from a script

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

The Hub learns that a merged change is live from the forge's own deployments. A GitHub Actions job with an `environment:`, or a deploy service's GitHub app, records one for you. A deploy that runs from a shell script records nothing, so the Hub cannot tell the change went out. `agentscm deployed` tells the Hub: one line at the end of the script.

```sh
./deploy.sh production
agentscm deployed --env production
```

It tells the Hub that the checkout's `HEAD` is live in `production`, and prints one line saying what was recorded:

```text
Recorded 1f3c9a07b2de live in production for acme/widgets on GitHub.
```

The Hub then records the deployment on GitHub or GitLab itself, with the credential it already holds for the repository: its GitHub App installation, or its GitLab connection's token. The script needs no GitHub or GitLab token. It signs in to the Hub the way every other client command does: the Hub connection `agentscm quickstart` or `agentscm client setup` saved for the checkout, or `AGENTSCM_HUB_URL` and `AGENTSCM_HUB_TOKEN_FILE` (an absolute path to a file holding an agent token for the repository) on a machine with no client setup, such as a CI runner. The token's repository must be the one the checkout's `origin` names.

## Options

- `--env NAME` (required): the environment, such as `production` or `staging`; at most 200 characters.
- `--commit REV`: the commit deployed, anything `git rev-parse` resolves. Default `HEAD`.
- `--status success|failure|in_progress`: default `success`. Run it with `in_progress` before the deploy and `success` or `failure` after, to show a deploy under way.
- `--url URL`: where the deployed app is reached. GitHub keeps it on the deployment status; GitLab deployments carry no URL, so it is not recorded there.

## What the forge needs

- GitHub: the Hub's GitHub App needs the repository permission **Deployments: Read and write**. An App made from the Hub's manifest after this command was added asks for it. For an older App, open the App's settings on GitHub (Developer settings, GitHub Apps, the App, Permissions & events), set Deployments to Read and write and save; then accept the new permission on each installation (the account's Settings, GitHub Apps, Configure, Review request). A token connection needs a token with that permission.
- GitLab: the connection's token, with the `api` scope, for a member with at least the Developer role.

When the credential may not write deployments, the Hub answers `403` with the error `forge_permission_missing` and a message naming the permission, and records nothing, on the forge or in the Hub.

## Running it again

It is safe to run twice. For the same commit and environment the Hub reuses the newest deployment the forge has, and writes a status only when the newest one differs; otherwise the command prints `Already recorded ...; nothing changed.` and nothing is written. A new status (say `failure` after `success`) goes on that same deployment, and a new environment gets its own deployment. The Hub records one deployment at a time per repository, so two runs at once cannot make two.

On GitHub the deployment is created with `auto_merge: false` and no required status checks, because the commit is already deployed: nothing is merged and no check is waited on. Its `payload` is `{"agentscm": "recorded"}`, so a later read of the forge knows the Hub wrote it. On GitLab the environment is created if it is new, and the deployment's ref is the project's default branch.

## In a GitHub Actions workflow

A job with `environment:` already records a deployment. For a workflow that deploys without one, the `actions/deployed` step in this repository wraps the command. The job needs a checkout of the commit, and the agent token in a secret:

```yaml
steps:
  - uses: actions/checkout@v4
  - run: ./deploy.sh production
  - uses: agentscm/agentscm/actions/deployed@main
    with:
      environment: production
      hub-url: https://hub.example.com
      hub-token: ${{ secrets.AGENTSCM_AGENT_TOKEN }}
      url: https://widgets.example.com
```

Its inputs are `environment`, `hub-url` and `hub-token` (all required), `status` (default `success`), `url`, `commit` (default the checked-out `HEAD`) and `version`, the npm version of `agentscm` it runs when the command is not already on `PATH` (default `latest`). The workflow's own `GITHUB_TOKEN` is not used. Pin the action to a release tag or commit instead of `main` in production workflows.

## What the Hub shows

A merged publication's `external` object lists each deployment of its merge commit with its `source`: `recorded` for one written this way, `forge` for one the Hub read from the forge that it did not write (see [external CI and deployment outcomes](hub.md#external-ci-and-deployment-outcomes) and the [HTTP API](api.md#hub-publication-external-outcomes)). A recorded deployment is shown for any repository, on GitHub or GitLab, whether or not the Hub watches it. Only the commit you record is marked: when one deploy ships several merges, record the newest merge commit, and the earlier merges in it stay unmarked.
