"""Real Kimi Code hook consumption with an isolated profile and local provider.

No external model service or user credentials are used. The test activates
Namedrop against a fake Herdr, replaces only fixture executables, then verifies:
- a Kimi `message` from the installed wrapper reaches the provider request;
- a confirmed handoff block prevents any provider request.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPO = Path(__file__).resolve().parent.parent
BLOCK = {
    "hookSpecificOutput": {
        "permissionDecision": "deny",
        "permissionDecisionReason": "Collaboration message sent.",
    },
    "message": "Collaboration message sent.",
}
CONTEXT = {"message": "Namedrop workspace roster. Native Kimi hook consumption marker."}


class Provider:
    def __init__(self):
        self.requests = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length)
                outer.requests.append((self.path, body))
                def chunk(delta, reason=None):
                    return {
                        "id": "dummy-response",
                        "object": "chat.completion.chunk",
                        "created": 0,
                        "model": "dummy",
                        "choices": [{"index": 0, "delta": delta, "finish_reason": reason}],
                    }
                chunks = [
                    chunk({"role": "assistant"}),
                    chunk({"content": "dummy ok"}),
                    chunk({}, "stop"),
                    {"id": "dummy-response", "object": "chat.completion.chunk", "created": 0,
                     "model": "dummy", "choices": [],
                     "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}},
                ]
                response = ("".join(f"data: {json.dumps(item)}\n\n" for item in chunks)
                            + "data: [DONE]\n\n").encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(response)))
                self.end_headers()
                self.wfile.write(response)

            def log_message(self, *_):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.server.server_port}/v1"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def run(args, *, env, cwd, input=None, timeout=60, check=True):
    result = subprocess.run([str(arg) for arg in args], env=env, cwd=cwd, input=input,
                            capture_output=True, text=True, timeout=timeout)
    if check and result.returncode != 0:
        raise AssertionError(f"command failed {args}:\nstdout={result.stdout}\nstderr={result.stderr}")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/debug/namedrop")
    args = parser.parse_args()
    kimi = shutil.which("kimi")
    if not kimi:
        print("BLOCKED: kimi is not on PATH", file=sys.stderr)
        return 2
    binary = args.binary.resolve()
    if not binary.is_file():
        raise AssertionError(f"build Namedrop first: {binary}")
    fixture = Path(tempfile.mkdtemp(prefix="namedrop-kimi-native-"))
    evidence = fixture / "evidence"
    evidence.mkdir()
    retained = REPO / ".scratch/kimi-native" / fixture.name
    provider = Provider()
    successful = False
    try:
        home = fixture / "home"
        kimi_home = fixture / "kimi-home"
        state = fixture / "state"
        for path in (home, kimi_home, state):
            path.mkdir()
        fake_herdr = fixture / "herdr"
        fake_herdr.write_text("""#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
if args[:2] == ['workspace', 'list']:
    result = {'workspaces': [{'workspace_id': 'workspace-1'}]}
elif args[:2] == ['worktree', 'list']:
    result = {'worktrees': [{'path': '/repo', 'open_workspace_id': 'workspace-1'}]}
elif args[:2] == ['pane', 'list']:
    result = {'panes': [{'pane_id': 'pane-1', 'terminal_id': 'terminal-1', 'tab_id': 'tab-1',
                         'workspace_id': 'workspace-1', 'agent': 'kimi',
                         'agent_session': {'value': 'session-1'}, 'agent_status': 'idle'}]}
elif args[:2] == ['config', 'path']:
    result = {'path': os.environ['HERDR_CONFIG_PATH']}
else:
    result = {}
