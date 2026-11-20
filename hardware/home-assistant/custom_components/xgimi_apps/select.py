"""Selecting an installed app sends a launch command."""
from homeassistant.components.select import SelectEntity
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.update_coordinator import CoordinatorEntity
from . import DOMAIN


async def async_setup_entry(hass, entry, async_add_entities):
    async_add_entities([AppsSelect(entry)])


class AppsSelect(CoordinatorEntity, SelectEntity):
    _attr_has_entity_name = True
    _attr_name = '打开应用'
    _attr_icon = 'mdi:apps'

    def __init__(self, entry):
        super().__init__(entry.runtime_data)
        self._attr_unique_id = f'{entry.unique_id}_apps'
        self._attr_device_info = DeviceInfo(identifiers={(DOMAIN, entry.unique_id)}, name=entry.title, manufacturer='XGIMI')

    @property
    def options(self):
        return [a['label'] for a in self.coordinator.data or []]

    @property
    def current_option(self):
        app = self.coordinator.last_requested
        return app['label'] if app and app['label'] in self.options else None

    @property
    def extra_state_attributes(self):
        return {'apps': self.coordinator.data or [], 'selection_means': 'last_requested_app', 'launch_confirmed': False}

    async def async_select_option(self, option):
        await self.coordinator.open_app(option)
