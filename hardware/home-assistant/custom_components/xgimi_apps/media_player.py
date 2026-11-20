"""One standard HA endpoint for BLE wake and LAN projector control."""
from homeassistant.components.media_player import (
    MediaPlayerEntity, MediaPlayerEntityFeature, MediaPlayerDeviceClass, MediaPlayerState,
)
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.event import async_track_state_change_event
from homeassistant.helpers.update_coordinator import CoordinatorEntity
from . import DOMAIN


async def async_setup_entry(hass, entry, async_add_entities):
    async_add_entities([ProjectorControl(entry)])


class ProjectorControl(CoordinatorEntity, MediaPlayerEntity):
    _attr_has_entity_name = True
    _attr_name = '统一控制'
    _attr_device_class = MediaPlayerDeviceClass.TV
    _attr_icon = 'mdi:projector'

    def __init__(self, entry):
        super().__init__(entry.runtime_data)
        self.remote = entry.options.get('remote_entity')
        self.wake_script = entry.options.get('wake_script')
        self._attr_unique_id = f'{entry.unique_id}_control'
        self._attr_device_info = DeviceInfo(identifiers={(DOMAIN, entry.unique_id)}, name=entry.title, manufacturer='XGIMI')
        features = MediaPlayerEntityFeature.SELECT_SOURCE
        if self.remote:
            features |= MediaPlayerEntityFeature.TURN_OFF | MediaPlayerEntityFeature.VOLUME_STEP
        if self.wake_script:
            features |= MediaPlayerEntityFeature.TURN_ON
        self._attr_supported_features = features

    @property
    def available(self):
        # Gateway control remains available with the projector OFF, so wake can
        # still be discovered/called after HA starts while the projector is off.
        return True

    @property
    def state(self):
        # The old remote probes RTSP port 554, which may close while an APP is
        # running. Use the actual app/control interface as the reachability signal.
        return MediaPlayerState.ON if self.coordinator.last_update_success else MediaPlayerState.OFF

    @property
    def source_list(self):
        return [app['label'] for app in self.coordinator.data or []]

    @property
    def extra_state_attributes(self):
        return {'apps': self.coordinator.data or [], 'last_requested_app': self.coordinator.last_requested,
                'launch_confirmed': False}

    async def async_added_to_hass(self):
        await super().async_added_to_hass()
        if self.remote:
            async def remote_changed(event):
                self.async_write_ha_state()
                new = event.data.get('new_state')
                if new:
                    await self.coordinator.async_request_refresh()
            self.async_on_remove(async_track_state_change_event(self.hass, [self.remote], remote_changed))

    async def async_turn_on(self):
        if not self.wake_script:
            raise HomeAssistantError('Configure the BLE wake script in XGIMI Apps options')
        await self.hass.services.async_call('script', self.wake_script.split('.', 1)[1], {}, blocking=True)

    async def remote_call(self, service, **data):
        if not self.remote:
            raise HomeAssistantError('Configure the existing Wi-Fi remote in XGIMI Apps options')
        await self.hass.services.async_call('remote', service, data, target={'entity_id': self.remote}, blocking=True)

    async def async_turn_off(self):
        await self.remote_call('turn_off')

    async def async_volume_up(self):
        await self.remote_call('send_command', command=['volumeup'])

    async def async_volume_down(self):
        await self.remote_call('send_command', command=['volumedown'])

    async def async_select_source(self, source):
        await self.coordinator.open_app(source)
