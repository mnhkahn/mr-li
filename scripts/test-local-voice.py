#!/usr/bin/env python3
"""Offline worker boundary tests without synthesizing or playing sound."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import wave

spec = importlib.util.spec_from_file_location("local_voice",Path(__file__).with_name("local-voice.py"))
voice = importlib.util.module_from_spec(spec)
spec.loader.exec_module(voice)


class ExecIntercept(BaseException):
    pass


class WorkerTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "bin").mkdir()
        (self.root / "bin/mr-li-local-voice").write_text("fake engine")
        (self.root / "config.json").write_text(json.dumps({"model_id":"zipvoice-test","steps":4}))
        (self.root / "reference.txt").write_text("参考文字")
        with wave.open(str(self.root / "reference.wav"),"wb") as w:
            w.setnchannels(1);w.setsampwidth(2);w.setframerate(24000);w.writeframes(b"\0\0"*100)
        self.state = patch.object(voice,"STATE",self.root)
        self.state.start()

    def tearDown(self):
        self.state.stop()
        self.tmp.cleanup()

    def invoke(self,text,args=()):
        with patch.object(sys,"argv",["local-voice.py",*args]), patch.object(sys,"stdin",io.StringIO(text)):
            voice.main()

    def test_text_stays_literal_in_private_file_and_engine_replaces_parent(self):
        text = '-v ? ; $(touch SHOULD_NOT_EXIST)\n你好，李老师。'
        captured = []
        def execute(program,args):
            captured.append(args)
            self.assertEqual(Path(args[2]).read_text(),text)
            self.assertEqual(Path(args[2]).stat().st_mode & 0o077,0)
            self.assertNotIn(text,args)
            self.assertEqual(args[-1],"cache")
            raise ExecIntercept()
        with patch.object(os,"execv",execute), self.assertRaises(ExecIntercept):
            self.invoke(text,["--cache-only"])
        self.assertEqual(len(captured),1)
        self.assertFalse((self.root / "SHOULD_NOT_EXIST").exists())

    def test_reference_or_generation_steps_invalidate_cached_identity(self):
        paths = []
        def execute(program,args):
            paths.append(args[3]);raise ExecIntercept()
        with patch.object(os,"execv",execute):
            for steps in (2,4):
                with self.assertRaises(ExecIntercept):
                    self.invoke("新文案",["--steps",str(steps)])
            (self.root / "reference.txt").write_text("另一段参考文字")
            with self.assertRaises(ExecIntercept):self.invoke("新文案",["--steps","4"])
        self.assertEqual(len(set(paths)),3)

    def test_invalid_text_and_missing_reference_are_rejected(self):
        for text in ("", "字"*501, "内容\0"):
            with self.assertRaises(ValueError):self.invoke(text)
        (self.root / "reference.wav").unlink()
        with self.assertRaisesRegex(ValueError,"参考音频"):self.invoke("你好")

    def test_cached_voice_does_not_acquire_synthesis_lock(self):
        def seed(program, args):
            with wave.open(args[3], "wb") as output:
                output.setparams((1, 2, 24000, 0, "NONE", "not compressed"))
                output.writeframes(b"\0\0" * 100)
            raise ExecIntercept()

        with patch.object(os, "execv", seed), self.assertRaises(ExecIntercept):
            self.invoke("我在，请说。", ["--cache-only"])

        (self.root / "bin/mr-li-local-voice.lock-v2").touch()
        with (self.root / "synthesis.lock").open("a") as synthesis:
            voice.fcntl.flock(synthesis, voice.fcntl.LOCK_EX)
            original = voice.fcntl.flock

            def check_playback_lock(lock, flags):
                self.assertEqual(Path(lock.name), self.root / "playback.lock")
                return original(lock, flags)

            with patch.object(voice.fcntl, "flock", check_playback_lock), \
                 patch.object(os, "execv", side_effect=ExecIntercept) as playback, \
                 self.assertRaises(ExecIntercept):
                self.invoke("我在，请说。")
            self.assertEqual(playback.call_args.args[0], "/usr/bin/afplay")


if __name__ == "__main__":
    unittest.main()
