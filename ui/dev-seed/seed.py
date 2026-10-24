"""Dev-only: drives a running hub's real HTTP and MCP endpoints with synthetic agents.

Usage: seed.py TOKENS_ENV REPO_ROOT [--live]
--live keeps posting a hook or checkpoint every ~20 s so the UI visibly moves.
"""
import json
import random
import sys
import time
import urllib.error
import urllib.request

import os

HUB = f"http://127.0.0.1:{os.environ.get('AGENTSCM_DEV_HUB_PORT', '7332')}"
tokens = dict(line.strip().split("=", 1) for line in open(sys.argv[1]) if "=" in line)
ROOT = sys.argv[2]
HOOK = {"IVY": "claude", "ORION": "codex", "JUNO": "claude", "PAX": "claude", "RUNE": "claude"}


def call(who, method, path, body=None, ok=(200,)):
    req = urllib.request.Request(HUB + path, method=method,
                                 data=None if body is None else json.dumps(body).encode(),
                                 headers={"Authorization": f"Bearer {tokens[who]}", "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            status, text = r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        status, text = e.code, e.read().decode()
    if status not in ok:
        raise SystemExit(f"{who} {method} {path} -> {status}: {text[:400]}")
    return status, (json.loads(text) if text else None)


def hook(who, session, event, file=None):
    body = {"session_id": session, "cwd": ROOT, "hook_event_name": event}
    if file:
        if HOOK[who] == "codex":
            body["tool_name"] = "apply_patch"
            body["tool_input"] = {"command": f"*** Begin Patch\n*** Update File: {file}\n@@\n+x\n*** End Patch\n"}
        else:
            body["tool_name"] = "Edit"
            body["tool_input"] = {"file_path": f"{ROOT}/{file}"}
    return call(who, "POST", f"/v1/hub/hook/{HOOK[who]}", body)[1]


def tool(who, name, arguments):
    body = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}}
    return call(who, "POST", "/mcp", body)[1]


def denied(reply):
    return (reply or {}).get("hookSpecificOutput", {}).get("permissionDecision") == "deny"


ops = iter(range(1, 10**9))
work = {}  # key -> (who, session, item)


def start(key, who, session, title, scope, next_action, branch=None, issue=None):
    body = {"title": title, "session": session, "scope": scope, "next_action": next_action,
            "branch": branch or f"work/{key}"}
    if issue:
        body["source"] = {"provider": "github", "host": "github.com", "repository": "counterbranch/agentscm", "issue": str(issue)}
    else:
        body["request_key"] = f"seed-{key}"
    item = call(who, "POST", "/v1/hub/standup/start", body)[1]["item"]
    work[key] = (who, session, item)
    return item


def checkpoint(key, kind, summary, next_action=None, scope=None, ok=(200,), **extra):
    who, session, item = work[key]
    body = {"work_id": item["id"], "operation_id": f"seed-{next(ops)}", "generation": item["generation"],
            "session": session, "expected_updated_seq": item["updated_seq"], "kind": kind,
            "summary": summary, "next_action": next_action, **extra}
    if scope is not None:
        body["scope"] = scope
    status, reply = call(who, "POST", "/v1/hub/standup/checkpoint", body, ok=ok)
    if status == 200:
        work[key] = (who, session, reply["item"])
    return status, reply


def refresh(key):
    who, session, item = work[key]
    work[key] = (who, session, call(who, "GET", f"/v1/hub/standup/work/{item['id']}")[1])


