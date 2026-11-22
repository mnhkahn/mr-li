#!/usr/bin/env python3
"""Local wake/VAD microphone frontend -> ASR -> text JSON lines.

After emitting wake/text, close the input device and wait for {"listen": bool}
on stdin. The Rust frontend resumes capture only after playback has finished.
"""
import argparse
from collections import deque
import io
import json
import os
from pathlib import Path
import re
import socket
import sys
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit
import wave

RATE = 16000
PROTOCOL = None


def isolate_protocol():
    """PortAudio can write native errors to fd 1; keep them out of JSON stdout."""
    global PROTOCOL
    sys.stdout.flush()
    PROTOCOL = os.fdopen(os.dup(sys.stdout.fileno()), "w", encoding="utf-8", buffering=1)
    os.dup2(sys.stderr.fileno(), sys.stdout.fileno())


def prefer_bundled_portaudio():
    # sounddevice otherwise finds /usr/local/lib first. An old 19.6 build there
    # uses Component Manager, rejected by macOS when hosted by modern Python.
    # Keep this scoped to this process; never replace the system-wide library.
    if sys.platform != "darwin":
        return
    import _sounddevice_data
    directory = Path(next(iter(_sounddevice_data.__path__))) / "portaudio-binaries"
    if not (directory / "libportaudio.dylib").is_file():
        raise FileNotFoundError("缺少语音环境自带的PortAudio录音库，请运行setup-voice.sh")
    previous = os.environ.get("DYLD_LIBRARY_PATH", "")
    os.environ["DYLD_LIBRARY_PATH"] = str(directory) + (os.pathsep + previous if previous else "")


def strip_wake(text):
    # Remove only a leading wake phrase, preserving punctuation inside the request.
    return re.sub(r"^\s*[你您][\s，,、]*好[\s，,、]*李[\s，,、]*老[\s，,、]*师[\s，,。.!！?？:：、]*", "", text).strip()


def emit(event, **values):
    if event in ("wake", "text"):
        print(f"VOICE_EVENT {event}", file=sys.stderr, flush=True)
    print(json.dumps(dict(event=event, **values), ensure_ascii=False), file=PROTOCOL, flush=True)


def segment_with_preroll(segment, history, total, window, np):
    """Retain quiet initial syllables which VAD can otherwise cut off."""
    if not isinstance(getattr(segment, "start", None), int):
        return segment.samples
    start_of_history = total - len(history) * window
    start = max(start_of_history, segment.start - int(0.4 * RATE))
    end = min(total, segment.start + len(segment.samples))
    return np.concatenate(tuple(history))[start - start_of_history:end - start_of_history]


def local_recognizer(root, sherpa):
    for name in ("asr/model.int8.onnx", "asr/tokens.txt"):
        if not (root / name).is_file():
            raise FileNotFoundError(f"缺少语音模型：{name}；请先运行setup-voice.sh")
    return sherpa.OfflineRecognizer.from_sense_voice(
        model=str(root / "asr/model.int8.onnx"),
        tokens=str(root / "asr/tokens.txt"),
        num_threads=2, use_itn=True,
    )


def load_models(root, sherpa, recognizer=None):
    required = ["kws/tokens.txt", "kws/encoder.onnx", "kws/decoder.onnx",
                "kws/joiner.onnx", "kws/keywords.txt", "silero_vad.onnx"]
    missing = [name for name in required if not (root / name).is_file()]
    if missing:
        raise FileNotFoundError(f"缺少语音模型：{', '.join(missing)}；请先运行setup-voice.sh")
    kws = root / "kws"
    spotter = sherpa.KeywordSpotter(
        tokens=str(kws / "tokens.txt"),
        encoder=str(kws / "encoder.onnx"),
        decoder=str(kws / "decoder.onnx"),
        joiner=str(kws / "joiner.onnx"),
        keywords_file=str(kws / "keywords.txt"),
        keywords_threshold=0.25,
        num_threads=2, provider="cpu",
    )
    if recognizer is None:
        recognizer = local_recognizer(root, sherpa)
    return spotter, recognizer


