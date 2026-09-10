#!/usr/bin/env python3
"""Actual Ctrl+C → follow-up → rewind → resume regression with isolated state."""
import http.server
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
from terminal_smoke import Terminal


class Endpoint(http.server.BaseHTTPRequestHandler):
    requests = []
    release = threading.Event()

    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.requests.append(request)
        messages = request["messages"]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        if len(messages) == 2 and messages[-1]["content"] == "implement change":
            change = {"tool_calls": [{"index": 0, "id": "read_original", "type": "function",
                "function": {"name": "read_file", "arguments": '{"path":"source.txt"}'}}]}
            reason = "tool_calls"
        elif messages[-1]["role"] == "tool" or messages[-1]["content"] == "cancel this":
            self.event({"content": "unfinished visible only"})
            self.event({"reasoning_content": "still thinking"})
            self.wfile.flush()
            self.release.wait(20)
            return
        else:
            change = {"content": "Handoff completed."}
            reason = "stop"
        self.event(change)
        self.event({}, reason)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def event(self, delta, reason=None):
        value = {"choices": [{"delta": delta, "finish_reason": reason}]}
        self.wfile.write(f"data: {json.dumps(value)}\n\n".encode())


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/builder").resolve()
    endpoint = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Endpoint)
    with tempfile.TemporaryDirectory(prefix="builder-interrupt-") as directory:
        root = Path(directory)
        home = root / "home"
        home.mkdir()
        (root / "source.txt").write_text("original source evidence")
        (home / "config.toml").write_text(f'''default_profile = "local"
[profiles.local]
base_url = "http://127.0.0.1:{endpoint.server_port}/v1"
model = "interrupt-test"
context_tokens = 100000
max_output_tokens = 1024
''')
        terminal = Terminal(binary, home, root)
        threading.Thread(target=endpoint.serve_forever, daemon=True).start()
        try:
            terminal.expect("Ask Builder")
            mark = len(terminal.output)
            terminal.send("implement change\r")
            terminal.expect("unfinished visible only", mark)
            terminal.send(b"\x03")
            terminal.expect("Paused. Send a follow-up", mark)
            terminal.expect("Ask Builder", mark)
            mark = len(terminal.output)
            terminal.send("write a document instead\r")
            terminal.expect("Handoff completed.", mark)
            terminal.expect("Ask Builder", mark)
            assert len(Endpoint.requests) == 3
            context = Endpoint.requests[-1]["messages"]
            assert context[-1]["content"] == "write a document instead"
            assert any(m["role"] == "tool" and "original source evidence" in m["content"] for m in context)
            assert not any("unfinished visible only" in (m.get("content") or "") for m in context)

            mark = len(terminal.output)
            terminal.send("/rewind\r")
            terminal.expect("previous message restored for editing", mark)
            terminal.expect("write a document instead", mark)
            # Change the restored prompt, without auto-sending it.
            assert len(Endpoint.requests) == 3
            mark = len(terminal.output)
            terminal.send(b"\x15revised handoff\r")
            terminal.expect("Handoff completed.", mark)
            terminal.expect("Ask Builder", mark)
            context = Endpoint.requests[-1]["messages"]
            assert context[-1]["content"] == "revised handoff"
            assert not any(m.get("content") == "write a document instead" for m in context)
            assert any(m["role"] == "tool" for m in context)

            mark = len(terminal.output)
            terminal.send("cancel this\r")
            terminal.expect("unfinished visible only", mark)
            terminal.send(b"\x03")
            terminal.expect("Paused. Send a follow-up", mark)
            terminal.expect("Ask Builder", mark)
            terminal.send("/exit\r")
            terminal.expect("Saved. Continue with", mark)
            terminal.close()
            terminal = None

            terminal = Terminal(binary, home, root, args=("resume",))
            terminal.expect("Paused turn restored")
            assert b"Handoff completed." not in terminal.output, \
                "Resume presented an answer from the previous turn as current"
            assert b"unfinished visible only" not in terminal.output, \
                "Resume presented provisional output from the interrupted turn"
            terminal.expect("Ask Builder")
            count = len(Endpoint.requests)
            time.sleep(0.1)
            assert count == len(Endpoint.requests), "Resume restarted cancelled generation"
            mark = len(terminal.output)
            terminal.send("/cancel\r")
            terminal.expect("Pending response cancelled", mark)
            terminal.expect("Ask Builder", mark)
            mark = len(terminal.output)
            terminal.send("/retry\r")
            terminal.expect("Ask Builder", mark)
            assert count == len(Endpoint.requests), "Cancelled turn retried itself"
            mark = len(terminal.output)
            terminal.send("/rewind\r")
            terminal.expect("previous message restored for editing", mark)
            terminal.expect("cancel this", mark)
            terminal.send(b"\x15/exit\r")
            terminal.expect("Saved. Continue with", mark)
            terminal.close()
            terminal = None

            terminal = Terminal(binary, home, root, args=("resume",))
            terminal.expect("Previous message restored as a draft")
            terminal.expect("cancel this")
            assert count == len(Endpoint.requests)
            mark = len(terminal.output)
            terminal.send(b"\x15fresh direction\r")
            terminal.expect("Handoff completed.", mark)
            terminal.expect("Ask Builder", mark)
            assert Endpoint.requests[-1]["messages"][-1]["content"] == "fresh direction"
            assert not any(m.get("content") == "cancel this" for m in Endpoint.requests[-1]["messages"])
            mark = len(terminal.output)
            terminal.send("/history archived\r")
            terminal.expect("write a document instead", mark)
            terminal.expect("cancel this", mark)
            terminal.expect("Ask Builder", mark)
            terminal.send("/exit\r")
            terminal.expect("Saved. Continue with", mark)
            print("PASS: Ctrl+C steering, completed tool context, discarded partial output, editable rewind, archived history, cancellation, paused resume, durable rewind draft")
        finally:
            if terminal is not None:
                terminal.close()
            Endpoint.release.set()
            endpoint.shutdown()
            endpoint.server_close()


if __name__ == "__main__":
    main()
