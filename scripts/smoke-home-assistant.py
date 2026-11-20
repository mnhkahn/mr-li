#!/usr/bin/env python3
"""Live smoke for mr-li's CLI/MCP; optional control uses only a temporary helper."""
import argparse
import asyncio
import json
import os
from pathlib import Path
import shlex
import subprocess
import time


def cli(binary, *args):
    r = subprocess.run([binary, 'ha', *args], capture_output=True, text=True, timeout=35)
    if r.returncode:
        raise RuntimeError(r.stderr.strip())
    return json.loads(r.stdout)


def read_only(binary):
    inventory = cli(binary, 'devices')
    entities = list(inventory['standalone_entities'])
    for device in inventory['devices']:
        entities.extend(device['entities'])
    assert entities, 'HA returned no entities'
    print(f"PASS: CLI discovery: {len(inventory['devices'])} physical devices, {len(entities)} entities")
    sample = next(e for e in entities if e['type'] in ['remote', 'light', 'switch', 'number', 'select']
                  and e['state'] not in ['unknown', 'unavailable'])
    result = cli(binary, 'commands', sample['entity_id'])
    assert result['entities'][0]['commands'], 'No operations found for live device'
    print('PASS: CLI operation discovery:', sample['entity_id'], sorted(result['entities'][0]['commands']))
    requests = [
        {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': '2024-11-05'}},
        {'jsonrpc': '2.0', 'method': 'notifications/initialized'},
        {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'},
        {'jsonrpc': '2.0', 'id': 3, 'method': 'tools/call', 'params': {'name': 'ha_devices', 'arguments': {}}},
    ]
    r = subprocess.run([binary, 'ha-mcp'], input=''.join(json.dumps(x)+'\n' for x in requests),
                       capture_output=True, text=True, timeout=35)
    assert r.returncode == 0, 'MCP process failed'
    responses = [json.loads(line) for line in r.stdout.splitlines()]
    assert [x['id'] for x in responses] == [1, 2, 3]
    assert {t['name'] for t in responses[1]['result']['tools']} == {'ha_devices', 'ha_commands', 'ha_control'}
    assert responses[2]['result']['isError'] is False
    assert {d['device_id'] for d in responses[2]['result']['structuredContent']['devices']} == {d['device_id'] for d in inventory['devices']}, 'MCP and CLI device IDs differ'
    print('PASS: stdio MCP handshake, tool schemas and live discovery')


async def control(binary):
    import aiohttp  # Available in HA's Python environment; read-only mode needs stdlib only.
    values = {}
    path = Path(os.environ.get('MR_LI_ENV', '.env'))
    for line in path.read_text().splitlines():
        if line.strip().startswith('#') or '=' not in line:
            continue
        key, value = line.split('=', 1)
        parts = shlex.split(value, comments=True)
        values[key.strip()] = parts[0] if parts else ''
    setting = lambda k: os.environ.get(k, values.get(k, ''))
    token = setting('XIAOLI_HA_TOKEN')
    if not token and setting('XIAOLI_HA_TOKEN_FILE'):
        token = Path(setting('XIAOLI_HA_TOKEN_FILE')).read_text().strip()
    url = setting('XIAOLI_HA_URL').rstrip('/')+'/api/websocket'
    async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=30)) as session:
        async with session.ws_connect(url) as ws:
            assert (await ws.receive_json())['type'] == 'auth_required'
            await ws.send_json({'type': 'auth', 'access_token': token})
            assert (await ws.receive_json())['type'] == 'auth_ok', 'HA authentication failed'
            sequence = 0
            async def call(kind, **params):
                nonlocal sequence
                sequence += 1
                await ws.send_json({'id': sequence, 'type': kind, **params})
                while True:
                    result = await ws.receive_json()
                    if result.get('id') == sequence:
                        assert result.get('success'), 'HA helper management failed'
                        return result.get('result')
            helper = await call('input_boolean/create', name=f'Mr Li HA smoke {time.time_ns()}', initial=False)
            helper_id = helper['id']
            entity = 'input_boolean.'+helper_id
            try:
                for desired in ['on', 'off']:
                    request = {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {
                        'name': 'ha_control', 'arguments': {'entity_id': entity, 'command': 'input_boolean.turn_'+desired}}}
                    r = await asyncio.to_thread(subprocess.run, [binary, 'ha-mcp'], input=json.dumps(request)+'\n',
                                                capture_output=True, text=True, timeout=35)
                    assert r.returncode == 0, 'Control MCP process failed'
                    result = json.loads(r.stdout)['result']
                    assert result['isError'] is False, 'Control tool failed'
                    value = result['structuredContent']
                    assert value['accepted'] and value['observed_state']['state'] == desired
                    print('PASS: MCP ha_control accepted and read back', desired)
            finally:
                await call('input_boolean/delete', input_boolean_id=helper_id)
                states = await call('get_states')
                assert all(s['entity_id'] != entity for s in states), 'Temporary helper remains'
                print('PASS: temporary helper deleted and removal verified')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('binary')
    parser.add_argument('--control', action='store_true')
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    read_only(binary)
    if args.control:
        asyncio.run(control(binary))
