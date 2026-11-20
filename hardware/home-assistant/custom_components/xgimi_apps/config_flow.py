"""Configure a local projector address."""
import aiohttp
import voluptuous as vol
from homeassistant import config_entries
from homeassistant.core import callback
from homeassistant.helpers import selector
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from . import DOMAIN
from .api import AppApi


class ConfigFlow(config_entries.ConfigFlow, domain=DOMAIN):
    VERSION = 1

    @staticmethod
    @callback
    def async_get_options_flow(config_entry):
        return ControlOptionsFlow()

    async def async_step_user(self, user_input=None):
        errors = {}
        if user_input is not None:
            host = user_input['host'].strip().lower()
            await self.async_set_unique_id(host)
            self._abort_if_unique_id_configured()
            try:
                await AppApi(async_get_clientsession(self.hass), host).apps()
            except (aiohttp.ClientError, TimeoutError, ValueError, UnicodeError, OSError):
                errors['base'] = 'cannot_connect'
            else:
                return self.async_create_entry(title=user_input['name'], data={'host': host})
        return self.async_show_form(step_id='user', data_schema=vol.Schema({vol.Required('host', default=(user_input or {}).get('host', '')): str, vol.Required('name', default=(user_input or {}).get('name', '极米投影应用')): str}), errors=errors)


class ControlOptionsFlow(config_entries.OptionsFlow):
    async def async_step_init(self, user_input=None):
        errors = {}
        if user_input is not None:
            for field, domain in [('remote_entity', 'remote'), ('wake_script', 'script')]:
                value = user_input.get(field)
                if value and (not value.startswith(domain + '.') or not self.hass.states.get(value)):
                    errors[field] = 'invalid_entity'
            if not errors:
                return self.async_create_entry(title='', data=user_input)
        values = user_input or self.config_entry.options
        schema = {}
        for field, domain in [('remote_entity', 'remote'), ('wake_script', 'script')]:
            key = vol.Optional(field, default=values[field]) if values.get(field) else vol.Optional(field)
            schema[key] = selector.EntitySelector(selector.EntitySelectorConfig(domain=domain))
        return self.async_show_form(step_id='init', data_schema=vol.Schema(schema), errors=errors)