def seed():
    # Live file activity: sessions, held files, a refusal, a granted and a declined handoff.
    hook("IVY", "ivy-1", "SessionStart")
    for f in ["ui/src/App.tsx", "ui/src/components/Header.tsx", "ui/src/styles.css"]:
        hook("IVY", "ivy-1", "PreToolUse", f)
    hook("ORION", "orion-1", "SessionStart")
    for f in ["src/hub/coordination.rs", "src/hub/coordination_http.rs"]:
        hook("ORION", "orion-1", "PreToolUse", f)
    hook("JUNO", "juno-1", "SessionStart")
    hook("JUNO", "juno-1", "PreToolUse", "docs/hub.md")
    hook("PAX", "pax-1", "SessionStart")
    hook("PAX", "pax-1", "PreToolUse", "src/hub/store_pg.rs")
    hook("RUNE", "rune-1", "SessionStart")
    hook("RUNE", "rune-1", "PreToolUse", "src/hub/intent.rs")
    # Juno touches Ivy's stylesheet: refused, then Ivy hands it over.
    assert denied(hook("JUNO", "juno-1", "PreToolUse", "ui/src/styles.css")), "expected a refusal"
    tool("IVY", "agentscm_handoff", {"path": "ui/src/styles.css", "accept": True, "session": "ivy-1"})
    hook("JUNO", "juno-1", "PreToolUse", "ui/src/styles.css")
    # Pax wants Orion's coordination.rs: refused, Orion declines.
    assert denied(hook("PAX", "pax-1", "PreToolUse", "src/hub/coordination.rs")), "expected a refusal"
    tool("ORION", "agentscm_handoff", {"path": "src/hub/coordination.rs", "accept": False, "session": "orion-1",
                                       "reason": "Mid-refactor of the request projection; free in ~30 min."})

    # Durable shared work in every state.
    start("shell", "IVY", "ivy-1", "Rebuild the hub status page shell",
          ["ui/src/App.tsx", "ui/src/components/Header.tsx", "ui/src/components/Dashboard.tsx"],
          "Wire the new header to whoami and health", issue=412)
    checkpoint("shell", "progress", "Header now shows repository, token expiry and hub health.",
               "Move the activity feed into a collapsible panel",
               source_refs=[{"kind": "repository_path", "path": "ui/src/components/Header.tsx", "line": 41}])
    start("coord", "ORION", "orion-1", "Bound coordination request history",
          ["src/hub/coordination.rs", "src/hub/coordination_http.rs"], "Clip receipts at 100 per target", issue=398)
    checkpoint("coord", "decision", "Receipts and replies are clipped server-side; the page flags omissions.",
               "Add a regression test for clipped receipts")
    checkpoint("coord", "blocker", "PostgreSQL retention migration must land first: the clip query needs its index.",
               "Wait for the retention migration, then rerun the clip tests")
    start("docs", "JUNO", "juno-1", "Document the skill workflow hub",
          ["docs/advanced/skill-workflow-hub.md", "docs/hub.md"], "Draft the run-state table", issue=405)
    checkpoint("docs", "progress", "Run states and landing bindings documented with examples.",
               "Review wording of the 'not assessed' admission state")
    checkpoint("docs", "handover", "Out of context budget; drafts are complete, needs a technical review pass.",
               "Reviewer: check the landing binding section against skill_workflow.rs")
    start("retention", "PAX", "pax-1", "Retention migration for PostgreSQL",
          ["src/hub/store_pg.rs", "src/hub/migrations/0014_coordination_retention.pg.sql"],
          "Write the down-migration notes")
    checkpoint("retention", "pause", "Paused while CI Postgres image is rebuilt.", "Resume once CI image is green")
    start("intent", "RUNE", "rune-1", "Fix intent renewal race", ["src/hub/intent.rs"], "Add a renewal test", issue=377)
    checkpoint("intent", "progress", "Renewal now checks the hold generation before extending.", "Run the full intent suite")
    checkpoint("intent", "finish", "Race fixed and covered by a concurrent renewal test.", None,
               source_refs=[{"kind": "commit", "oid": "512d3aa" + "0" * 33, "path": "src/hub/intent.rs"}])
    start("csp", "PAX", "pax-2", "Tighten CSP headers on /ui/", ["src/hub/ui.rs"], "Drop unsafe-eval")
    checkpoint("csp", "finish", "CSP now allows scripts only from the hub; inline styles stay for the component library.")
    start("mcp", "RUNE", "rune-2", "Expose work overview over MCP", ["src/hub/mcp.rs"], "Add the tool schema")
    checkpoint("mcp", "progress", "Tool schema drafted; returns the same bounded overview as HTTP.", "Add MCP integration test")

    # Directed coordination requests: widening a scope into someone else's paths queues a request.
    s, r = checkpoint("shell", "progress", "Need the shared work stylesheet too.", "Restyle the work lanes",
                      scope=["ui/src/App.tsx", "ui/src/components/Header.tsx", "ui/src/components/Dashboard.tsx",
                             "src/hub/mcp.rs"], ok=(200, 409))
    pending = r.get("coordination_request") if s == 409 else None
    s, r = checkpoint("mcp", "progress", "Overview tool needs the intent listing.", "Reuse intent::list",
                      scope=["src/hub/mcp.rs", "src/hub/intent.rs", "src/hub/coordination_http.rs"], ok=(200, 409))
    responded = r.get("coordination_request") if s == 409 else None
    start("index", "PAX", "pax-3", "Add a request-history index", ["src/hub/store_sqlite.rs"], "Benchmark the clip query")
    s, r = checkpoint("index", "progress", "Index needs the coordination store too.", "Pair with Orion",
                      scope=["src/hub/store_sqlite.rs", "src/hub/coordination.rs"], ok=(200, 409))
    resolved = r.get("coordination_request") if s == 409 else None

    # Orion receives and replies to Rune's request; Orion and Pax settle theirs.
    if responded:
        target = next(t for t in responded["targets"] if t["lead"]["agent"] == "codex-orion")
        _, r = call("ORION", "POST", "/v1/hub/standup/coordination/ack", {
            "request_id": responded["id"], "target_work_id": target["work_id"], "session": "orion-1",
            "expected_updated_seq": responded["updated_seq"]})
        seq = r["updated_seq"]
        call("ORION", "POST", "/v1/hub/standup/coordination/reply", {
            "request_id": responded["id"], "operation_id": f"seed-{next(ops)}", "target_work_id": target["work_id"],
            "session": "orion-1", "expected_updated_seq": seq, "kind": "propose_split",
            "note": "Take coordination_http.rs read handlers; I keep the store and projection."})
    if resolved:
        target = resolved["targets"][0]
        _, r = call("ORION", "POST", "/v1/hub/standup/coordination/ack", {
            "request_id": resolved["id"], "target_work_id": target["work_id"], "session": "orion-1",
            "expected_updated_seq": resolved["updated_seq"]})
        seq = r["updated_seq"]
        _, r = call("ORION", "POST", "/v1/hub/standup/coordination/reply", {
            "request_id": resolved["id"], "operation_id": f"seed-{next(ops)}", "target_work_id": target["work_id"],
            "session": "orion-1", "expected_updated_seq": seq, "kind": "join",
            "note": "Join as helper on the index; I'll review the migration."})
        refresh("index")
        checkpoint("index", "finish", "Index landed with Orion's review; clip query is 4x faster.")
        listed = call("PAX", "GET", "/v1/hub/standup/coordination?limit=100")[1]["requests"]
        seq = next(q["updated_seq"] for q in listed if q["id"] == resolved["id"])
        call("PAX", "POST", "/v1/hub/standup/coordination/resolve", {
            "request_id": resolved["id"], "operation_id": f"seed-{next(ops)}", "session": "pax-3",
            "expected_updated_seq": seq})
    print("seeded: pending", bool(pending), "responded", bool(responded), "resolved", bool(resolved), flush=True)


def live():
    files = ["ui/src/App.tsx", "ui/src/components/Header.tsx", "src/hub/coordination.rs", "src/hub/mcp.rs"]
    owners = {"ui/src/App.tsx": ("IVY", "ivy-1"), "ui/src/components/Header.tsx": ("IVY", "ivy-1"),
              "src/hub/coordination.rs": ("ORION", "orion-1"), "src/hub/mcp.rs": ("RUNE", "rune-1")}
    notes = ["Tests green locally.", "Split a helper out for readability.", "Addressed review nits.",
             "Rebased on main.", "Measured: p95 under 3 ms on loopback."]
    while True:
        time.sleep(20)
        try:
            if random.random() < 0.5:
                who, session = owners[random.choice(files)]
                hook(who, session, "PreToolUse", random.choice([f for f, o in owners.items() if o[0] == who]))
            else:
                key = random.choice(["shell", "mcp"])
                refresh(key)
                checkpoint(key, "progress", random.choice(notes), work[key][2].get("next_action"))
        except SystemExit as e:
            print("live tick skipped:", e, flush=True)


if __name__ == "__main__":
    seed()
    if "--live" in sys.argv:
        live()
