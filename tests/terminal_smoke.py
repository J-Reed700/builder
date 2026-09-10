#!/usr/bin/env python3
"""Real Unix PTY regression: atomic large paste, exact payload, resize, cleanup.

Run after cargo build: python3 tests/terminal_smoke.py [path/to/builder]
No third-party Python packages, model server, or credentials needed.
"""
import errno
import fcntl
import http.server
import json
import os
from pathlib import Path
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


class Endpoint(http.server.BaseHTTPRequestHandler):
    requests = []

    def log_message(self, *_):
        pass

    def do_POST(self):
        value = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.requests.append(value)
        # Exercise the distinction between a saved paste and a slow endpoint.
        time.sleep(0.2)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        reasoning = {"choices": [{"delta": {"reasoning_content": "internal test reasoning"}, "finish_reason": None}]}
        self.wfile.write(f"data: {json.dumps(reasoning)}\n\n".encode())
        self.wfile.flush()
        time.sleep(0.2)
        for text in ["Terminal ", "paste ", "verified."]:
            value = {"choices": [{"delta": {"content": text}, "finish_reason": None}]}
            self.wfile.write(f"data: {json.dumps(value)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.02)
        self.wfile.write(b'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n')
        self.wfile.flush()


class Terminal:
    def __init__(self, binary, home, workspace, color=False, args=()):
        self.output = bytearray()
        self.pid, self.fd = os.forkpty()
        if self.pid == 0:
            os.environ.update(TERM="xterm-256color", NO_COLOR="1")
            if color:
                os.environ.pop("NO_COLOR", None)
                os.environ["CLICOLOR_FORCE"] = "1"
            os.execv(str(binary), [str(binary), "--home", str(home), "-C", str(workspace), *args])
        self.resize(100, 32)
        os.set_blocking(self.fd, False)

    def resize(self, columns, rows):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)

    def drain(self, wait=0.01):
        if select.select([self.fd], [], [], wait)[0]:
            try:
                data = os.read(self.fd, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    return
                raise
            self.output.extend(data)

    def expect(self, text, start=0, timeout=8):
        deadline = time.monotonic() + timeout
        needle = text.encode()
        while needle not in self.output[start:]:
            if time.monotonic() > deadline:
                raise AssertionError(f"Timed out waiting for {text!r}: {self.output[-1500:]!r}")
            self.drain()

    def send(self, data):
        data = data.encode() if isinstance(data, str) else data
        at = 0
        deadline = time.monotonic() + 15
        while at < len(data):
            assert time.monotonic() < deadline, "Terminal input stalled"
            if select.select([], [self.fd], [], 0.01)[1]:
                try:
                    at += os.write(self.fd, data[at:at + 32768])
                except BlockingIOError:
                    pass
            self.drain(0)

    def close(self):
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        os.close(self.fd)
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if os.waitpid(self.pid, os.WNOHANG)[0]:
                return
            time.sleep(0.01)
        os.kill(self.pid, signal.SIGKILL)
        os.waitpid(self.pid, 0)


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/builder").resolve()
    clipboard = "--clipboard" in sys.argv[2:]
    if clipboard:
        assert sys.platform == "darwin", "Direct clipboard test requires macOS"
        # Opt-in: read existing text without replacing the user's clipboard.
        # Only the local mock endpoint receives it; temporary history is deleted.
        clipboard_text = subprocess.check_output(["/usr/bin/pbpaste"], timeout=3).decode("utf-8")
        assert len(clipboard_text.encode()) <= 4 * 1024 * 1024, "Clipboard exceeds the draft limit"
    endpoint = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Endpoint)
    # Start HTTP thread after fork; this avoids forking with live Python threads.
    with tempfile.TemporaryDirectory(prefix="builder-pty-") as directory:
        root = Path(directory)
        home = root / "home"
        home.mkdir()
        config = f'''default_profile = "local"
[profiles.local]
base_url = "http://127.0.0.1:{endpoint.server_port}/v1"
model = "terminal-test"
tools = false
context_tokens = 4000000
max_output_tokens = 1024
'''
        (home / "config.toml").write_text(config)
        started = time.monotonic()
        terminal = Terminal(binary, home, root)
        threading.Thread(target=endpoint.serve_forever, daemon=True).start()
        try:
            terminal.expect("Ask Builder")
            print(f"Process start → ready composer: {(time.monotonic() - started) * 1000:.1f} ms")
            if clipboard and not clipboard_text:
                mark = len(terminal.output)
                terminal.send(b"\x16")
                terminal.expect("Clipboard is empty or contains no text", mark)
                assert not Endpoint.requests, "Empty clipboard submitted a prompt"
                terminal.send(b"\x04")
                terminal.expect("Saved. Continue with", mark)
                print("PASS: Ctrl+V reads clipboard, reports empty text, leaves draft unchanged")
                return
            prefix = "// pasted source\n"
            unit = "  fn example() { /* 🦀 */ }\r\n"
            paste = prefix + unit * ((1024 * 1024 - len(prefix.encode())) // len(unit.encode()))
            paste += " " * (1024 * 1024 - len(paste.encode()))
            assert len(paste.encode()) == 1024 * 1024
            if clipboard:
                paste = clipboard_text
            start = time.monotonic()
            mark = len(terminal.output)
            terminal.send(b"\x16" if clipboard else b"\x1b[200~" + paste.encode() + b"\x1b[201~")
            terminal.expect("pasted", mark)
            elapsed = time.monotonic() - start
            assert not Endpoint.requests, "Paste submitted itself without Enter"
            assert len(terminal.output) < 20000, "Large paste flooded terminal output"
            method = "Ctrl+V direct clipboard" if clipboard else "bracketed paste"
            print(f"{len(paste.encode()):,} bytes {method} → compact visible block: {elapsed * 1000:.1f} ms")
            if clipboard:
                terminal.send(b"\x1a\x19")  # Undo and redo retain the exact paste.

            mark = len(terminal.output)
            start = time.monotonic()
            terminal.send(" explain")
            terminal.expect(" explain", mark)
            print(f"Typing after large paste → repaint: {(time.monotonic() - start) * 1000:.1f} ms")
            terminal.send("\r")
            terminal.expect("Endpoint accepted the request", mark)
            terminal.expect("Model is thinking", mark)
            terminal.expect("Terminal paste verified.", mark)
            assert b"internal test reasoning" not in terminal.output
            terminal.expect("Ask Builder", mark)
            assert Endpoint.requests[0]["messages"][-1]["content"] == paste + " explain", "Paste bytes or whitespace changed"

            mark = len(terminal.output)
            terminal.resize(44, 18)
            terminal.expect("\x1b[?2026l", mark)
            mark = len(terminal.output)
            terminal.resize(20, 5)
            terminal.expect("\x1b[?2026l", mark)
            terminal.resize(100, 32)
            time.sleep(0.05)
            terminal.drain()
            mark = len(terminal.output)
            terminal.send("/")
            terminal.expect("› /help", mark)
            terminal.expect("/settings", mark)
            mark = len(terminal.output)
            terminal.send("\x1b[B")
            terminal.expect("› /status", mark)
            mark = len(terminal.output)
            terminal.send("\r")
            terminal.expect("enter run", mark)
            assert b"Estimated history tokens:" not in terminal.output[mark:], "Choosing a command executed it"
            assert len(Endpoint.requests) == 1, "Browsing commands contacted the model"
            terminal.send("\x15")
            terminal.send("/sta\t\r")
            terminal.expect("Estimated history tokens:", mark)
            terminal.expect("Ask Builder", mark)
            mark = len(terminal.output)
            terminal.send("\x04")
            terminal.expect("Saved. Continue with", mark)
            assert b"\x1b[?2004l" in terminal.output, "Bracketed paste mode was not restored"
            print("PASS: exact multiline payload, no auto-submit, compact output, command menu, tab completion, resize, terminal cleanup")
        finally:
            terminal.close()
            endpoint.shutdown()
            endpoint.server_close()


if __name__ == "__main__":
    main()
