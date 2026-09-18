#!/usr/bin/env python3
"""Real PTY: automatic/manual compaction, original history, persisted checkpoint."""
import http.server
import json
from pathlib import Path
import sys
import tempfile
import threading
from terminal_smoke import Terminal


class Endpoint(http.server.BaseHTTPRequestHandler):
    requests = []
    normal = 0
    summaries = 0

    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.requests.append(request)
        summary = request["messages"][0]["content"].startswith("Write a concise factual handoff")
        if summary:
            assert "tools" not in request
            self.__class__.summaries += 1
            text = "x" * 10000 if self.summaries == 2 else "Source inspection completed. Preserve the user's requested scope and verify current files."
        else:
            self.__class__.normal += 1
            text = "evidence " * 2000 if self.normal <= 2 else "Task completed."
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        value = {"choices": [{"delta": {"content": text}, "finish_reason": None}]}
        self.wfile.write(f"data: {json.dumps(value)}\n\n".encode())
        reason = "length" if summary and self.summaries == 1 else "stop"
        final = {"choices": [{"delta": {}, "finish_reason": reason}]}
        self.wfile.write(f"data: {json.dumps(final)}\n\ndata: [DONE]\n\n".encode())


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/builder").resolve()
    endpoint = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Endpoint)
    with tempfile.TemporaryDirectory(prefix="builder-compact-") as directory:
        root = Path(directory)
        home = root / "home"
        home.mkdir()
        (home / "config.toml").write_text(f'''default_profile = "local"
[profiles.local]
base_url = "http://127.0.0.1:{endpoint.server_port}/v1"
model = "compact-test"
tools = false
context_tokens = 16000
max_output_tokens = 4096
''')
        terminal = Terminal(binary, home, root)
        threading.Thread(target=endpoint.serve_forever, daemon=True).start()
        try:
            terminal.expect("Ask Builder")
            terminal.send("/status\r")
            terminal.expect_row("Compaction", "automatic at 75%")
            terminal.expect("Ask Builder", terminal.output.find(b"automatic at 75%"))
            mark = len(terminal.output)
            terminal.send("inspect source\r")
            terminal.expect("evidence evidence", mark)
            terminal.expect("Ask Builder", mark)
            mark = len(terminal.output)
            terminal.send("continue inspection\r")
            terminal.expect("Summarizing", mark)
            terminal.expect("Context summary needed more room", mark)
            terminal.expect("retrying with 8.2k tokens", mark)
            terminal.expect("Tightening context summary", mark)
            terminal.expect("Context compacted ·", mark)
            summary_requests = [r for r in Endpoint.requests if r["messages"][0]["content"].startswith("Write a concise factual handoff")]
            assert summary_requests[0]["max_tokens"] == 4096
            assert summary_requests[1]["max_tokens"] == 8192
            assert summary_requests[0]["messages"] == summary_requests[1]["messages"]
            assert summary_requests[1]["messages"][1] == summary_requests[2]["messages"][1]
            assert summary_requests[2]["max_tokens"] == 8192
            terminal.expect("Ask Builder", mark)
            mark = len(terminal.output)
            terminal.send("/compact\r")
            terminal.expect("Context compacted ·", mark)
            terminal.expect("Ask Builder", mark)
            assert Endpoint.normal == 2, "Manual compaction executed the task"
            mark = len(terminal.output)
            terminal.send("/history archived\r")
            terminal.expect("inspect source", mark)
            terminal.expect("evidence evidence", mark)
            terminal.expect("Ask Builder", mark)
            terminal.send("/exit\r")
            terminal.expect("Saved. Continue with", mark)
            terminal.close()
            terminal = None
            terminal = Terminal(binary, home, root, args=("resume",))
            terminal.expect("Ask Builder")
            mark = len(terminal.output)
            terminal.send("finish now\r")
            terminal.expect("Task completed.", mark)
            terminal.expect("Ask Builder", mark)
            context = Endpoint.requests[-1]["messages"]
            assert any("Compacted handoff" in (m.get("content") or "") for m in context)
            assert context[-1]["content"] == "finish now"
            assert not any("evidence evidence" in (m.get("content") or "") for m in context)
            terminal.send("/exit\r")
            terminal.expect("Saved. Continue with", mark)
            print("PASS: compaction output-limit and oversized-handoff recovery, default auto-compaction, manual /compact, original history, checkpoint resume, latest instruction retained")
        finally:
            if terminal is not None:
                terminal.close()
            endpoint.shutdown()
            endpoint.server_close()


if __name__ == "__main__":
    main()
