import importlib.util
from pathlib import Path
import json
import subprocess
import sys
import unittest

spec = importlib.util.spec_from_file_location("mcp_bridge", Path(__file__).parents[1] / "mcp_bridge.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class Serial:
    def __init__(self, respond=True):
        self.pending = []
        self.writes = []
        self.closed = False
        self.respond = respond

    def write(self, data):
        self.writes.append(data)
        req = json.loads(data)
        if self.respond:
            reply = b'MRLI_HW_RESULT ' + json.dumps({"id": req["id"], "result": {"state": "transmitted", "physical_effect_confirmed": False}}).encode() + b'\n'
            self.pending = [b'ROM startup\n', b'MRLI_HW_RESULT {"id":"stale"}\n', reply[:20], reply[20:]]
        return len(data)

    def flush(self):
        pass

    def read_until(self, *_):
        return self.pending.pop(0) if self.pending else b''

    def close(self):
        self.closed = True


class Tests(unittest.TestCase):
    def test_stdio_protocol_without_device(self):
        messages = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        ]
        process = subprocess.run([sys.executable, str(Path(bridge.__file__)), "--port", "not-connected"],
                                 input="\n".join(json.dumps(x) for x in messages) + "\n", capture_output=True, text=True, check=True)
        replies = [json.loads(x) for x in process.stdout.splitlines()]
        self.assertEqual([x["id"] for x in replies], [1, 2])
        self.assertEqual(replies[0]["result"]["serverInfo"]["name"], "mr-li-esp32-hardware")
        self.assertEqual(len(replies[1]["result"]["tools"]), 13)
        self.assertEqual(process.stderr, "")

    def test_usb_fragmentation_startup_and_stale_reply(self):
        serial = Serial()
        hardware = bridge.Hardware("fake", connection=serial)
        request = {"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "self.xgimi.power_on", "arguments": {}}}
        reply = bridge.handle(request, hardware)
        self.assertFalse(reply["result"]["isError"])
        self.assertFalse(reply["result"]["structuredContent"]["physical_effect_confirmed"])
        self.assertEqual(json.loads(serial.writes[0])["method"], "xgimi.power_on")

    def test_timeout_never_resends(self):
        serial = Serial(respond=False)
        hardware = bridge.Hardware("fake", timeout=0.001, connection=serial)
        with self.assertRaises(TimeoutError):
            hardware.call("xgimi.power_on", {})
        self.assertEqual(len(serial.writes), 1)
        self.assertTrue(serial.closed)

    def test_discovery_does_not_open_hardware(self):
        hardware = bridge.Hardware("nonexistent")
        reply = bridge.handle({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}, hardware)
        self.assertEqual(len(reply["result"]["tools"]), 13)
        self.assertIsNone(hardware.connection)
        self.assertIsNone(bridge.handle({"jsonrpc": "2.0", "method": "notifications/initialized"}, hardware))

    def test_unknown_and_bad_args_do_not_touch_hardware(self):
        hardware = bridge.Hardware("nonexistent")
        for name, args in [("arbitrary", {}), ("self.xgimi.configure", {}), ("self.xgimi.power_on", {"token": "bad"})]:
            reply = bridge.handle({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}, hardware)
            self.assertTrue(reply["result"]["isError"])
            self.assertIsNone(hardware.connection)

    def test_curtain_commands_route_without_claiming_motion(self):
        for action in ("up", "down", "stop"):
            serial = Serial()
            hardware = bridge.Hardware("fake", connection=serial)
            reply = bridge.handle({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "self.curtain." + action}}, hardware)
            self.assertFalse(reply["result"]["isError"])
            self.assertFalse(reply["result"]["structuredContent"]["physical_effect_confirmed"])
            self.assertEqual(json.loads(serial.writes[0])["method"], "curtain." + action)

    def test_rf_learning_and_enable_validation_before_usb(self):
        for action,args in [('learn',{'action':'other'}),('learn',{'action':'up','seconds':0}),('setup',{'tx_gpio':21,'rx_gpio':21}),('enable',{'enabled':True}),('enable',{'enabled':'true'})]:
            h=bridge.Hardware('not-connected')
            r=bridge.handle({'jsonrpc':'2.0','id':1,'method':'tools/call','params':{'name':'self.curtain.'+action,'arguments':args}},h)
            self.assertTrue(r['result']['isError'])
            self.assertIsNone(h.connection)
        serial=Serial();h=bridge.Hardware('fake',connection=serial)
        r=bridge.handle({'jsonrpc':'2.0','id':1,'method':'tools/call','params':{'name':'self.curtain.learn','arguments':{'action':'stop','seconds':5}}},h)
        self.assertFalse(r['result']['isError'])
        self.assertEqual(json.loads(serial.writes[0])['method'],'curtain.learn')

    def test_invalid_rf_waveforms_never_reach_gpio(self):
        valid = {"protocol": "raw_ook_fixed", "tx_gpio": 4, "repeat": 6, "up": [350, -1050], "down": [1050, -350], "stop": [350, -350]}
        bridge.validate_curtain(valid)
        for patch in [{"protocol": "somfy_rolling"}, {"repeat": 0}, {"tx_gpio": True}, {"up": []},
                      {"down": [350, 350]}, {"stop": [40000, -350]}, {"up": [30000, -30000] * 3}]:
            hardware = bridge.Hardware("not-connected")
            reply = bridge.handle({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "self.curtain.configure", "arguments": dict(valid, **patch)}}, hardware)
            self.assertTrue(reply["result"]["isError"])
            self.assertIsNone(hardware.connection)


if __name__ == "__main__":
    unittest.main()
