# Explicit authenticated Git transport

> **Experimental (alpha).** HTTPS uses the standard Git smart-HTTP protocol;
> SSH uses an explicitly selected OpenSSH-compatible client. Local synthetic
> qualification does not establish live GitHub/GitLab permissions or deployment.

These profiles extend [local Git remote exchange](git-remote.md) over the same
owned export mirror, fixed export-event mapping, durable operation journal,
exclusive lease and no-replay recovery. They transfer exact SHA-1 commits through
standard Git. They do not create PRs/MRs, merge branches, approve native Changes,
move native targets or import full Git history.

Choose one canonical repository URL and branch per new mirror. Accepted URLs are
`https://host[:port]/owner/repository.git` or
`ssh://user@host[:port]/owner/repository.git`, including nested GitLab groups.
Only simple ASCII repository paths are supported. URL passwords, HTTPS user
information, query/fragment parameters, percent-encoded paths, scp shorthand,
plain HTTP, `file://`, `git://` and remote helper protocols are refused. Redirects
are refused; select the final authorized repository URL explicitly.

## HTTPS credentials and TLS

Supply a private regular JSON file owned by the invoking user, at an absolute
path, without a symlink and with no group/other permissions (normally mode 0600):

```json
{"username":"OPERATOR_SELECTED_USERNAME","password":"OPERATOR_SUPPLIED_TOKEN"}
```

Choose the username and token type supported by your explicitly approved
GitHub, GitLab or custom server/account. This command does not discover accounts,
issue tokens, refresh credentials or infer repository permissions.
Credential rotation is an explicit operator action on the selected input file.

```sh
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git \
  --credential-file /private/credentials/git.json bind-network \
  --url https://github.com/OWNER/REPOSITORY.git --branch agentscm/proposal
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git \
  --credential-file /private/credentials/git.json observe
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git \
  --credential-file /private/credentials/git.json publish publish-1 --event 1
```

Omitting `--expected-oid` requires an absent destination branch through the
server's create-only ref lease. For an existing branch, supply its exact observed
commit and export a descendant (normally from a mirror seeded with that Git
base). The command does not bypass server hooks, protected branches or other
server admission policy. Rejected writes retain uncertainty for inspection.

TLS verification is always enabled. Default trust uses the Git installation's
system TLS roots. For a private CA, pass `--ca-file /absolute/ca.pem` both when
binding and on subsequent calls. The binding stores its SHA-256, not its path;
changed or missing trust input is refused. Trust files must be regular files,
not symlinks, owned by the user or root and not writable by group/others. There
is no insecure-TLS switch, proxy configuration or automatic trust bootstrap.
Each connection uses a mode-0600 copy of the exact digest-verified CA or
known-hosts bytes in a controller-created mode-0700 temporary directory, retained
until the local transport process group completes and then removed. Concurrent
rotation of the original file cannot substitute different trust after checking.
A controller crash can leave these public trust copies for operator cleanup;
credential and identity contents are never copied into them.

The built-in scoped credential helper returns username/password only over Git's
private helper pipe after checking exact HTTPS host, port and repository path.
Secrets do not appear in Git arguments, binding/journal records or transport
errors. The helper never stores credentials. Client environment/config isolation
refuses ambient credential helpers, netrc discovery, interactive prompts and
original per-command Git overrides. Server diagnostics are not returned as raw
errors, because an untrusted peer could echo credentials. This is not a memory
erasure guarantee or protection against a hostile local owner inspecting its
own process/files.

## SSH identity and host trust

Provide an absolute private identity-file path and an independently verified
`known_hosts` file. Do not populate trust by blindly accepting a new host key.
The binding stores the known-hosts file's SHA-256; every invocation must supply
the same trusted contents. A new trust profile requires a new mirror/binding.

```sh
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git \
  --ssh /usr/bin/ssh --ssh-identity /private/credentials/git-key \
  --known-hosts /private/credentials/known_hosts bind-network \
  --url ssh://git@gitlab.com/OWNER/REPOSITORY.git --branch agentscm/proposal
```

Supply the same SSH options for observe, fetch, publish and recover. The private
key must be a regular user-owned file without group/other permissions. The
identity and known-hosts paths cannot contain control characters, `%` or `$`, because OpenSSH expands
tokens and environment variables in these arguments after shell parsing. The
profile uses strict host-key checking, the selected identity only, no agent,
password or keyboard-interactive authentication, no user/global SSH config,
proxy/jump command, connection sharing, local command or host-key updates.
Encrypted identities requiring interaction are unsupported. No real identity,
account or known-hosts file is discovered or modified.
Adjacent automatic SSH certificates and hardware/token providers are disabled;
this profile accepts an explicitly selected software private key only.

The actual OpenSSH option parser and synthetic SSH command/Git-protocol path
are tested locally. They do not qualify authentication against a real SSH daemon
or account. That requires separately authorized server/account evidence.

## Explicit fetch and uncertain outcomes

Use the same operations as the local profile, with the selected access options:

```sh
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git \
  --credential-file /private/credentials/git.json fetch inbound-1 --expected-oid FULL_OBSERVED_COMMIT
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git lookup publish-1
agentscm --data-dir ./native-data git-remote --mirror ./owned-mirror.git \
  --credential-file /private/credentials/git.json recover publish-1
```

A missing network advertisement means **not advertised**, not proven absent:
servers can hide refs. Recovery records `observed_unadvertised` for that case;
it does not call the absent preimage verified. This is an explicit historical
observation, after which an operator may choose a new operation ID. Every new
publication still has an exact current-ref/create-only lease and ancestry
check. It cannot overwrite a hidden existing branch by assuming absence.

`pending`/`unknown` operations are never automatically retried, and repeating the
same ID returns its saved result. Recovery only reads; it does not repeat a
push. A remote server may finish a request after the client terminates, so a
recovery read is not proof of remote quiescence or which actor wrote the ref.
Read-only observation, exact ref leases and historical receipts remain separate
from native admission. Inspect the state, choose any subsequent attempt
explicitly under a new ID and preserve the old receipt.

Local processes retain the reviewed timeout/output/process-group bounds.
Network transfer has no hard disk quota or whole-workflow latency guarantee.
The remote transport does not add a scheduler, automatic reconciliation/landing,
GitLab MR creation, repository archival or live authority cutover.
