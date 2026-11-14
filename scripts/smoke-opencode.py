#!/usr/bin/env python3
"""Isolated API contract smoke test; does not submit prompts or call Feishu."""
import base64
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

binary = str(Path(sys.argv[1]).resolve())
assert subprocess.check_output([binary, '--version'], text=True).strip() == '1.19.0'
with tempfile.TemporaryDirectory(prefix='mr-li-smoke-') as temp:
    root = Path(temp)
    workspace = root / 'workspace'
    workspace.mkdir()
    config = json.loads((Path(__file__).resolve().parent.parent / 'opencode.example.json').read_text())
    (workspace / 'opencode.json').write_text(json.dumps(config))
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    password = secrets.token_hex(24)
    env = {k: v for k, v in os.environ.items() if not k.startswith(('OPENCODE_', 'XDG_'))}
    env.update(HOME=temp, XDG_CONFIG_HOME=temp+'/config', XDG_DATA_HOME=temp+'/data',
               XDG_STATE_HOME=temp+'/state', XDG_CACHE_HOME=temp+'/cache',
               OPENCODE_SERVER_PASSWORD=password, OPENCODE_SERVER_USERNAME='opencode')
    auth = 'Basic ' + base64.b64encode(('opencode:'+password).encode()).decode()
    def request(path, data=None, method=None, authorized=True):
        headers = {'Content-Type': 'application/json'}
        if authorized:
            headers['Authorization'] = auth
        req = urllib.request.Request(f'http://127.0.0.1:{port}'+path,
            data=None if data is None else json.dumps(data).encode(), headers=headers, method=method)
        with urllib.request.urlopen(req, timeout=3) as response:
            raw = response.read()
            return json.loads(raw) if raw else None
    with (root / 'server.log').open('w') as log:
        proc = subprocess.Popen([binary, 'serve', '--hostname', '127.0.0.1', '--port', str(port)],
            cwd=workspace, env=env, stdout=log, stderr=log)
        try:
            for _ in range(100):
                try:
                    health = request('/global/health')
                    break
                except (OSError, urllib.error.URLError):
                    if proc.poll() is not None:
                        raise RuntimeError('OpenCode exited during startup')
                    time.sleep(.2)
            else:
                raise RuntimeError('OpenCode did not become ready')
            assert health == {'healthy': True, 'version': '1.19.0'}, health
            try:
                request('/global/health', authorized=False)
                raise AssertionError('Unauthenticated access was accepted')
            except urllib.error.HTTPError as error:
                assert error.code == 401
            session = request('/session', {'title': 'mr-li isolated contract test'})
            sid = session['id']
            assert request('/session/'+sid)['id'] == sid
            assert isinstance(request('/permission'), list)
            assert isinstance(request('/question'), list)
            assert request('/config')['permission']['*'] == 'ask'
            assert request('/session/'+sid+'/abort', {}) is True
            request('/session/'+sid, method='DELETE')
            print('PASS: OpenCode 1.19.0 health, auth, sessions, permissions, questions, config, abort')
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
