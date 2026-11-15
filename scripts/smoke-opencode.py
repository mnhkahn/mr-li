#!/usr/bin/env python3
"""Isolated OpenCode V2 contract check; preserves HOME for macOS compatibility launchers."""
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
import urllib.parse
import urllib.request

binary = str(Path(sys.argv[1]).resolve())
version = subprocess.check_output([binary, '--version'], text=True).strip()
assert any(part.lstrip('v').startswith('2.3.') for part in version.split()), 'OpenCode V2.3.x required'
with tempfile.TemporaryDirectory(prefix='mr-li-v2-smoke-') as temp:
    root = Path(temp)
    workspace = root/'workspace'
    workspace.mkdir()
    config = json.loads((Path(__file__).resolve().parent.parent/'opencode.example.json').read_text())
    (workspace/'opencode.json').write_text(json.dumps(config))
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    password = secrets.token_hex(24)
    # HOME must stay intact: the macOS-12 launcher locates its patched ICU bundle through it.
    env = {k:v for k,v in os.environ.items() if not k.startswith(('OPENCODE_', 'XDG_'))
           and k.lower() not in ['http_proxy','https_proxy','all_proxy']}
    env.update(XDG_CONFIG_HOME=temp+'/config', XDG_DATA_HOME=temp+'/data',
               XDG_STATE_HOME=temp+'/state', XDG_CACHE_HOME=temp+'/cache',
               OPENCODE_DB=temp+'/test.db', OPENCODE_SERVER_PASSWORD=password,
               OPENCODE_SERVER_USERNAME='opencode', NO_PROXY='*', no_proxy='*')
    auth = 'Basic '+base64.b64encode(('opencode:'+password).encode()).decode()
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    def request(path, data=None, method=None, authorized=True):
        headers = {'Content-Type':'application/json'}
        if authorized:
            headers['Authorization'] = auth
        req = urllib.request.Request(f'http://127.0.0.1:{port}'+path,
            data=None if data is None else json.dumps(data).encode(),headers=headers,method=method)
        with opener.open(req, timeout=20) as response:
            raw = response.read()
            return json.loads(raw) if raw else None
    with (root/'server.log').open('w') as log:
        proc = subprocess.Popen([binary, 'serve', '--hostname', '127.0.0.1', '--port', str(port)],
            cwd=workspace,env=env,stdout=log,stderr=log)
        try:
            for _ in range(100):
                try:
                    info = request('/api/info')
                    break
                except (OSError, urllib.error.URLError):
                    if proc.poll() is not None:
                        raise RuntimeError('OpenCode exited during startup')
                    time.sleep(.2)
            else:
                raise RuntimeError('OpenCode V2 did not become ready')
            assert info['version'].startswith('2.3.')
            try:
                request('/api/info',authorized=False)
                raise AssertionError('Unauthenticated access accepted')
            except urllib.error.HTTPError as error:
                assert error.code == 401
            session = request('/api/session',{'title':'mr-li isolated V2 contract check',
                'location':{'directory':str(workspace)}})['data']
            sid = session['id']
            assert request('/api/session/'+sid)['data']['id'] == sid
            assert request('/api/session/'+sid+'/permission')['data'] == []
            assert request('/api/session/'+sid+'/form')['data'] == []
            entries = request('/api/config?'+urllib.parse.urlencode({'location[directory]':str(workspace)}))
            assert any(e.get('info',{}).get('permissions') for e in entries)
            assert request('/api/session/'+sid+'/interrupt',{}) == {'interrupted':False}
            request('/api/session/'+sid,method='DELETE')
            print('PASS: OpenCode V2 health, auth, sessions, permissions, forms, config and interrupt; no model called')
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
