#!/usr/bin/env python3
"""Local wake/VAD microphone frontend -> ASR -> text JSON lines.

After emitting wake/text, close the input device and wait for {"listen": bool}
on stdin. The Rust frontend resumes capture only after playback has finished.
"""
import argparse
from collections import deque
import json
import os
from pathlib import Path
import re
import sys
import time

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


def load_models(root, sherpa, recognizer=None):
    required = ["kws/tokens.txt", "kws/encoder.onnx", "kws/decoder.onnx",
                "kws/joiner.onnx", "kws/keywords.txt", "silero_vad.onnx"]
    if recognizer is None:
        required += ["asr/model.int8.onnx", "asr/tokens.txt"]
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
        recognizer = sherpa.OfflineRecognizer.from_sense_voice(
            model=str(root / "asr/model.int8.onnx"),
            tokens=str(root / "asr/tokens.txt"),
            num_threads=2, use_itn=True,
        )
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--models", type=Path, required=True)
    parser.add_argument("--device", default="")
    parser.add_argument("--check", action="store_true", help="检查模型和输入设备，不开启录音")
    backend = parser.add_mutually_exclusive_group()
    backend.add_argument("--local-asr", dest="local_asr", action="store_true", default=True, help="使用本地SenseVoice（默认，不消耗在线额度）")
    backend.add_argument("--cloud-asr", dest="local_asr", action="store_false", help="显式使用xiaoli的在线ASR接口")
    args = parser.parse_args()
    if not args.check:
        isolate_protocol()
    import numpy as np
    import sherpa_onnx as sherpa
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
