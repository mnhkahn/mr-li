"""Run with HA's Python to check the gateway routes without controlling a device."""
import importlib.util
import sys
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, call

HAS_HA = importlib.util.find_spec('homeassistant') is not None
if HAS_HA:
    sys.path.insert(0, str(Path(__file__).parents[1]))
    from homeassistant.components.media_player import MediaPlayerEntityFeature, MediaPlayerState
    from custom_components.xgimi_apps.media_player import ProjectorControl


@unittest.skipUnless(HAS_HA, 'Requires the installed Home Assistant Python environment')
class ProjectorControlTests(unittest.IsolatedAsyncioTestCase):
    def make_control(self, options=None):
        apps = [{'name':'云视听小电视','label':'云视听小电视','package':'com.xiaodianshi.tv.yst'}]
        coordinator = SimpleNamespace(data=apps, last_update_success=False, last_requested=None, open_app=AsyncMock())
        entry = SimpleNamespace(runtime_data=coordinator, unique_id='192.168.1.11', title='投影', options=options if options is not None else {'remote_entity':'remote.projector','wake_script':'script.ble_wake'})
        entity = ProjectorControl(entry)
        entity.hass = SimpleNamespace(states=SimpleNamespace(get=lambda _: SimpleNamespace(state='off')), services=SimpleNamespace(async_call=AsyncMock()))
        return entity

    async def test_offline_projector_still_exposes_ble_wake(self):
        e = self.make_control()
        self.assertTrue(e.available)
        self.assertEqual(e.state, MediaPlayerState.OFF)
        self.assertTrue(e.supported_features & MediaPlayerEntityFeature.TURN_ON)
        await e.async_turn_on()
        e.hass.services.async_call.assert_awaited_once_with('script','ble_wake',{},blocking=True)

    async def test_power_off_and_volume_use_existing_wifi_remote(self):
        e = self.make_control()
        await e.async_turn_off()
        await e.async_volume_up()
        await e.async_volume_down()
        self.assertEqual(e.hass.services.async_call.await_args_list, [
            call('remote','turn_off',{},target={'entity_id':'remote.projector'},blocking=True),
            call('remote','send_command',{'command':['volumeup']},target={'entity_id':'remote.projector'},blocking=True),
            call('remote','send_command',{'command':['volumedown']},target={'entity_id':'remote.projector'},blocking=True)])

    async def test_app_catalog_and_launch_use_the_validated_app_api(self):
        e = self.make_control()
        self.assertEqual(e.source_list,['云视听小电视'])
        await e.async_select_source('云视听小电视')
        e.coordinator.open_app.assert_awaited_once_with('云视听小电视')
        self.assertFalse(e.extra_state_attributes['launch_confirmed'])

    async def test_running_app_overrides_the_legacy_closed_rtsp_probe(self):
        e = self.make_control()
        e.coordinator.last_update_success = True
        self.assertEqual(e.state, MediaPlayerState.ON)

    async def test_unconfigured_routes_are_not_advertised(self):
        e = self.make_control({})
        self.assertEqual(e.supported_features, MediaPlayerEntityFeature.SELECT_SOURCE)


if __name__ == '__main__': unittest.main()
