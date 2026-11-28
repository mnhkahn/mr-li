#!/usr/bin/env python3
"""Provision over physical USB; credentials and API keys never appear in output."""
import argparse
from getpass import getpass
import json
import os
from pathlib import Path
import secrets
import tempfile
import time
from mcp_bridge import Hardware


def private_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as output:
        temporary = Path(output.name)
        try:
            json.dump(value, output, indent=2)
            output.write('\n')
            output.close()
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', required=True)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument('--use-saved-wifi', action='store_true', help='Import original firmware wifi namespace on device')
    group.add_argument('--credentials-file', type=Path, help='Private JSON containing ssid and password; 2.4 GHz Wi-Fi')
    group.add_argument('--ssid', help='Add or update a 2.4 GHz network and enter its password without echo')
    parser.add_argument('--connection-file', type=Path, default=Path(__file__).parent / '.local/connection.json')
    args = parser.parse_args()
    previous = json.loads(args.connection_file.read_text()) if args.connection_file.exists() else {}
    token = previous.get('api_token') or secrets.token_hex(32)
    params = {'api_token': token}
    if args.use_saved_wifi:
        params['use_saved_wifi'] = True
    elif args.ssid is not None:
        params.update(ssid=args.ssid, password=getpass('Wi-Fi password: '))
    else:
        creds = json.loads(args.credentials_file.read_text())
        params.update(ssid=creds['ssid'], password=creds['password'])
    board = Hardware(args.port)
    try:
        current = board.call('network.status', {})
        if 'saved_ssids' not in current and current.get('configured'):
            raise SystemExit('Upgrade ESP32 firmware before adding a network to preserve the existing Wi-Fi.')
        # Save a newly generated key before provisioning so an interruption cannot lose it.
        if not previous.get('api_token'):
            private_json(args.connection_file, {**previous, 'api_token': token})
        print(json.dumps(board.call('network.configure', params), ensure_ascii=False))
        print(json.dumps(board.call('network.restart', {})))
    finally:
        board.close()
    time.sleep(3)
    deadline = time.monotonic() + 95
    status = None
    while time.monotonic() < deadline:
        try:
            status = board.call('network.status', {})
            if status.get('connected') or status.get('error'):
                break
        except (OSError, RuntimeError):
            pass
        time.sleep(2)
    board.close()
    if status:
        print(json.dumps(status, ensure_ascii=False))
        base = 'http://' + status['hostname'] + '.local'
        private_json(args.connection_file, {'api_token': token, 'base_url': base, 'last_ip': status['ip']})
        # A separate file is suitable for merging into HA secrets.yaml, never source control.
        path = args.connection_file.parent / 'ha-secrets.yaml'
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, 'w') as output:
            for key, value in {
                'mr_li_esp32_status_url': base + '/api/status',
                'mr_li_esp32_wake_url': base + '/api/xgimi/power_on',
                'mr_li_esp32_authorization': 'Bearer ' + token,
            }.items():
                output.write(key + ': ' + json.dumps(value) + '\n')
    if not status or not status.get('connected'):
        raise SystemExit('Wi-Fi not connected. Check network.status; settings remain saved. API key was not printed.')


if __name__ == '__main__':
    main()
