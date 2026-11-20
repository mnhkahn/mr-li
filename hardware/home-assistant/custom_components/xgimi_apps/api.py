"""Local XGIMI application catalog and UDP launch protocol."""
import asyncio
from collections import Counter
import json
import re
from urllib.parse import unquote

LIST_PATH = '/data/data/com.xgimi.vcontrol/app_appDatas/list'
PACKAGE = re.compile(r'[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)+')


def parse_apps(text):
    # Some firmware returns URL-encoded JSON, others return JSON directly.
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        value = json.loads(unquote(text))
    items = value.get('appList') if isinstance(value, dict) else None
    if not isinstance(items, list):
        raise ValueError('Projector returned an invalid appList')
    apps = {}
    for item in items:
        if not isinstance(item, dict):
            continue
        name, package = item.get('appName'), item.get('packageName')
        if isinstance(name, str) and name.strip() and isinstance(package, str) and PACKAGE.fullmatch(package):
            apps[package] = {'name': name.strip(), 'package': package}
    if items and not apps:
        raise ValueError('Projector returned no valid applications')
    counts = Counter(app['name'] for app in apps.values())
    return [dict(app, label=app['name'] if counts[app['name']] == 1 else f"{app['name']} ({app['package']})") for app in apps.values()]


def resolve_app(apps, requested):
    matches = [a for a in apps if requested in (a['package'], a['name'], a['label'])]
    if len(matches) != 1:
        raise ValueError('App not installed or name ambiguous; use an exact label or package from list_apps')
    return matches[0]


def launch_packet(package):
    if not isinstance(package, str) or not PACKAGE.fullmatch(package):
        raise ValueError('Invalid Android package')
    return json.dumps({'action': 20000, 'controlCmd': {'data': package, 'delayTime': 0, 'mode': 7, 'time': 0, 'type': 1}, 'msgid': '2'}, separators=(',', ':')).encode()


class AppApi:
    def __init__(self, session, host):
        if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9.-]*', host):
            raise ValueError('Use a projector IPv4 address or hostname without scheme/port/path')
        self.session = session
        self.host = host

    async def apps(self):
        async with asyncio.timeout(5):
            async with self.session.get(f'http://{self.host}:16741{LIST_PATH}') as response:
                response.raise_for_status()
                # Bound data read before decoding; installed catalogs are small.
                chunks, size = [], 0
                async for chunk in response.content.iter_chunked(16384):
                    size += len(chunk)
                    if size > 1024 * 1024:
                        raise ValueError('Projector app list exceeds 1 MiB')
                    chunks.append(chunk)
                return parse_apps(b''.join(chunks).decode('utf-8'))

    async def open(self, package):
        payload = launch_packet(package)
        transport, _ = await asyncio.get_running_loop().create_datagram_endpoint(asyncio.DatagramProtocol, remote_addr=(self.host, 16750))
        try:
            transport.sendto(payload)
            # Yield to the transport; UDP has no application acknowledgement.
            await asyncio.sleep(0)
        finally:
            transport.close()
