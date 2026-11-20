"""Discover and launch XGIMI applications over the local network."""
from datetime import timedelta
import logging
import json
import aiohttp
import voluptuous as vol
from homeassistant.const import Platform
from homeassistant.core import SupportsResponse
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import config_validation as cv
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.storage import Store
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed
from .api import AppApi, resolve_app, parse_apps

DOMAIN = 'xgimi_apps'
_LOGGER = logging.getLogger(__name__)


class AppsCoordinator(DataUpdateCoordinator):
    def __init__(self, hass, entry):
        super().__init__(hass, _LOGGER, name=entry.title, config_entry=entry, update_interval=timedelta(seconds=30))
        self.api = AppApi(async_get_clientsession(hass), entry.data['host'])
        self.last_requested = None
        self.store = Store(hass, 1, f"xgimi_apps.{entry.entry_id}.apps")

    async def _async_update_data(self):
        try:
            apps = await self.api.apps()
            self.update_interval = timedelta(seconds=30)
            if apps != self.data:
                self.store.async_delay_save(lambda: {"apps": apps}, 5)
            return apps
        except (aiohttp.ClientError, TimeoutError, ValueError, UnicodeError) as e:
            self.update_interval = timedelta(seconds=30)
            raise UpdateFailed(f'Cannot read projector applications: {e}') from e

    async def open_app(self, requested):
        # Check reachability and current installation before sending. Do not wake
        # the projector, replay UDP, or claim foreground launch confirmation.
        await self.async_refresh()
        if not self.last_update_success:
            raise HomeAssistantError('Projector app interface is unavailable; power it on first')
        try:
            app = resolve_app(self.data, requested)
            await self.api.open(app['package'])
        except (ValueError, OSError) as e:
            raise HomeAssistantError(str(e)) from e
        self.last_requested = app
        self.async_update_listeners()
        return {'sent': True, 'app': app, 'launch_confirmed': False}


async def async_setup_entry(hass, entry):
    coordinator = AppsCoordinator(hass, entry)
    saved = await coordinator.store.async_load()
    if isinstance(saved, dict) and isinstance(saved.get('apps'), list):
        try:
            coordinator.data = parse_apps(json.dumps({'appList': [
                {'appName': a['name'], 'packageName': a['package']} for a in saved['apps']]}))
        except (ValueError, KeyError, TypeError):
            _LOGGER.warning('Ignoring invalid saved projector app list')
    # Offline is normal: retain the gateway entity so BLE power-on still works.
    await coordinator.async_refresh()
    entry.runtime_data = coordinator
    hass.data.setdefault(DOMAIN, {})[entry.entry_id] = coordinator

    if not hass.services.has_service(DOMAIN, 'list_apps'):
        def target(call):
            entries = hass.data[DOMAIN]
            if entry_id := call.data.get('entry_id'):
                if entry_id not in entries:
                    raise HomeAssistantError('Unknown XGIMI Apps config entry')
                return entries[entry_id]
            if len(entries) != 1:
                raise HomeAssistantError('Specify entry_id when multiple projectors are configured')
            return next(iter(entries.values()))

        async def list_apps(call):
            c = target(call)
            if call.data.get('refresh'):
                await c.async_refresh()
            return {'apps': c.data or [], 'projector_online': c.last_update_success, 'cached': not call.data.get('refresh', False) or not c.last_update_success}

        async def open_app(call):
            return await target(call).open_app(call.data['app'])

        common = {vol.Optional('entry_id'): cv.string}
        hass.services.async_register(DOMAIN, 'list_apps', list_apps, schema=vol.Schema({**common, vol.Optional('refresh', default=False): cv.boolean}), supports_response=SupportsResponse.ONLY)
        hass.services.async_register(DOMAIN, 'open_app', open_app, schema=vol.Schema({**common, vol.Required('app'): cv.string}), supports_response=SupportsResponse.OPTIONAL)
    await hass.config_entries.async_forward_entry_setups(entry, [Platform.SELECT, Platform.MEDIA_PLAYER])
    entry.async_on_unload(entry.add_update_listener(async_reload_entry))
    return True


async def async_reload_entry(hass, entry):
    await hass.config_entries.async_reload(entry.entry_id)


async def async_unload_entry(hass, entry):
    if not await hass.config_entries.async_unload_platforms(entry, [Platform.SELECT, Platform.MEDIA_PLAYER]):
        return False
    hass.data[DOMAIN].pop(entry.entry_id, None)
    if not hass.data[DOMAIN]:
        hass.services.async_remove(DOMAIN, 'list_apps')
        hass.services.async_remove(DOMAIN, 'open_app')
    return True
