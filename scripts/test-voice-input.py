#!/usr/bin/env python3
"""No microphone/model downloads: exercise the capture state machine with fake audio."""
import importlib.util
from pathlib import Path
from types import SimpleNamespace as NS
import unittest

spec = importlib.util.spec_from_file_location("voice_input", Path(__file__).with_name("voice-input.py"))
voice = importlib.util.module_from_spec(spec)
spec.loader.exec_module(voice)


class CaptureTest(unittest.TestCase):
    def capture(self, *, wake_at=None, segments=(), listening=False):
        state = NS(frame=0, open=False, decoded=0)

        class Audio:
            def reshape(self, _):
                return self

        class Mic:
            def __init__(self, **_):
                pass
            def __enter__(self):
                state.open = True
                return self
            def __exit__(self, *_):
                state.open = False
            def read(self, _):
                state.frame += 1
                if state.frame > 20:
                    raise RuntimeError("fixture exhausted")
                return Audio(), False

        class Vad:
            def __init__(self, *_, **__):
                self.pending = []
            def accept_waveform(self, _):
                self.pending += [text for frame, text in segments if frame == state.frame]
            def empty(self):
                return not self.pending
            def is_speech_detected(self):
                return False
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