def capture(sd, sherpa, np, root, device, spotter, recognizer, listening, notify_ready=False):
    config = sherpa.VadModelConfig()
    config.silero_vad.model = str(root / "silero_vad.onnx")
    config.silero_vad.min_silence_duration = 0.8
    config.silero_vad.min_speech_duration = 0.2
    config.silero_vad.max_speech_duration = 30
    config.sample_rate = RATE
    vad = sherpa.VoiceActivityDetector(config, buffer_size_in_seconds=60)
    window = config.silero_vad.window_size
    stream = spotter.create_stream()
    active = listening
    deadline = time.monotonic() + 15 if active else None
    history = deque(maxlen=int(35 * RATE / window))
    overflows = deque(maxlen=8)
    total = 0
    diagnostics = os.environ.get("MR_LI_VOICE_DIAGNOSTICS") == "1"
    next_diagnostic = time.monotonic() + 5
    peak = 0.0
    # Exceptions (no input hardware, permissions, device unplugged) end the process
    # visibly instead of silently switching to an unrelated device or audio source.
    with sd.InputStream(device=device, channels=1, dtype="float32", samplerate=RATE,
                        blocksize=window, latency=0.5) as mic:
        if notify_ready:
            emit("ready")
        while True:
            samples, overflow = mic.read(window)
            if overflow:
                now = time.monotonic()
                overflows.append(now)
                if len(overflows) == overflows.maxlen and now - overflows[0] < 10:
                    raise RuntimeError("麦克风在10秒内连续丢帧8次，无法完整收音，请检查CPU负载或输入设备")
                # A gap invalidates the partial request: never dispatch truncated words.
                vad.reset()
                stream = spotter.create_stream()
                history.clear()
                total = 0
                active = False
                deadline = None
                print("麦克风短暂丢帧，已恢复等待唤醒，请重新说“你好，李老师”。", file=sys.stderr, flush=True)
                continue
            samples = np.ascontiguousarray(samples.reshape(-1))
            history.append(samples)
            total += window
            vad.accept_waveform(samples)
            if diagnostics:
                peak = max(peak, float(np.max(np.abs(samples))))
                if time.monotonic() >= next_diagnostic:
                    print(f"MIC_SIGNAL peak={peak:.6f} speech={vad.is_speech_detected()}", file=sys.stderr, flush=True)
                    next_diagnostic = time.monotonic() + 5
                    peak = 0.0
            if active and vad.is_speech_detected():
                # The 15-second follow-up window limits waiting, not the request length.
                deadline = time.monotonic() + 35
            if not active:
                stream.accept_waveform(RATE, samples)
                while spotter.is_ready(stream):
                    spotter.decode_stream(stream)
                    if spotter.get_result(stream):
                        active = True
                        deadline = time.monotonic() + 35
                        break
            while not vad.empty():
                audio = segment_with_preroll(vad.front, history, total, window, np)
                vad.pop()
                if not active:
                    continue
                result = recognizer.create_stream()
                result.accept_waveform(RATE, audio)
                recognizer.decode_stream(result)
                text = strip_wake(result.result.text)
                if text:
                    return {"event": "text", "text": text}
                return {"event": "wake"}
            if deadline is not None and time.monotonic() > deadline:
                # No speech in the follow-up window: return to wake-word detection.
                active = False
                deadline = None
                stream = spotter.create_stream()
                print("等待唤醒词：你好，李老师", file=sys.stderr, flush=True)


