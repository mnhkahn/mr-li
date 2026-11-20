# Camera session timeout fix

`mr-li.patch` applies to official go2rtc v1.9.14, commit
`b5948cfb25404cc5cb37b166ecaa2dca20b11d4b`. It includes the existing
homebridge-xiaomi-go2rtc v0.9.1 Xiaomi-control patch (plugin commit
`66da2a01eb3a31d100c0fea39901b1683aa4c2e8`) so camera controls and protocol
compatibility are retained. go2rtc is MIT licensed; see `LICENSE.go2rtc`.

The additional fix bounds camera authentication to 10 seconds, bounds TCP frame
writes to 3 seconds, serializes concurrent frame writes, and closes the camera
socket whenever its CS2 read worker exits. Authentication previously waited
without a deadline while holding the shared stream source lock; every later
RTSP request then waited on that same lock. Closing FFmpeg did not release it.
An exited read worker also left its underlying camera socket open.

The regressions simulate an accepted authentication request with no reply,
a camera that stops reading commands, and a timed-out reader. An integration
test uses the actual CS2 UDP/TCP handshake and the same shared stream source
for a silent authentication followed by a successful authentication and media
probe. They require
failure to return, readers and sockets to be released, and a fresh authentication
request to succeed in the same process. No automatic process restart is added.

Build on macOS with Go 1.24 or later (validated with Go 1.25.5 for macOS 12 Intel):

```sh
GO_BIN=/usr/local/go/bin/go bash scripts/build-camera-relay.sh
```

Apply `plugin-session-recovery.patch` to homebridge-xiaomi-go2rtc v0.9.1 to
disable its existing whole-relay restart on camera source loss. Unexpected
process exit supervision remains unchanged.

Install the binary at a separate path and set the Homebridge XiaomiGo2rtc
platform's `go2rtcPath` to it. Back up the config and original binary first.
Loading a new executable requires one process replacement during deployment;
runtime timeouts clean up the affected camera session without restarting the
relay. Restore the previous `go2rtcPath` and reload Homebridge to roll back.
