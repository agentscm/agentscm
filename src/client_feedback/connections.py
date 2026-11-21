"""Read the existing checkout-bound AgentSCM connection without changing it."""
import json
import os
import subprocess

from clients import safe_path


def git(cwd, *args):
    out = subprocess.run(("git", "-C", cwd) + args, capture_output=True, text=True, timeout=1)
    return out.stdout.strip() if out.returncode == 0 else ""


def connection_for(cwd):
    """The connection whose root is cwd's checkout or the main checkout of its worktree."""
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.join(os.environ.get("HOME", ""), ".config")
    with open(os.path.join(base, "agentscm", "connections.json")) as f:
        connections = json.load(f)["connections"]
    top = git(cwd, "rev-parse", "--show-toplevel")
    if not safe_path(top):
        return None
    top = os.path.realpath(top)
    for c in connections:
        if os.path.realpath(c["root"]) == top:
            return c
    common = git(cwd, "rev-parse", "--path-format=absolute", "--git-common-dir")
    if not safe_path(common):
        return None
    common = os.path.realpath(common)
    for c in connections:
        if os.path.realpath(c["root"]) != os.path.dirname(common):
            continue
        gitdir = git(cwd, "rev-parse", "--absolute-git-dir")
        # Only a registered linked-worktree directory and its backlink can borrow the main connection.
        if not safe_path(gitdir) or os.path.dirname(os.path.realpath(gitdir)) != os.path.join(common, "worktrees"):
            return None
        with open(os.path.join(gitdir, "gitdir")) as binding:
            target = binding.read(4097).rstrip("\n")
        if len(target) > 4096 or os.path.realpath(os.path.join(gitdir, target)) != os.path.join(top, ".git"):
            return None
        return c
