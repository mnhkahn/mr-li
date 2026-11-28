#!/usr/bin/env python3
"""stdio MCP adapter for the standalone mr-li ESP32 USB hardware controller."""
import argparse
import json
import re
import sys
import time
import uuid

PREFIX = b"MRLI_HW_RESULT "
COMMAND_PATTERN = r"^(network|light|voice|curtain|xgimi)\.[a-z][a-z0-9_]*$"
TOOLS = [
    {"name": "self.hardware.command", "description": "Send any supported USB hardware command to the ESP32. Use configure_wifi.py for passwords. Network, light, voice, curtain, and XGIMI commands are routed to firmware.",
     "inputSchema": {"type": "object", "properties": {
         "method": {"type": "string", "pattern": COMMAND_PATTERN},
         "params": {"type": "object"}}, "required": ["method"], "additionalProperties": False}},
    {"name": "self.xgimi.status", "description": "Query XGIMI BLE configuration and operation status. transmitted does not confirm projector power-on.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
     "annotations": {"readOnlyHint": True}},
    {"name": "self.xgimi.learn", "description": "Learn and save the original XGIMI remote power-on broadcast for 20 seconds. Poll status until learning, then press the original remote power button near the ESP32 with projector off. Poll until learned/error.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
    {"name": "self.xgimi.power_on", "description": "Send the learned XGIMI BLE wake packet for 3 seconds. Returns accepted; poll status. Does not confirm physical power-on.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
    {"name": "self.xgimi.configure", "description": "Save this projector's captured 15/16-byte wake token as hex, excluding manufacturer ID 4600. Prefer learn; never guess another projector's token.",
     "inputSchema": {"type": "object", "properties": {"token": {"type": "string", "minLength": 30, "maxLength": 32}},
                     "required": ["token"], "additionalProperties": False}},
]

for action, description in [("status", "Query external 433 MHz curtain transmitter configuration and operation status"),
                            ("up", "Transmit configured curtain UP waveform"),
                            ("down", "Transmit configured curtain DOWN waveform"),
                            ("stop", "Transmit configured curtain STOP waveform, prioritized at the next frame boundary")]:
    tool = {"name": "self.curtain." + action,
            "description": description + ". Requires an external OOK transmitter such as MX-FS-03V and verified fixed-code waveforms. Not for rolling-code remotes. Does not confirm motor motion.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}}
    if action == "status":
        tool["annotations"] = {"readOnlyHint": True}
    TOOLS.append(tool)

pulse_array = {"type": "array", "minItems": 2, "maxItems": 512,
               "items": {"type": "integer", "minimum": -32767, "maximum": 32767}}
TOOLS.append({"name": "self.curtain.configure",
              "description": "Save disabled GPIO wiring, repeat count and measured UP/DOWN/STOP waveforms for an external 433 MHz OOK transmitter (e.g. MX-FS-03V). Signed microseconds alternate +carrier-on/-carrier-off. Never guess codes from frequency or brand; not for rolling-code protocols. Does not transmit.",
              "inputSchema": {"type": "object", "properties": {
                  "protocol": {"type": "string", "enum": ["raw_ook_fixed"]},
                  "tx_gpio": {"type": "integer", "minimum": 0, "maximum": 48},
                  "repeat": {"type": "integer", "minimum": 1, "maximum": 10},
                  "up": pulse_array, "down": pulse_array, "stop": pulse_array},
                  "required": ["protocol", "tx_gpio", "repeat", "up", "down", "stop"], "additionalProperties": False}})


RF_SETTINGS_TOOLS = {
    "setup": {"description": "Save RF wiring without transmitting. This S3 board uses tx_gpio=21, rx_gpio=47. Remove old LCD wires and use appropriate 5V/3.3V level adapters.", "properties": {"tx_gpio": {"type":"integer","minimum":0,"maximum":48}, "rx_gpio": {"type":"integer","minimum":0,"maximum":48}, "repeat": {"type":"integer","minimum":1,"maximum":10}}, "required":["tx_gpio","rx_gpio"]},
    "learn": {"description": "Receive only. Press the SAME curtain button three separate times, pausing at least one second. Poll status until learned/error; saving a frame disables transmission.", "properties": {"action":{"type":"string","enum":["up","down","stop"]}, "seconds":{"type":"integer","minimum":5,"maximum":60}, "frame_gap_us":{"type":"integer","minimum":3000,"maximum":20000}}, "required":["action"]},
    "enable": {"description": "Explicitly enable verified fixed codes after learning three distinct actions. Enabling requires fixed_code_confirmed=true. Does not transmit.", "properties": {"enabled":{"type":"boolean"}, "fixed_code_confirmed":{"type":"boolean"}}, "required":["enabled"]},
    "export": {"description": "Read one saved waveform as signed microseconds; does not transmit.", "properties":{"action":{"type":"string","enum":["up","down","stop"]}}, "required":["action"]},
}
for action, spec in RF_SETTINGS_TOOLS.items():
    TOOLS.append({"name":"self.curtain."+action,"description":spec["description"],"inputSchema":{"type":"object","properties":spec["properties"],"required":spec["required"],"additionalProperties":False},"annotations":{"readOnlyHint":action=="export"}})

def validate_rf_settings(action, arguments):
    spec=RF_SETTINGS_TOOLS[action]
    if not set(spec["required"]).issubset(arguments) or not set(arguments).issubset(spec["properties"]):
        raise ValueError("Missing or unknown RF setting")
    for key,value in arguments.items():
        rule=spec["properties"][key]
        if rule["type"]=="integer" and (type(value) is not int or not rule["minimum"]<=value<=rule["maximum"]):
            raise ValueError("Invalid RF numeric setting: "+key)
        if rule["type"]=="boolean" and type(value) is not bool:
            raise ValueError("RF flag must be boolean: "+key)
        if "enum" in rule and value not in rule["enum"]:
            raise ValueError("Unknown RF action")
    if action=="enable" and arguments["enabled"] and arguments.get("fixed_code_confirmed") is not True:
        raise ValueError("Confirm fixed_code_confirmed before enabling RF")
    if action=="setup" and arguments["tx_gpio"]==arguments["rx_gpio"]:
        raise ValueError("TX/RX GPIOs must differ")


def validate_curtain(arguments):
    if set(arguments) != {"protocol", "tx_gpio", "repeat", "up", "down", "stop"}:
        raise ValueError("curtain.configure requires protocol, tx_gpio, repeat, up, down and stop")
    if arguments["protocol"] != "raw_ook_fixed":
        raise ValueError("Only verified fixed-code raw OOK waveforms are supported")
    for key, low, high in [("tx_gpio", 0, 48), ("repeat", 1, 10)]:
        value = arguments[key]
        if type(value) is not int or not low <= value <= high:
            raise ValueError(key + " is out of range or not an integer")
    for action in ("up", "down", "stop"):
        pulses = arguments[action]
        if not isinstance(pulses, list) or not 2 <= len(pulses) <= 512 or len(pulses) % 2:
            raise ValueError(action + " needs 2..512 alternating high/low pulse durations")
        for i, value in enumerate(pulses):
            if type(value) is not int or not 1 <= abs(value) <= 32767 or (value > 0) != (i % 2 == 0):
                raise ValueError("RF pulses must alternate positive/negative signed microseconds")
        if sum(abs(x) for x in pulses) > 150000:
            raise ValueError("Each RF frame must not exceed 150 ms")


class Hardware:
    def __init__(self, port, timeout=8, connection=None):
        self.port = port
        self.timeout = timeout
        self.connection = connection
        self.buffer = bytearray()

    def close(self):
        if self.connection:
            self.connection.close()
            self.connection = None
        self.buffer.clear()

    def _open(self):
        if self.connection is None:
            import serial
            # Disable reset lines before opening. Exclusive access prevents two
            # MCP processes from competing for replies on the same USB device.
            connection = serial.Serial(port=None, baudrate=115200, timeout=0.2,
                                       write_timeout=2, exclusive=True)
            connection.dtr = False
            connection.rts = False
            connection.port = self.port
            connection.open()
            self.connection = connection

    def call(self, method, arguments):
        try:
            self._open()
            request_id = uuid.uuid4().hex
            request = json.dumps({"id": request_id, "method": method, "params": arguments}, separators=(",", ":")).encode() + b"\n"
            if len(request) > 4096:
                raise ValueError("Hardware request exceeded 4096 bytes")
            written = self.connection.write(request)
            if written != len(request):
                raise RuntimeError("USB write incomplete; outcome is unknown, do not retry automatically")
            self.connection.flush()
            duration = arguments.get("seconds")
            extra = duration if method == "curtain.trace_input" and type(duration) is int and 1 <= duration <= 45 else 0
            deadline = time.monotonic() + self.timeout + extra
            while time.monotonic() < deadline:
                chunk = self.connection.read_until(b"\n", 4096)
                self.buffer.extend(chunk)
                while b"\n" in self.buffer:
                    line, _, rest = self.buffer.partition(b"\n")
                    self.buffer = bytearray(rest)
                    if not line.startswith(PREFIX):
                        continue  # ROM/startup messages are not protocol replies.
                    try:
                        result = json.loads(line[len(PREFIX):])
                    except (ValueError, UnicodeError):
                        continue
                    if not isinstance(result, dict) or result.get("id") != request_id:
                        continue
                    if "error" in result:
                        raise RuntimeError(str(result["error"]))
                    if not isinstance(result.get("result"), dict):
                        raise RuntimeError("Invalid hardware reply")
                    return result["result"]
                if len(self.buffer) > 4096:
                    raise RuntimeError("Hardware reply exceeded size limit")
            raise TimeoutError("USB reply timed out; outcome is unknown, query status before retrying")
        except Exception:
            # Never resend a potentially executed hardware command on timeout or
            # disconnect. A later explicit request may reconnect.
            self.close()
            raise


def handle(request, hardware):
    if not isinstance(request, dict) or request.get("jsonrpc") != "2.0":
        return {"jsonrpc": "2.0", "id": None, "error": {"code": -32600, "message": "Invalid JSON-RPC request"}}
    if "id" not in request:
        return None
    response = {"jsonrpc": "2.0", "id": request["id"]}
    method = request.get("method")
    params = request.get("params", {})
    if not isinstance(params, dict):
        response["error"] = {"code": -32602, "message": "params must be an object"}
        return response
    if method == "initialize":
        version = params.get("protocolVersion")
        if version not in ("2024-11-05", "2025-03-26", "2025-06-18"):
            version = "2024-11-05"
        response["result"] = {"protocolVersion": version, "capabilities": {"tools": {}},
                              "serverInfo": {"name": "mr-li-esp32-hardware", "version": "0.1.0"}}
    elif method == "ping":
        response["result"] = {}
    elif method == "tools/list":
        response["result"] = {"tools": TOOLS}
    elif method == "tools/call":
        try:
            name = params.get("name")
            if name not in {tool["name"] for tool in TOOLS}:
                raise ValueError("Unknown hardware tool")
            arguments = params.get("arguments", {})
            if not isinstance(arguments, dict):
                raise ValueError("arguments must be an object")
            target = name.removeprefix("self.")
            if name == "self.hardware.command":
                target = arguments.get("method")
                if not isinstance(target, str) or not re.fullmatch(COMMAND_PATTERN, target):
                    raise ValueError("Expected an ESP32 hardware command")
                if set(arguments) - {"method", "params"} or not isinstance(arguments.get("params", {}), dict):
                    raise ValueError("Expected method and optional params object")
                arguments = arguments.get("params", {})
            elif name == "self.xgimi.configure":
                if set(arguments) != {"token"} or not isinstance(arguments["token"], str) or len(arguments["token"]) not in (30, 32):
                    raise ValueError("configure requires a 30 or 32 character token")
            elif name == "self.curtain.configure":
                validate_curtain(arguments)
            elif name.removeprefix("self.curtain.") in RF_SETTINGS_TOOLS:
                validate_rf_settings(name.removeprefix("self.curtain."), arguments)
            elif arguments:
                raise ValueError("This tool takes no arguments")
            result = hardware.call(target, arguments)
            response["result"] = {"content": [{"type": "text", "text": json.dumps(result, ensure_ascii=False)}],
                                  "structuredContent": result, "isError": False}
        except Exception as error:
            response["result"] = {"content": [{"type": "text", "text": str(error)}], "isError": True}
    else:
        response["error"] = {"code": -32601, "message": "Method not found"}
    return response


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", required=True, help="ESP32 USB serial device, e.g. /dev/cu.usbmodem... or /dev/cu.usbserial...")
    args = parser.parse_args()
    hardware = Hardware(args.port)
    try:
        while True:
            line = sys.stdin.buffer.readline(65537)
            if not line:
                break
            if len(line) > 65536:
                raise ValueError("MCP input exceeded 64 KiB limit")
            try:
                request = json.loads(line)
            except (ValueError, UnicodeError):
                response = {"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": "Parse error"}}
            else:
                response = handle(request, hardware)
            if response is not None:
                print(json.dumps(response, ensure_ascii=False, separators=(",", ":")), flush=True)
    finally:
        hardware.close()


if __name__ == "__main__":
    main()