def esp32_audio(config_path, recognizer, np, check):
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args, **kwargs):
            return None

    config = json.loads(config_path.read_text())
    base = config["base_url"].rstrip("/")
    address = urlsplit(base)
    local_host = address.hostname if address.scheme == "http" and address.hostname and address.hostname.endswith(".local") else None
    resolved_base = None
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def request_device(path, body=None):
        nonlocal resolved_base
        headers = {"Authorization": "Bearer " + config["api_token"]}
        if local_host:
            if resolved_base is None:
                # Resolve mDNS once, rather than waiting on a lookup for every poll.
                ip = socket.gethostbyname(local_host)
                netloc = f"{ip}:{address.port}" if address.port else ip
                resolved_base = address._replace(netloc=netloc).geturl()
            headers["Host"] = address.netloc
        if body is not None:
            headers["Content-Type"] = "application/json"
            body = json.dumps(body).encode()
        request = urllib.request.Request(
            (resolved_base or base) + path, data=body, headers=headers,
        )
        return opener.open(request, timeout=4)

    def status():
        with request_device("/api/voice/status") as response:
            state = json.load(response)
        if state["state"] == "error":
            raise RuntimeError(f"ESP32 语音初始化失败：{state['error']}")
        return state

    state = status()
    if check:
        print(f"PASS: ESP32 唤醒词“{state['wake_word']}”，状态 {state['state']}")
        return
    emit("ready", wake_word=state["wake_word"])
    announced = (state["boot_id"], 0)
    last = announced
    capturing = False
    while True:
        try:
            state = status()
            pending = (state["boot_id"], state["pending_wake"])
            if pending[1] and pending != announced:
                announced = pending
                emit("wake")
                line = sys.stdin.readline(4096)
                if not line:
                    return
                if json.loads(line).get("listen") is True:
                    try:
                        with request_device("/api/voice/listen", {"boot_id": pending[0], "wake": pending[1]}):
                            pass
                        capturing = True
                        continue  # Poll fresh state after acknowledging this wake.
                    except urllib.error.HTTPError as error:
                        if error.code != 409:  # The wake can expire during playback.
                            raise
                capturing = False
                emit("capture_end")
            wake = (state["boot_id"], state["recording_wake"])
            if wake[1] and wake == announced and wake != last:
                with request_device("/api/voice/recording.wav") as response:
                    if response.status != 204 and response.headers.get("X-Mr-Li-Wake") == f"{wake[0]}-{wake[1]}":
                        data = response.read(256_045)
                        if len(data) > 256_044:
                            raise ValueError("ESP32 录音超过长度限制")
                        with wave.open(io.BytesIO(data)) as wav:
                            if (wav.getnchannels(), wav.getsampwidth(), wav.getframerate()) != (1, 2, RATE):
                                raise ValueError("ESP32 录音格式不正确")
                            expected = wav.getnframes()
                            pcm = np.frombuffer(wav.readframes(expected), dtype="<i2")
                        if len(pcm) != expected:
                            raise ValueError("ESP32 录音不完整")
                        last = wake
                        stream = recognizer.create_stream()
                        stream.accept_waveform(RATE, pcm.astype(np.float32) / 32768)
                        recognizer.decode_stream(stream)
                        text = stream.result.text.strip()
                        if any(character.isalnum() for character in text):
                            capturing = False
                            emit("text", text=text)
                            if not sys.stdin.readline(4096):
                                return
            if capturing and state["state"] == "listening" and not state["pending_wake"]:
                capturing = False
                emit("capture_end")
        except urllib.error.HTTPError:
            raise
        except (urllib.error.URLError, TimeoutError, socket.gaierror):
            resolved_base = None
            time.sleep(2)
            continue
        time.sleep(0.4)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--models", type=Path, required=True)
    parser.add_argument("--device", default="")
    parser.add_argument("--esp32-config", type=Path, help="ESP32 局域网连接配置")
    parser.add_argument("--check", action="store_true", help="检查模型和输入设备，不开启录音")
    backend = parser.add_mutually_exclusive_group()
    backend.add_argument("--local-asr", dest="local_asr", action="store_true", default=True, help="使用本地SenseVoice（默认，不消耗在线额度）")
    backend.add_argument("--cloud-asr", dest="local_asr", action="store_false", help="显式使用xiaoli的在线ASR接口")
    args = parser.parse_args()
    if not args.check:
        isolate_protocol()
    import numpy as np
    import sherpa_onnx as sherpa
    if args.esp32_config:
        if not args.local_asr:
            raise ValueError("ESP32 输入使用本地 ASR")
        esp32_audio(args.esp32_config, local_recognizer(args.models, sherpa), np, args.check)
        return
    prefer_bundled_portaudio()
    import sounddevice as sd

    device = int(args.device) if args.device.isdigit() else (args.device or None)
    if args.check:
        print(sd.query_devices())
        sd.check_input_settings(device=device, channels=1, dtype="float32", samplerate=RATE)
    from speech_api import SpeechAPI, ApiRecognizer
    recognizer = None if args.local_asr else ApiRecognizer(SpeechAPI())
    spotter, recognizer = load_models(args.models, sherpa, recognizer)
    if args.check:
        print("PASS: 唤醒、ASR模型及16kHz输入设备检查通过（未录音）")
        return
    listening = False
    ready = False
    while True:
        event = capture(sd, sherpa, np, args.models, device, spotter, recognizer, listening,
                        notify_ready=not ready)
        ready = True
        # capture's with-block has closed the microphone before stdout is written.
        emit(**event)
        line = sys.stdin.readline(4096)
        if not line:
            return
        response = json.loads(line)
        if not isinstance(response.get("listen"), bool):
            raise ValueError("无效的麦克风恢复指令")
        listening = response["listen"]


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
    except Exception as error:
        print(f"语音输入失败：{error}", file=sys.stderr)
        sys.exit(1)
