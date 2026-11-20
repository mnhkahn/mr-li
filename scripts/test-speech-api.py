#!/usr/bin/env python3
"""Local protocol tests; never contact model providers or play audio."""
import io
import json
from pathlib import Path
import tempfile
import threading
import unittest
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from speech_api import SpeechAPI, ApiRecognizer, private_write, valid_wav


def wav():
    out = io.BytesIO()
    with wave.open(out, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes(b"\0\0" * 160)
    return out.getvalue()


class ProtocolTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.calls = []
        self.fail = False
        case = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_GET(self):
                case.calls.append((self.path, None, self.headers))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{"results":[]}')
            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                case.calls.append((self.path, body, self.headers))
                if case.fail:
                    self.send_response(429)
                    self.end_headers()
                    self.wfile.write(b'{"message":"quota exhausted secret-key"}')
                    return
                self.send_response(200)
                self.end_headers()
                if self.path.endswith("transcriptions"):
                    self.wfile.write(json.dumps({"text":"你好，李老师，看看今天的任务。"}).encode())
                elif self.path.endswith("/uploads/audio/voice"):
                    self.wfile.write(b'{"uri":"speech:mr-li:test"}')
                else:
                    self.wfile.write(wav())

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        url = f"http://127.0.0.1:{self.server.server_port}/v1"
        private_write(self.root / "key", b"secret-key")
        self.path = self.root / "config.json"
        self.config = {"api_key_file":"key", "asr":{"url":url+"/audio/transcriptions","model":"FunAudioLLM/SenseVoiceSmall"},
                       "tts":{"url":url+"/audio/speech","model":"FunAudioLLM/CosyVoice2-0.5B"}}
        private_write(self.path,json.dumps(self.config).encode())
        self.api = SpeechAPI(self.path)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.tmp.cleanup()

    def test_registers_original_reference_once_and_does_not_use_fixed_demo_voice(self):
        reference = self.root / "reference.wav"
        reference.write_bytes(wav())
        with self.assertRaisesRegex(ValueError,"注册克隆音色"):
            self.api.synthesize("你好")
        self.api.register_voice(reference)
        self.assertEqual(len(self.calls),3)
        self.assertIn(b'name="file"',self.calls[1][1])
        self.assertIn(b"FunAudioLLM/SenseVoiceSmall",self.calls[1][1])
        self.assertIn(wav(),self.calls[2][1])
        self.api.register_voice(reference)
        self.assertEqual(len(self.calls),3)
        saved = json.loads(self.path.read_text())
        self.assertEqual(saved["tts"]["voice"],"speech:mr-li:test")
        self.assertNotIn("secret-key",self.path.read_text())

    def test_tts_model_voice_and_cache_identity_are_independent_of_agent(self):
        self.api.config["tts"]["voice"] = "speech:existing-clone"
        first = self.api.cache_path("新文案")
        self.assertTrue(valid_wav(self.api.synthesize("新文案")))
        payload = json.loads(self.calls[0][1])
        self.assertEqual(payload["model"],"FunAudioLLM/CosyVoice2-0.5B")
        self.assertEqual(payload["voice"],"speech:existing-clone")
        self.assertEqual(payload["response_format"],"wav")
        self.assertEqual(payload["input"],"新文案")
        self.assertNotIn("agent",payload)
        self.api.config["tts"]["voice"] = "speech:other"
        self.assertNotEqual(first,self.api.cache_path("新文案"))

    def test_vad_segment_is_sent_as_wav_and_returns_text_only(self):
        recognizer = ApiRecognizer(self.api)
        stream = recognizer.create_stream()
        stream.accept_waveform(16000,[0.0,-1.0,1.0,0.2] * 200)
        recognizer.decode_stream(stream)
        self.assertIn("看看今天的任务",stream.result.text)
        body = self.calls[0][1]
        self.assertIn(b"RIFF",body)
        self.assertIn(b'name="model"',body)
        self.assertEqual(self.calls[0][2]["Authorization"],"Bearer secret-key")

    def test_http_error_is_bounded_redacted_and_never_retried(self):
        self.fail = True
        with self.assertRaisesRegex(RuntimeError,"ASR API HTTP 429") as result:
            self.api.transcribe(wav())
        self.assertNotIn("secret-key",str(result.exception))
        self.assertEqual(len(self.calls),1)

    def test_rejects_public_key_file_and_credential_bearing_url(self):
        (self.root / "key").chmod(0o644)
        with self.assertRaisesRegex(ValueError,"权限"):
            SpeechAPI(self.path)
        (self.root / "key").chmod(0o600)
        self.config["tts"]["url"] = "https://key:secret@example.com/speech"
        private_write(self.path,json.dumps(self.config).encode())
        with self.assertRaisesRegex(ValueError,"凭证"):
            SpeechAPI(self.path)


if __name__ == "__main__":
    unittest.main()