print(json.dumps({'result': result}))
""".replace("import json, sys", "import json, os, sys"))
        fake_herdr.chmod(0o700)
        fixture_binary = fixture / "namedrop-candidate"
        shutil.copyfile(binary, fixture_binary)
        fixture_binary.chmod(0o700)
        env = os.environ.copy()
        env.update({
            "HOME": str(home),
            "KIMI_CODE_HOME": str(kimi_home),
            "CLAUDE_CONFIG_DIR": str(home / ".claude"),
            "CODEX_HOME": str(home / ".codex"),
            "HERDR_BIN_PATH": str(fake_herdr),
            "HERDR_CONFIG_PATH": str(fixture / "herdr-config.toml"),
            "HERDR_SOCKET_PATH": str(fixture / "herdr.sock"),
            "HERDR_WORKSPACE_ID": "workspace-1",
            "HERDR_PANE_ID": "pane-1",
            "KIMI_DISABLE_TELEMETRY": "1",
            "KIMI_CODE_NO_AUTO_UPDATE": "1",
            "NO_COLOR": "1",
        })
        env["HERDR_PLUGIN_STATE_DIR"] = str(state)
        activation = run([fixture_binary, "--plugin-state", state,
                          "--herdr-socket", env["HERDR_SOCKET_PATH"],
                          "--herdr-workspace", "workspace-1", "--herdr-pane", "pane-1",
                          "activate", "--input-json", '{"confirmed":true}'], env=env, cwd=fixture)
        (evidence / "activation.json").write_text(activation.stdout)
        output_file = fixture / "fake-output"
        fake = "#!/bin/sh\ncat '{}'\n".format(output_file)
        for candidate in fixture.rglob("namedrop"):
            if candidate.is_file():
                candidate.write_text(fake)
                candidate.chmod(0o700)
        fixture_binary.write_text(fake)
        fixture_binary.chmod(0o700)
        (kimi_home / "config.toml").write_text(
            "default_model = \"dummy\"\n\n"
            "[providers.dummy]\n"
            "type = \"openai\"\n"
            "api_key = \"dummy\"\n"
            f"base_url = \"{provider.base_url}\"\n\n"
            "[models.dummy]\n"
            "provider = \"dummy\"\n"
            "model = \"dummy\"\n"
            "max_context_size = 8192\n"
            "capabilities = [\"tool_use\"]\n\n"
            + (kimi_home / "config.toml").read_text()
        )

        output_file.write_text(json.dumps(CONTEXT, separators=(",", ":")))
        ordinary = run([kimi, "-p", "hello", "--output-format", "stream-json"], env=env, cwd=fixture)
        (evidence / "ordinary.json").write_text(ordinary.stdout)
        if not provider.requests:
            raise AssertionError(f"Kimi made no provider request: {ordinary.stderr}")
        request_text = b"\n".join(body for _, body in provider.requests).decode()
        if CONTEXT["message"] not in request_text:
            raise AssertionError("Kimi did not put the Namedrop message in the model request")

        provider.requests.clear()
        output_file.write_text(json.dumps(BLOCK, separators=(",", ":")))
        blocked = run([kimi, "-p", "@reviewer do work", "--output-format", "stream-json"],
                      env=env, cwd=fixture, check=False)
        if blocked.returncode != 1:
            raise AssertionError(f"Kimi did not reject the blocked prompt: {blocked}")
        (evidence / "blocked.json").write_text(blocked.stdout)
        if provider.requests:
            raise AssertionError("confirmed Kimi handoff still reached the provider")
        if "Collaboration message sent." not in blocked.stdout + blocked.stderr:
            raise AssertionError("Kimi did not display the hook block result")

        summary = {
            "kimi": run([kimi, "--version"], env=env, cwd=fixture).stdout.strip(),
            "context_requests": 1,
            "handoff_provider_requests": len(provider.requests),
            "evidence": str(retained),
        }
        (evidence / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(json.dumps(summary, indent=2))
        successful = True
        return 0
    except Exception:
        print(f"Evidence retained at {fixture}", file=sys.stderr)
        raise
    finally:
        provider.close()
        if successful:
            retained.parent.mkdir(parents=True, exist_ok=True)
            shutil.copytree(evidence, retained, dirs_exist_ok=True)
            shutil.rmtree(fixture, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
