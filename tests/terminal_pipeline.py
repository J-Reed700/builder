#!/usr/bin/env python3
"""Real PTY settings-menu regression; no external endpoint or credentials."""
import http.server
from pathlib import Path
import sys
import tempfile
import threading
import tomllib
from terminal_smoke import Terminal, Endpoint


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/builder").resolve()
    endpoint = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Endpoint)
    with tempfile.TemporaryDirectory(prefix="builder-settings-") as directory:
        root = Path(directory)
        home = root / "home"
        home.mkdir()
        path = home / "config.toml"
        path.write_text(f'''default_profile = "local"
[profiles.local]
base_url = "http://127.0.0.1:{endpoint.server_port}/v1"
model = "menu-test"
context_tokens = 100000
max_output_tokens = 1024
''')
        original = path.read_bytes()
        terminal = Terminal(binary, home, root)
        threading.Thread(target=endpoint.serve_forever, daemon=True).start()
        try:
            terminal.expect("Ask Builder")
            # Browse the existing slash menu, without typing a settings command.
            mark = len(terminal.output)
            terminal.send("/")
            terminal.expect("Pipeline features and budgets", mark)
            terminal.send(b"\x1b[B\x1b[B\r")
            terminal.expect("/settings", mark)
            terminal.send(b"\r")
            terminal.expect("Pipeline settings", mark)
            terminal.expect("Research pipeline: On", mark)
            terminal.send(b" ")
            terminal.expect("Research pipeline: Off", mark)
            terminal.send(b"\x1b")
            terminal.expect("Settings unchanged", mark)
            terminal.expect("Ask Builder", terminal.output.find(b"Settings unchanged", mark) + len(b"Settings unchanged"))
            assert path.read_bytes() == original, "Cancel wrote configuration"
            assert not Endpoint.requests, "Opening menu contacted model"
            # Reopen, toggle and save using the menu's End shortcut.
            mark = len(terminal.output)
            terminal.send("/settings\r")
            terminal.expect("Research pipeline: On", mark)
            terminal.send(b" \x1b[F\r")
            terminal.expect("Pipeline settings saved for local and applied to this session", mark)
            terminal.expect("Ask Builder", terminal.output.find(b"applied to this session", mark) + len(b"applied to this session"))
            assert not tomllib.loads(path.read_text())["profiles"]["local"]["pipeline"]["enabled"]
            terminal.send("hello after settings\r")
            terminal.expect("Terminal paste verified", mark)
            ready_start = terminal.output.find(b"Terminal paste verified", mark) + len(b"Terminal paste verified")
            terminal.expect("Ask Builder", ready_start)
            assert len(Endpoint.requests) == 1
            assert all(t["function"]["name"] != "research" for t in Endpoint.requests[0]["tools"]), "Live session kept old settings"
            # Numeric edit validation; unsaved invalid input never reaches disk.
            mark = len(terminal.output)
            terminal.send("/settings\r")
            terminal.expect("Research pipeline: Off", mark)
            terminal.send(b"\x1b[B" * 15 + b"\r0\r")
            terminal.expect("must be between 1 and 20", mark)
            assert tomllib.loads(path.read_text())["profiles"]["local"]["pipeline"]["candidate_attempts"] == 3
            terminal.send(b"\x7f5\r\x1b[F\r")
            terminal.expect("Pipeline settings saved", mark)
            assert tomllib.loads(path.read_text())["profiles"]["local"]["pipeline"]["candidate_attempts"] == 5
            assert len(Endpoint.requests) == 1
            terminal.expect("Ask Builder", terminal.output.find(b"applied to this session", mark) + len(b"applied to this session"))
            mark = len(terminal.output)
            terminal.send("/settings\r")
            terminal.expect("Research pipeline: Off", mark)
            terminal.send(b" ")
            terminal.expect("Research pipeline: On", mark)
            path.write_text(path.read_text().replace("candidate_attempts = 5", "candidate_attempts = 6"))
            terminal.send(b"\x1b[F\r")
            terminal.expect("Pipeline settings changed elsewhere", mark)
            assert tomllib.loads(path.read_text())["profiles"]["local"]["pipeline"]["candidate_attempts"] == 6
            assert not tomllib.loads(path.read_text())["profiles"]["local"]["pipeline"]["enabled"]
            terminal.send(b"\x1b")
            terminal.expect("Settings unchanged", mark)
            terminal.expect("Ask Builder", terminal.output.find(b"Settings unchanged", mark) + len(b"Settings unchanged"))
            terminal.send("/exit\r")
            terminal.expect("Saved. Continue with", mark)
            assert b"\x1b[?1049l" in terminal.output, "Alternate-screen cleanup missing"
            print("PASS: menu navigation, cancel, validation, persistence, live policy and terminal cleanup")
        finally:
            terminal.close()
            endpoint.shutdown()
        plain = Terminal(binary, home, root, args=("--plain",))
        try:
            plain.expect("builder ›")
            mark = len(plain.output)
            plain.send("/settings\r")
            plain.expect("Choose an item:", mark)
            plain.send("1\r")
            plain.expect("Research pipeline: On", mark)
            # The run budget is a menu setting and applies without restarting.
            import re
            field_source = Path(__file__).resolve().parents[1] / "src/input/pipeline.rs"
            field_keys = re.findall(r'\(\s*"([a-z_]+)",', field_source.read_text().split("const SAVE")[0])
            budget_index = field_keys.index("max_rounds") + 1
            plain.send(f"{budget_index}\r")
            plain.expect("New value", mark)
            plain.send("250\r")
            plain.expect("Agent rounds per run: 250", mark)
            plain.send("s\r")
            plain.expect("Pipeline settings saved", mark)
            assert tomllib.loads(path.read_text())["profiles"]["local"]["pipeline"]["enabled"]
            assert tomllib.loads(path.read_text())["profiles"]["local"]["pipeline"]["max_rounds"] == 250
            plain.send("/status\r")
            plain.expect("Agent rounds per run: 250", mark)
            plain.send("/exit\r")
            plain.expect("Saved. Continue with", mark)
            print("PASS: numbered settings menu in plain terminals")
        finally:
            plain.close()


if __name__ == "__main__":
    main()
