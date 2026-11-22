#!/usr/bin/env python3
"""Read-only smoke for teacher MCP. Does not play speech, capture media or send messages."""
import json
import os
from pathlib import Path
import subprocess
import sys

binary = str(Path(sys.argv[1]).resolve())
env = dict(os.environ)
env.setdefault('MR_LI_DATA_DIR', str(Path('.data').resolve()))
requests = [
    {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': '2024-11-05'}},
    {'jsonrpc': '2.0', 'method': 'notifications/initialized'},
    {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'},
    {'jsonrpc': '2.0', 'id': 3, 'method': 'tools/call', 'params': {
        'name': 'teacher_query', 'arguments': {'request_token': 'invalid-smoke-context', 'kind': 'status'}}},
]
r = subprocess.run([binary, 'builtin-mcp'], input=''.join(json.dumps(x)+'\n' for x in requests),
                   env=env, capture_output=True, text=True, timeout=20)
assert r.returncode == 0, r.stderr
responses = [json.loads(line) for line in r.stdout.splitlines()]
assert [r['id'] for r in responses] == [1, 2, 3]
tools = responses[1]['result']['tools']
assert {t['name'] for t in tools} == {'teacher_speak', 'teacher_ask_user_question', 'teacher_camera', 'teacher_reframe', 'teacher_posture', 'teacher_notice', 'teacher_query', 'teacher_proxy', 'teacher_music', 'teacher_file', 'teacher_image'}
for t in tools:
    assert 'request_token' in t['inputSchema']['required']
    assert t['inputSchema']['additionalProperties'] is False
assert responses[2]['result']['isError'] is True
message = responses[2]['result']['content'][0]['text']
assert '上下文已失效' in message, message
print(f'PASS: stdio handshake, {len(tools)} tool schemas, live bridge connection and invalid-context rejection')
