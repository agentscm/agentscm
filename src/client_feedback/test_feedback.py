"""Loopback self-check for the native binary wrapper, using only synthetic credentials."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BINARY = os.environ.get("AGENTSCM_FEEDBACK_BIN")
REVISION = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
WORK = "wrk_" + "a" * 32


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.server.seen.append((self.path, None))
        self.send_response(200)
        self.end_headers()
        time.sleep(self.server.delay)
        body = ({"id": WORK, "state": "finished", "generation": 7}
                if self.path.startswith("/v1/hub/standup/work/") else
                {"enabled": True, "revision": REVISION} if self.server.enabled else {"enabled": False})
        try:
            self.wfile.write(json.dumps(body).encode())
        except BrokenPipeError:
            pass

    def do_POST(self):
        self.server.seen.append((self.path, json.loads(self.rfile.read(int(self.headers["Content-Length"])))))
        self.send_response(202)
        self.end_headers()
        self.wfile.write(b'{"received":true}')

    def log_message(self, *_):
        pass


@unittest.skipUnless(BINARY, "set AGENTSCM_FEEDBACK_BIN to the candidate binary")
class NativeFeedback(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.home = Path(temp.name)
        self.root = self.home / "checkout's space"
        self.root.mkdir()
        config = self.home / ".config/agentscm"
        self.env = {"PATH": os.environ.get("PATH", os.defpath), "HOME": str(self.home),
                    "XDG_CONFIG_HOME": str(config.parent), "GIT_CONFIG_NOSYSTEM": "1",
                    "NO_PROXY": "127.0.0.1", "AGENTSCM_TELEMETRY": "off"}
        subprocess.run(["git", "init", "-q", str(self.root)], check=True, env=self.env, cwd=self.root)
        self.hub = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.hub.daemon_threads = True
        self.hub.enabled, self.hub.delay, self.hub.seen = True, 0, []
        threading.Thread(target=self.hub.serve_forever, daemon=True).start()
        self.addCleanup(self.hub.server_close)
        self.addCleanup(self.hub.shutdown)
        token = self.home / "token"
        token.write_text("synthetic-token")
        token.chmod(0o600)
        config.mkdir(parents=True)
        (config / "connections.json").write_text(json.dumps({"connections": [{
            "root": str(self.root), "hub": f"http://127.0.0.1:{self.hub.server_port}",
            "token_file": str(token)}]}))

    def invoke(self, args, body, **env):
        result = subprocess.run([BINARY, "--no-telemetry", "feedback", *args],
                                input=json.dumps(body), text=True, capture_output=True,
                                env={**self.env, **env}, cwd=self.root, timeout=10)
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        return result.stdout

    def event(self, client, vscode=False):
        value = {"hook_event_name": "SessionStart", "source": "startup",
                 "cwd": str(self.root), "session_id": "native-parent"}
        if client == "cursor":
            value = {"hook_event_name": "sessionStart", "workspace_roots": [str(self.root)],
                     "conversation_id": "native-parent"}
        elif client == "copilot":
            value = ({**value, "source": "new"} if vscode else
                     {"source": "new", "cwd": str(self.root), "sessionId": "native-parent"})
        return value

    def test_five_native_clients_offer_the_same_bounded_report_protocol(self):
        cases = [(client, False) for client in ("claude", "codex", "cursor", "copilot", "gemini")]
        for client, vscode in [*cases, ("copilot", True)]:
            with self.subTest(client=client, vscode=vscode):
                raw = self.invoke(["start", "--client", client], self.event(client, vscode))
                reply = json.loads(raw)
                text = (reply["additional_context"] if client == "cursor" else
                        reply["additionalContext"] if client == "copilot" and not vscode else
                        reply["hookSpecificOutput"]["additionalContext"])
                if vscode:
                    self.assertEqual(reply["hookSpecificOutput"]["hookEventName"], "SessionStart")
                self.assertIn("Only the top-level agent", text)
                self.assertNotIn("synthetic-token", text)
                command = next(line.split(" <<", 1)[0] for line in text.splitlines()
                               if " --no-telemetry feedback submit " in line)
                argv = shlex.split(command.replace("WORK_ID", WORK))
                self.assertEqual(argv[:4], [BINARY, "--no-telemetry", "feedback", "submit"])
                result = subprocess.run(argv,
                    input=json.dumps({"surface": "completion", "friction": "none", "impact": "none",
                                      "experience": "as_expected", "observation": None, "idea": None}),
                    text=True, capture_output=True, env=self.env, cwd=self.root, timeout=10)
                self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "", ""))
                path, payload = self.hub.seen[-1]
                self.assertEqual(path, "/api/v1/agent-feedback")
                self.assertEqual((payload["version"], payload["client"], payload["generation"]), (3, client, 7))
                self.assertEqual(payload["session"], "native-parent")

    def test_unenrolled_and_helper_sessions_are_silent_and_do_not_submit(self):
        self.hub.enabled = False
        self.assertEqual(self.invoke(["start", "--client", "claude"], self.event("claude")), "")
        self.assertEqual(len(self.hub.seen), 1)
        self.hub.seen.clear()
        for client in ("claude", "codex", "cursor", "copilot", "gemini"):
            self.assertEqual(self.invoke(["start", "--client", client],
                                        {**self.event(client), "parent_session_id": "parent"}), "")
        self.assertEqual(self.hub.seen, [])

    def test_checkout_modules_and_python_environment_cannot_shadow_stdlib(self):
        (self.root / "json.py").write_text("raise RuntimeError('untrusted checkout module')\n")
        raw = self.invoke(["start", "--client", "claude"], self.event("claude"),
                          PYTHONPATH=str(self.root), PYTHONHOME=str(self.root))
        self.assertIn("Optional AgentSCM feedback", json.loads(raw)["hookSpecificOutput"]["additionalContext"])

    def test_feedback_connection_ignores_ambient_proxy(self):
        proxy = f"http://127.0.0.1:{self.hub.server_port}"
        raw = self.invoke(["start", "--client", "claude"], self.event("claude"),
                          http_proxy=proxy, no_proxy="", NO_PROXY="")
        self.assertIn("Optional AgentSCM feedback", json.loads(raw)["hookSpecificOutput"]["additionalContext"])
        # The same listener acts as origin and proxy. A proxy request would carry an absolute URL.
        self.assertEqual(self.hub.seen, [("/api/v1/agent-feedback/eligibility", None)])

    def test_missing_python_and_slow_collector_leave_the_host_running(self):
        self.assertEqual(self.invoke(["start", "--client", "claude"], self.event("claude"), PATH=""), "")
        self.assertEqual(self.hub.seen, [])
        self.hub.delay = 4
        start = time.monotonic()
        self.assertEqual(self.invoke(["start", "--client", "claude"], self.event("claude")), "")
        self.assertLess(time.monotonic() - start, 3.5)
        self.assertEqual(self.hub.seen, [("/api/v1/agent-feedback/eligibility", None)])


if __name__ == "__main__":
    unittest.main()
