#!/usr/bin/env python3
"""Docker-to-host smoke test. Requires a running Docker daemon and built Builder.

Uses only disposable state, mock model responses, and a uniquely named Compose
project. Covers IPv4 host-gateway routing, auth, approvals, and durable history.
"""
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid

REPO = Path(__file__).resolve().parents[1]
BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else REPO / 'target/debug/builder').resolve()


def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class Model(http.server.BaseHTTPRequestHandler):
    calls = 0

    def log_message(self, *_):
        pass

    def do_POST(self):
        data = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        type(self).calls += 1
        if data['messages'][-1]['role'] == 'tool':
            message = {'role': 'assistant', 'content': 'Container round trip complete'}
        else:
            message = {'role': 'assistant', 'tool_calls': [{
                'id': 'docker-write', 'type': 'function', 'function': {
                    'name': 'write_file', 'arguments': json.dumps({'path': 'result.txt', 'content': 'approved through Docker'})
                }
            }]}
        body = json.dumps({'choices': [{'message': message, 'finish_reason': 'tool_calls' if 'tool_calls' in message else 'stop'}]}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    backend_port, web_port = port(), port()
    project = 'builder-test-' + uuid.uuid4().hex[:10]
    environment = dict(os.environ, BUILDER_UPSTREAM=f'host.docker.internal:{backend_port}', BUILDER_WEB_PORT=str(web_port))
    compose = ['docker', 'compose', '-p', project, '-f', str(REPO / 'remote/compose.yaml')]
    endpoint = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
    threading.Thread(target=endpoint.serve_forever, daemon=True).start()
    process = None
    try:
        with tempfile.TemporaryDirectory(prefix='builder-docker-') as directory:
            root = Path(directory)
            home, workspace = root / 'home', root / 'workspace'
            home.mkdir()
            workspace.mkdir()
            (home / 'config.toml').write_text(f'default_profile = "fixture"\n[profiles.fixture]\nbase_url = "http://127.0.0.1:{endpoint.server_port}/v1"\nmodel = "fixture"\nstream = false\n[profiles.fixture.pipeline]\nenabled = false\n')
            origin = f'http://localhost:{web_port}'
            with (root / 'host.log').open('w') as log:
                process = subprocess.Popen([str(BINARY), '--home', str(home), '-C', str(workspace), 'remote', '--listen', f'0.0.0.0:{backend_port}', '--origin', origin], stdout=log, stderr=log)
                deadline = time.monotonic() + 10
                while not (home / 'remote-token').exists():
                    assert process.poll() is None, (root / 'host.log').read_text()
                    assert time.monotonic() < deadline, 'Host startup timed out'
                    time.sleep(0.05)
                token = (home / 'remote-token').read_text()
                subprocess.run(compose + ['up', '-d', '--build'], env=environment, check=True, timeout=180, stdout=subprocess.DEVNULL)

                def api(path, body=None, authenticated=True):
                    headers = {'Origin': origin}
                    if authenticated:
                        headers['X-Builder-Token'] = token
                    if body is not None:
                        headers['Content-Type'] = 'application/json'
                    request = urllib.request.Request(origin + '/api/' + path, data=json.dumps(body).encode() if body is not None else None, headers=headers)
                    try:
                        with urllib.request.urlopen(request, timeout=15) as response:
                            return response.status, json.load(response)
                    except urllib.error.HTTPError as error:
                        return error.code, error.read().decode()

                deadline = time.monotonic() + 15
                while True:
                    try:
                        status, _ = api('status', authenticated=False)
                        if status == 401:
                            break
                    except (urllib.error.URLError, ConnectionError):
                        # Docker may accept and close a socket before nginx is ready.
                        # Only this bounded read-only readiness probe retries.
                        pass
                    assert time.monotonic() < deadline, 'Container did not reach the host'
                    time.sleep(0.1)
                # Repeated read requests catch dual-stack round-robin routing failures.
                for _ in range(12):
                    assert api('status')[0] == 200
                request_id = str(uuid.uuid4())
                status, run = api('run', {'request_id': request_id, 'session': None, 'operation': {'action': 'message', 'prompt': 'Create the test result file'}})
                assert status == 202, (status, run)

                def wait_phase(phase):
                    deadline = time.monotonic() + 15
                    while True:
                        status, value = api('status')
                        assert status == 200, value
                        if value['state']['phase'] == phase:
                            return value['state']
                        assert time.monotonic() < deadline, value
                        time.sleep(0.15)

                state = wait_phase('awaiting_approval')
                assert not (workspace / 'result.txt').exists()
                assert api('approval', {'run_id': run['run_id'], 'approval_id': state['approval']['id'], 'allow': True})[0] == 200
                wait_phase('complete')
                assert (workspace / 'result.txt').read_text() == 'approved through Docker'
                status, history = api(f"sessions/{run['session']}/messages")
                assert status == 200
                assert history['entries'][-1]['message']['content'] == 'Container round trip complete'
                assert api('run', {'request_id': request_id, 'session': run['session'], 'operation': {'action': 'retry'}})[0] == 409
                assert Model.calls == 2
                print('Docker smoke passed: auth, IPv4 host routing, approval, host write, durable history, no duplicate replay.')
                process.send_signal(signal.SIGINT)
                process.wait(timeout=10)
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        subprocess.run(compose + ['down'], env=environment, timeout=30, check=False, stdout=subprocess.DEVNULL)
        endpoint.shutdown()


if __name__ == '__main__':
    main()
