#!/usr/bin/env python3
"""No microphone/model downloads: exercise the capture state machine with fake audio."""
import importlib.util
from pathlib import Path
from types import SimpleNamespace as NS
import json
import subprocess
import sys
import unittest
from unittest.mock import patch
import tempfile

spec = importlib.util.spec_from_file_location("voice_input", Path(__file__).with_name("voice-input.py"))
voice = importlib.util.module_from_spec(spec)
spec.loader.exec_module(voice)


class CaptureTest(unittest.TestCase):
    def test_mac_selects_private_bundled_audio_library_before_global_library(self):
        with tempfile.TemporaryDirectory() as temporary:
            package=Path(temporary)
            directory=package/"portaudio-binaries"
            directory.mkdir()
            (directory/"libportaudio.dylib").touch()
            with patch.object(voice.sys,"platform","darwin"), patch.dict(sys.modules,{"_sounddevice_data":NS(__path__=[str(package)])}), patch.dict(voice.os.environ,{"DYLD_LIBRARY_PATH":"/existing"}):
                voice.prefer_bundled_portaudio()
                self.assertEqual(voice.os.environ["DYLD_LIBRARY_PATH"],str(directory)+":/existing")
    def test_native_audio_diagnostics_cannot_corrupt_json_protocol(self):
        fixture = '''import importlib.util, os, sys
spec=importlib.util.spec_from_file_location("voice",sys.argv[1])
voice=importlib.util.module_from_spec(spec);spec.loader.exec_module(voice)
voice.isolate_protocol()
os.write(1,b"||PaMacCore|| native input failure\\n")
print("library diagnostic",flush=True)
voice.emit("ready")
voice.emit("text",text="帮我查一下作业")
'''
        result=subprocess.run([sys.executable,"-c",fixture,str(Path(__file__).with_name("voice-input.py"))],capture_output=True,text=True,check=True)
        self.assertEqual([json.loads(line) for line in result.stdout.splitlines()],
                         [{"event":"ready"},{"event":"text","text":"帮我查一下作业"}])
        self.assertIn("native input failure",result.stderr)
        self.assertIn("library diagnostic",result.stderr)

    def capture(self, *, wake_at=None, segments=(), listening=False, overflow_at=()):
        state = NS(frame=0, open=False, decoded=0, resets=0, options=None)

        class Audio:
            def reshape(self, _):
                return self

        class Mic:
            def __init__(self, **options):
                state.options = options
            def __enter__(self):
                state.open = True
                return self
            def __exit__(self, *_):
                state.open = False
            def read(self, _):
                state.frame += 1
                if state.frame > 20:
                    raise RuntimeError("fixture exhausted")
                return Audio(), state.frame in overflow_at

        class Vad:
            def __init__(self, *_, **__):
                self.pending = []
            def accept_waveform(self, _):
                self.pending += [text for frame, text in segments if frame == state.frame]
            def empty(self):
                return not self.pending
            def is_speech_detected(self):
                return False
            def reset(self):
                self.pending.clear()
                state.resets += 1
            @property
            def front(self):
                return NS(samples=self.pending[0])
            def pop(self):
                self.pending.pop(0)

        class Stream:
            def accept_waveform(self, _, samples):
                self.result = NS(text=samples)

        class Recognizer:
            def create_stream(self):
                return Stream()
            def decode_stream(self, _):
                state.decoded += 1

        class Spotter:
            def create_stream(self):
                return Stream()
            def is_ready(self, _):
                return state.frame == wake_at
            def decode_stream(self, _):
                pass
            def get_result(self, _):
                return "你好李老师"

        sherpa = NS(
            VadModelConfig=lambda: NS(silero_vad=NS(window_size=512)),
            VoiceActivityDetector=Vad,
        )
        event = voice.capture(NS(InputStream=Mic), sherpa, NS(ascontiguousarray=lambda x: x),
                              Path("/unused"), None, Spotter(), Recognizer(), listening)
        self.assertFalse(state.open, "mic must be closed before emitting a playback event")
        return event, state

    def test_ignores_unwoken_speech_and_preserves_command_after_wake(self):
        event, state = self.capture(wake_at=4, segments=[(2, "旁人的话"), (6, "你好，李老师，帮我看看今天的任务。")])
        self.assertEqual(event, {"event": "text", "text": "帮我看看今天的任务。"})
        self.assertEqual(state.decoded, 1)

    def test_wake_only_requests_acknowledgement(self):
        event, _ = self.capture(wake_at=2, segments=[(3, "你好，李老师。")])
        self.assertEqual(event, {"event": "wake"})

    def test_followup_needs_no_second_wake(self):
        event, _ = self.capture(listening=True, segments=[(3, "那明天呢？")])
        self.assertEqual(event, {"event": "text", "text": "那明天呢？"})

    def test_transient_overflow_discards_partial_request_and_listens_again(self):
        event, state = self.capture(listening=True, wake_at=6, overflow_at=(3,),
                                    segments=[(3,"不要执行这段残缺的话"),(8,"你好李老师，查一下作业")])
        self.assertEqual(event,{"event":"text","text":"查一下作业"})
        self.assertEqual(state.resets,1)
        self.assertEqual(state.decoded,1)
        self.assertEqual(state.options["latency"],0.5)

    def test_sustained_overflow_fails_instead_of_dispatching_partial_speech(self):
        with self.assertRaisesRegex(RuntimeError,"连续丢帧8次"):
            self.capture(listening=True,overflow_at=tuple(range(1,9)))

    def test_strips_only_leading_wake_phrase(self):
        self.assertEqual(voice.strip_wake("你好，李老师！打开灯，然后播报天气。"), "打开灯，然后播报天气。")
        self.assertEqual(voice.strip_wake("请说你好李老师"), "请说你好李老师")

    def test_vad_segments_keep_quiet_initial_audio_without_unbounded_history(self):
        frames = [list(range(0,4000)),list(range(4000,8000)),list(range(8000,12000))]
        np = NS(concatenate=lambda chunks:[v for chunk in chunks for v in chunk])
        segment = NS(start=8000,samples=[0]*3000)
        actual = voice.segment_with_preroll(segment,frames,12000,4000,np)
        self.assertEqual(actual,list(range(1600,11000)))
        # A long-running listener may have dropped old frames; never use negative offsets.
        actual = voice.segment_with_preroll(NS(start=4000,samples=[0]*6000),frames[1:],12000,4000,np)
        self.assertEqual(actual,list(range(4000,10000)))


if __name__ == "__main__":
    unittest.main()
