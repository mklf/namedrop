"""Real Herdr and Kimi Code collaboration with a local scripted provider.

Two interactive Kimi sessions register with Namedrop, then a leading mention is
sent from @lead to @reviewer. The local provider returns registration, reply,
and completion tool calls, so no external model service or credentials are used.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPO = Path(__file__).resolve().parent.parent


def text_parts(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(text_parts(item) for item in value)
    if isinstance(value, dict):
        if isinstance(value.get("text"), str):
            return value["text"]
        return "\n".join(text_parts(item) for item in value.values())
    return ""


class Provider:
    def __init__(self):
        self.commands = []
        self.requests = []
        outer = self

        def chunk(delta, reason=None):
            return {
                "id": "scripted-response",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "scripted",
                "choices": [{"index": 0, "delta": delta, "finish_reason": reason}],
            }

        def respond(chunks):
            chunks.append({
                "id": "scripted-response",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "scripted",
                "choices": [],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })
            return ("".join(f"data: {json.dumps(item)}\n\n" for item in chunks)
                    + "data: [DONE]\n\n").encode()

        def plain(content):
            return respond([chunk({"role": "assistant"}), chunk({"content": content}), chunk({}, "stop")])

        def tool(command):
            outer.commands.append(command)
            call = {
                "index": 0,
                "id": f"call_{len(outer.commands)}",
                "type": "function",
                "function": {
                    "name": "Bash",
                    "arguments": json.dumps({"command": command}),
                },
            }
            return respond([chunk({"role": "assistant", "tool_calls": [call]}), chunk({}, "tool_calls")])

        def bound_command(envelope, token, body=None):
            control = envelope["control"]
            tool = control["tool"]
            operation = next(item for item in tool["operations"] if item["token"] == token)
            environment = tool["environment"]
            args = [
                shlex_quote(tool["executable"]),
                "--plugin-state", shlex_quote(environment["HERDR_PLUGIN_STATE_DIR"]),
                *(shell_argument(arg) for arg in operation["argv"]),
                "--herdr-socket", shlex_quote(environment["HERDR_SOCKET_PATH"]),
                "--herdr-workspace", shlex_quote(environment["HERDR_WORKSPACE_ID"]),
                "--herdr-pane", shlex_quote(environment["HERDR_PANE_ID"]),
            ]
            if "input" in operation:
                payload = dict(operation["input"])
                if body is not None and isinstance(payload.get("source"), str):
                    payload["source"] = payload["source"].replace("<reply body>", body)
                args.extend([tool["input_flag"], shlex_quote(json.dumps(payload))])
            return " ".join(args)

        def shlex_quote(value):
            return "'" + str(value).replace("'", "'\"'\"'") + "'"

        def shell_argument(value):
            value = str(value)
            if value and all(char.isalnum() or char in "._/:=@+,-" for char in value):
                return value
            return shlex_quote(value)

        def route(content):
            prompt = text_parts(content)
            if "Namedrop registration challenge" in prompt:
                return tool(prompt.split("then stop this turn: ", 1)[1].strip())
            candidates = content if isinstance(content, list) else [content]
            for candidate in candidates:
                try:
                    envelope = json.loads(text_parts(candidate))
                except (ValueError, TypeError):
                    continue
                if isinstance(envelope, dict) and envelope.get("envelope_kind") == "delivery":
                    control = envelope["control"]
                    if control.get("result_set"):
                        return tool(bound_command(envelope, "complete.no-reply"))
                    return tool(bound_command(envelope, "reply", "fixture response"))
            if "fixture response" in prompt:
                return plain("The fixture response was returned to the lead.")
            return plain("ready")

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(length)
                outer.requests.append(raw)
                try:
                    payload = json.loads(raw)
                    status, body = 200, route(payload["messages"][-1]["content"])
                except Exception as error:
                    status, body = 500, str(error).encode()
                self.send_response(status)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

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


def main():
    kimi = shutil.which("kimi")
    herdr = shutil.which("herdr")
    if not kimi or not herdr:
        print("BLOCKED: kimi and herdr are required", file=sys.stderr)
        return 2
    binary = (REPO / "target/debug/namedrop").resolve()
    if not binary.is_file():
        raise AssertionError(f"build Namedrop first: {binary}")
    base = Path(tempfile.mkdtemp(prefix="namedrop-kimi-roundtrip-"))
    evidence = base / "evidence"
    evidence.mkdir()
    retained = REPO / ".scratch/kimi-roundtrip" / base.name
    provider = Provider()
    server = None
    server_log = None
    successful = False
    try:
        for name in ("home", "config", "state", "data", "run", "workspace"):
            (base / name).mkdir()
        (base / "home/.kimi-code").mkdir()
        env = os.environ.copy()
        env.update({
            "HOME": str(base / "home"),
            "XDG_CONFIG_HOME": str(base / "config"),
            "XDG_STATE_HOME": str(base / "state"),
            "XDG_DATA_HOME": str(base / "data"),
            "XDG_RUNTIME_DIR": str(base / "run"),
            "CODEX_HOME": str(base / "home/.codex"),
            "CLAUDE_CONFIG_DIR": str(base / "home/.claude"),
            "KIMI_CODE_HOME": str(base / "home/.kimi-code"),
            "KIMI_DISABLE_TELEMETRY": "1",
            "KIMI_CODE_NO_AUTO_UPDATE": "1",
            "KIMI_CODE_TRUST_WORKSPACE": "1",
            "HERDR_SOCKET_PATH": str(base / "run/herdr.sock"),
            "HERDR_CONFIG_PATH": str(base / "config/herdr.toml"),
            "SHELL": "/bin/bash",
            "NO_COLOR": "1",
        })

        def command(*args, context=None, check=True, timeout=60, input=None):
            result = subprocess.run([str(arg) for arg in args],
                                    env=dict(env, **(context or {})),
                                    cwd=base / "workspace", capture_output=True,
                                    text=True, timeout=timeout, input=input)
            with (evidence / "commands.jsonl").open("a") as log:
                log.write(json.dumps({"args": [str(a) for a in args], "exit": result.returncode,
                                      "stdout": result.stdout, "stderr": result.stderr}) + "\n")
            if check and result.returncode != 0:
                raise AssertionError(f"command failed {args}:\n{result.stdout}\n{result.stderr}")
            return result

        def host(*args, context=None):
            result = command(herdr, *args, context=context)
            return json.loads(result.stdout) if result.stdout.strip() else None

        def wait(predicate, description, timeout=45):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                value = predicate()
                if value:
                    return value
                time.sleep(.15)
            raise AssertionError(f"timed out: {description}")

        subprocess.run(["git", "init", "--quiet"], cwd=base / "workspace", check=True)
        server_log = (evidence / "server.log").open("w")
        server = subprocess.Popen([herdr, "server"], env=env, cwd=base / "workspace",
                                  stdout=server_log, stderr=server_log, start_new_session=True)
        wait(lambda: Path(env["HERDR_SOCKET_PATH"]).exists(), "Herdr socket")
        created = host("workspace", "create", "--cwd", str(base / "workspace"),
                       "--label", "Kimi roundtrip")["result"]
        workspace = created["workspace"]["workspace_id"]
        lead = created["root_pane"]["pane_id"]
        existing = {pane["pane_id"] for pane in host("pane", "list")["result"]["panes"]}
        host("tab", "create", "--workspace", workspace, "--cwd", str(base / "workspace"),
             "--label", "reviewer")
        reviewer = wait(lambda: next((pane["pane_id"] for pane in host("pane", "list")["result"]["panes"]
                                      if pane["pane_id"] not in existing), None), "reviewer pane")
        env["HERDR_WORKSPACE_ID"] = workspace
        env["HERDR_PANE_ID"] = lead
        state = base / "state/namedrop"
        context = {
            "HERDR_WORKSPACE_ID": workspace,
            "HERDR_PANE_ID": lead,
            "HERDR_PLUGIN_STATE_DIR": str(state),
        }
        command(herdr, "integration", "install", "kimi", context=context)
        command(binary, "--plugin-state", state, "--herdr-socket", env["HERDR_SOCKET_PATH"],
                "--herdr-workspace", workspace, "--herdr-pane", lead,
                "activate", "--input-json", '{"confirmed":true}', context=context)
        config = base / "home/.kimi-code/config.toml"
        config.write_text(
            "default_model = \"scripted\"\n\n"
            "default_permission_mode = \"manual\"\n\n"
            "[providers.scripted]\n"
            "type = \"openai\"\n"
            "api_key = \"scripted\"\n"
            f"base_url = \"{provider.base_url}\"\n\n"
            "[models.scripted]\n"
            "provider = \"scripted\"\n"
            "model = \"scripted\"\n"
            "max_context_size = 32768\n"
            "capabilities = [\"tool_use\"]\n\n"
            + config.read_text()
        )
        command(herdr, "pane", "run", lead, kimi, context=context)
        command(herdr, "pane", "run", reviewer, kimi, context=context)
        def screen(pane):
            return command(herdr, "pane", "read", pane, "--source", "recent-unwrapped",
                           "--lines", "80", context=context).stdout

        for pane in (lead, reviewer):
            wait(lambda pane=pane: "No session yet" in screen(pane), f"Kimi composer in {pane}")
        command(herdr, "pane", "send-text", lead, "ready\n", context=context)
        command(herdr, "pane", "send-text", reviewer, "ready\n", context=context)
        command(herdr, "pane", "send-keys", lead, "return", context=context)
        command(herdr, "pane", "send-keys", reviewer, "return", context=context)
        time.sleep(2)
        (evidence / "lead-screen.txt").write_text(screen(lead))
        (evidence / "reviewer-screen.txt").write_text(screen(reviewer))

        def pane_value(pane, key):
            return next(candidate for candidate in host("pane", "list")["result"]["panes"]
                        if candidate["pane_id"] == pane).get(key)

        for pane in (lead, reviewer):
            wait(lambda pane=pane: pane_value(pane, "agent_session") is not None,
                 f"native Kimi session in {pane}")
            wait(lambda pane=pane: pane_value(pane, "agent_status") in ("idle", "done"),
                 f"idle Kimi session in {pane}")

        def namedrop(*args, pane, check=True):
            return command(binary, "--plugin-state", state, "--herdr-socket", env["HERDR_SOCKET_PATH"],
                           "--herdr-workspace", workspace, "--herdr-pane", pane,
                           *args, context=context, check=check)

        namedrop("name", "@lead", "--type", "agent", pane=lead)
        namedrop("name", "@reviewer", "--type", "agent", pane=reviewer)

        def registered(alias):
            listed = json.loads(namedrop("address", "list", "--json", pane=lead).stdout)
            return next((item for item in listed["data"] if item["alias"] == alias), None)

        for alias in ("lead", "reviewer"):
            wait(lambda alias=alias: registered(alias) and registered(alias)["registered"] is True,
                 f"registered {alias}")

        command(herdr, "pane", "send-text", lead, "@reviewer Return the fixture response.\n", context=context)
        command(herdr, "pane", "send-keys", lead, "return", context=context)

        (evidence / "address-after.json").write_text(
            namedrop("address", "list", "--json", pane=lead, check=False).stdout)
        lead_session = pane_value(lead, "agent_session")["value"]
        hook_payload = json.dumps({
            "hook_event_name": "UserPromptSubmit",
            "session_id": lead_session,
            "prompt": [{"type": "text", "text": "hello"}],
        })
        direct_hook = command(binary, "hook", "user-prompt-submit",
                              "--state-directory", state, "--format", "kimi",
                              context=context, check=False, input=hook_payload)
        (evidence / "direct-hook.json").write_text(json.dumps({
            "payload": json.loads(hook_payload),
            "exit": direct_hook.returncode,
            "stdout": direct_hook.stdout,
            "stderr": direct_hook.stderr,
        }, indent=2) + "\n")
        assert direct_hook.returncode == 0, direct_hook.stderr
        assert direct_hook.stdout == '', direct_hook.stdout
        assert direct_hook.stderr == '', direct_hook.stderr

        def sent_handoff():
            try:
                report = json.loads(namedrop("handoff", "status", "--json", pane=lead).stdout)
                return next((handoff for handoff in report.get("data", {}).get("handoffs", [])
                             if handoff.get("request") == "@reviewer Return the fixture response."), None)
            except (ValueError, KeyError, IndexError, TypeError):
                return None

        sent = wait(sent_handoff, "leading handoff")
        result_input = json.dumps({"message_id": sent["message_id"]})

        def results():
            return namedrop("handoff", "results", "--input-json", result_input, pane=lead).stdout

        approvals = set()

        def completed():
            try:
                for pane in (lead, reviewer):
                    if pane_value(pane, "agent_status") == "blocked":
                        if pane not in approvals:
                            (evidence / f"approval-{pane}.txt").write_text(screen(pane))
                            command(herdr, "pane", "send-keys", pane, "return", context=context)
                            approvals.add(pane)
                    else:
                        approvals.discard(pane)
                report = json.loads(results())
                replied = any(" reply " in command and "fixture response" in command
                              for command in provider.commands)
                return replied and report.get("data", report).get("complete") is True
            except (ValueError, KeyError, IndexError, TypeError):
                return False

        wait(completed, "reply and completed handoff")
        (evidence / "handoff.json").write_text(results())
        (evidence / "provider-commands.json").write_text(json.dumps(provider.commands, indent=2) + "\n")
        if not any(" reply " in command and "fixture response" in command
                   for command in provider.commands):
            raise AssertionError("no reply command with the fixture response was executed")
        approval_count = 0
        for pane, name in ((lead, "lead"), (reviewer, "reviewer")):
            session_id = pane_value(pane, "agent_session")["value"]
            wires = list((base / "home/.kimi-code/sessions").glob(
                f"*/{session_id}/agents/main/wire.jsonl"))
            if len(wires) != 1:
                raise AssertionError(f"could not locate the {name} Kimi wire: {wires}")
            wire = wires[0].read_text()
            (evidence / f"{name}-wire.jsonl").write_text(wire)
            approval_count += wire.count('"kind":"approval"')
        summary = {
            "herdr": command(herdr, "--version").stdout.strip(),
            "kimi": command(kimi, "--version").stdout.strip(),
            "lead": lead,
            "reviewer": reviewer,
            "provider_commands": len(provider.commands),
            "permission_approvals": approval_count,
            "handoff": "completed with fixture response",
            "evidence": str(retained),
        }
        (evidence / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(json.dumps(summary, indent=2))
        successful = True
        return 0
    finally:
        provider.close()
        if server is not None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)
        if server_log is not None:
            server_log.close()
        if not successful:
            print(f"Evidence retained at {base}", file=sys.stderr)
        else:
            retained.parent.mkdir(parents=True, exist_ok=True)
            shutil.copytree(evidence, retained, dirs_exist_ok=True)
            shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
