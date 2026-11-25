"""Route Home Assistant ESP32 requests using the board's MAC address."""
import asyncio
import ipaddress
import json
import logging
import re
import socket
import subprocess
import time

from aiohttp import ClientError, ClientTimeout, web
import voluptuous as vol

from homeassistant.const import EVENT_HOMEASSISTANT_STOP
from homeassistant.helpers.aiohttp_client import async_get_clientsession

DOMAIN = "mr_li_esp32_lan"
PORT = 18320
STATUS = "/api/status"
POST_PATHS = {
    "/api/xgimi/power_on",
    "/api/curtain/enable",
    *(f"/api/curtain/{action}" for action in ("up", "down", "stop")),
    *(f"/api/curtain/learn/{action}" for action in ("up", "down", "stop")),
}
MAC = r"[0-9a-fA-F]{2}(?::[0-9a-fA-F]{2}){5}"
CONFIG_SCHEMA = vol.Schema(
    {DOMAIN: vol.Schema({vol.Required("mac"): vol.Match(MAC + "$")})},
    extra=vol.ALLOW_EXTRA,
)
_LOGGER = logging.getLogger(__name__)


def output(command, *args):
    return subprocess.check_output(
        [command, *args], text=True, stderr=subprocess.DEVNULL, timeout=1
    ).strip()


def neighbors(mac, interface):
    matches = []
    for line in output("/usr/sbin/arp", "-an", "-i", interface).splitlines():
        entry = re.search(r"\(([\d.]+)\) at ([0-9a-fA-F:]+) ", line)
        if entry:
            address, raw_mac = entry.groups()
            if ":".join(part.zfill(2) for part in raw_mac.lower().split(":")) == mac:
                matches.append(address)
    return matches


class AddressDiscovery:
    def __init__(self, mac):
        self.mac = mac.lower()
        self.last_probe = float("-inf")

    def candidates(self, refresh):
        route = output("/sbin/route", "-n", "get", "default")
        interface = re.search(r"^\s*interface:\s*(\S+)", route, re.M)
        if not interface:
            return []
        interface = interface[1]
        local = output("/usr/sbin/ipconfig", "getifaddr", interface)
        mask = output("/usr/sbin/ipconfig", "getoption", interface, "subnet_mask")
        network = ipaddress.IPv4Network(f"{local}/{mask}", strict=False)
        if refresh and network.num_addresses <= 1024 and time.monotonic() - self.last_probe >= 5:
            self.last_probe = time.monotonic()
            # An on-link UDP datagram triggers ARP even when the recipient's port is closed.
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
                for ip in network.hosts():
                    if str(ip) != local:
                        probe.sendto(b"", (str(ip), 9))
            time.sleep(0.35)
        return [ip for ip in neighbors(self.mac, interface) if ipaddress.IPv4Address(ip) in network]


async def async_setup(hass, config):
    discovery = AddressDiscovery(config[DOMAIN]["mac"])
    hostname = "mr-li-esp32-" + discovery.mac.replace(":", "")[-6:]
    session = async_get_clientsession(hass)
    lock = asyncio.Lock()

    async def board_request(ip, method, path, headers, body=None, timeout=1.5):
        async with session.request(
            method, f"http://{ip}{path}", headers=headers, data=body,
            allow_redirects=False, timeout=ClientTimeout(total=timeout),
        ) as response:
            return response.status, response.headers.get("Content-Type"), await response.read()

    async def locate(headers):
        async with lock:
            checked = set()
            for refresh in (False, True):
                candidates = await hass.async_add_executor_job(discovery.candidates, refresh)
                for ip in candidates:
                    if ip in checked:
                        continue
                    checked.add(ip)
                    try:
                        status, content_type, body = await board_request(
                            ip, "GET", STATUS, headers, timeout=0.55
                        )
                        if status == 200:
                            if json.loads(body).get("device") == hostname:
                                return ip, (status, content_type, body)
                    except (ClientError, OSError, ValueError):
                        continue
            return None

    async def relay(request):
        authorization = request.headers.get("Authorization", "")
        if not authorization.startswith("Bearer "):
            raise web.HTTPUnauthorized()
        headers = {"Authorization": authorization}
        if "Content-Type" in request.headers:
            headers["Content-Type"] = request.headers["Content-Type"]
        body = await request.read() if request.method == "POST" else None
        try:
            target = await locate(headers)
        except (OSError, subprocess.SubprocessError, ValueError):
            target = None
        if target is None:
            return web.json_response({"error": "ESP32 address could not be verified on the LAN"}, status=503)
        ip, result = target
        if request.method == "POST":
            try:
                result = await board_request(ip, request.method, request.path, headers, body)
            except (ClientError, OSError):
                return web.json_response({"error": "ESP32 command outcome unknown"}, status=502)
        status, content_type, data = result
        return web.Response(
            status=status, headers={"Content-Type": content_type or "application/json"}, body=data
        )

    app = web.Application(client_max_size=513)
    app.router.add_get(STATUS, relay, allow_head=False)
    for path in sorted(POST_PATHS):
        app.router.add_post(path, relay)
    runner = web.AppRunner(app, access_log=None)
    await runner.setup()
    try:
        await web.TCPSite(runner, "127.0.0.1", PORT).start()
    except OSError:
        await runner.cleanup()
        _LOGGER.exception("Could not start the ESP32 LAN relay")
        return False

    async def stop(_event):
        await runner.cleanup()

    hass.bus.async_listen_once(EVENT_HOMEASSISTANT_STOP, stop)
    return True
