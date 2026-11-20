import importlib.util
import json
from pathlib import Path
import unittest
from urllib.parse import quote

path = Path(__file__).parents[1] / 'custom_components/xgimi_apps/api.py'
spec = importlib.util.spec_from_file_location('xgimi_app_api', path)
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)


class AppProtocolTests(unittest.TestCase):
    def test_encoded_catalog_and_literal_percent(self):
        data = {'appList': [{'appName': '云视听小电视 100%20', 'packageName': 'com.xiaodianshi.tv.yst'}]}
        raw = json.dumps(data, ensure_ascii=False)
        self.assertEqual(api.parse_apps(raw), api.parse_apps(quote(raw)))
        self.assertEqual(api.parse_apps(raw)[0]['name'], '云视听小电视 100%20')

    def test_duplicate_names_require_package_or_label(self):
        apps = api.parse_apps(json.dumps({'appList': [
            {'appName': '电视', 'packageName': 'com.one.tv'},
            {'appName': '电视', 'packageName': 'com.two.tv'}]}))
        with self.assertRaises(ValueError): api.resolve_app(apps, '电视')
        self.assertEqual(api.resolve_app(apps, 'com.two.tv')['package'], 'com.two.tv')
        self.assertEqual(api.resolve_app(apps, apps[0]['label']), apps[0])

    def test_uninstalled_app_and_malformed_catalog_rejected(self):
        with self.assertRaises(ValueError): api.resolve_app([], 'com.unknown.app')
        for value in ({}, {'appList': 'bad'}, {'appList': [{'appName':'X','packageName':'a; command'}]}):
            with self.assertRaises(ValueError): api.parse_apps(json.dumps(value))

    def test_launch_protocol_not_remote_key_mode(self):
        packet = json.loads(api.launch_packet('com.xiaodianshi.tv.yst'))
        self.assertEqual(packet['action'], 20000)
        self.assertEqual(packet['controlCmd'], {'data':'com.xiaodianshi.tv.yst','delayTime':0,'mode':7,'time':0,'type':1})
        with self.assertRaises(ValueError): api.launch_packet('com.fake.app"}')

    def test_host_cannot_insert_port_path_or_credentials(self):
        for host in ('http://192.168.1.11', 'host/path', 'user@host', 'host:1234'):
            with self.assertRaises(ValueError): api.AppApi(None, host)


if __name__ == '__main__': unittest.main()
