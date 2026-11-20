#!/usr/bin/env python3
"""SiliconFlow ASR/TTS frontend, matching xiaoli's models and HTTP contracts.

Credentials live in .data/speech, never in Agent prompts or command arguments.
The say-compatible entry point synthesizes new text and plays it on this Mac.
"""
import argparse
from array import array
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import uuid
import wave
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_CONFIG = ROOT / ".data/speech/config.json"
MAX_AUDIO = 16 * 1024 * 1024


def private_write(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as out:
        temporary = Path(out.name)
        os.chmod(temporary, 0o600)
        out.write(data)
        out.flush()
        os.fsync(out.fileno())
    temporary.replace(path)


def multipart(fields, audio, filename="speech.wav"):
    boundary = "mr-li-" + uuid.uuid4().hex
    parts = []
    for name, value in fields.items():
        parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode())
    parts += [f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{filename}"\r\nContent-Type: audio/wav\r\n\r\n'.encode(),
              audio, f"\r\n--{boundary}--\r\n".encode()]
    return b"".join(parts), "multipart/form-data; boundary=" + boundary


def valid_wav(data):
    try:
        with wave.open(io.BytesIO(data)) as audio:
            return audio.getnframes() > 0 and audio.getframerate() > 0
    except (wave.Error, EOFError):
        return False


class SpeechAPI:
    def __init__(self, path=DEFAULT_CONFIG):
        self.path = Path(path)
        self.config = json.loads(self.path.read_text())
        key_file = Path(self.config["api_key_file"])
        if not key_file.is_absolute():
            key_file = self.path.parent / key_file
        if key_file.stat().st_mode & 0o077:
            raise ValueError("语音API密钥文件权限必须为600")
        self.key = key_file.read_text().strip()
        if not self.key:
            raise ValueError("语音API密钥为空")
        # Match xiaoli's normal network environment; deployments can explicitly
        # select direct access when their shell contains an obsolete proxy.
        proxy = {} if self.config.get("direct_network", False) else None
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler(proxy))
        for kind in ("asr", "tts"):
            url = urllib.parse.urlsplit(self.config[kind]["url"])
            if ((url.scheme != "https" and not (url.scheme == "http" and url.hostname in ("127.0.0.1", "localhost")))
                    or url.username or url.password or url.query or url.fragment):
                raise ValueError("语音API仅支持HTTPS或本机测试地址，凭证必须独立保存")

    def request(self, kind, url, body=None, content_type="application/json", limit=MAX_AUDIO):
        request = urllib.request.Request(url, data=body, headers={
            "Authorization": "Bearer " + self.key,
            "Content-Type": content_type,
        })
        try:
            with self.http.open(request, timeout=45 if kind == "ASR" else 30) as response:
                data = response.read(limit + 1)
        except urllib.error.HTTPError as error:
            detail = error.read(1500).decode("utf-8", errors="replace").replace(self.key, "[redacted]")
            try:
                payload = json.loads(detail)
                detail = str(payload.get("message", payload.get("error", "")))
            except (ValueError, AttributeError):
                pass
            raise RuntimeError(f"{kind} API HTTP {error.code}: {detail[:400]}") from None
        except (OSError, urllib.error.URLError):
            raise RuntimeError(f"{kind} API 网络连接失败或超时") from None
        if len(data) > limit:
            raise ValueError(f"{kind} API 响应过大")
        return data

    def transcribe(self, audio):
        if not audio or len(audio) > MAX_AUDIO:
            raise ValueError("ASR音频为空或过大")
        cfg = self.config["asr"]
        body, content_type = multipart({"model": cfg["model"], "response_format": "json"}, audio)
        payload = json.loads(self.request("ASR", cfg["url"], body, content_type, limit=65536))
        text = payload.get("text", "").strip()
        if not text:
            raise ValueError("ASR API 没有返回识别文字")
        return text

    def synthesize(self, text):
        cfg = self.config["tts"]
        if not cfg.get("voice"):
            raise ValueError("请先用现有参考音频注册克隆音色")
        text = text.strip()
        if not text or len(text) > 500 or "\0" in text:
            raise ValueError("播报文字需为1–500字")
        body = json.dumps({"model": cfg["model"], "voice": cfg["voice"], "input": text,
                           "response_format": "wav", "sample_rate": 24000, "stream": False}).encode()
        audio = self.request("TTS", cfg["url"], body)
        if not valid_wav(audio):
            raise ValueError("TTS API 没有返回有效WAV音频")
        return audio

    def cache_path(self, text):
        cfg = self.config["tts"]
        identity = json.dumps([cfg["url"], cfg["model"], cfg.get("voice"), "wav", 24000, text], ensure_ascii=False)
        return self.path.parent / "cache" / (hashlib.sha256(identity.encode()).hexdigest() + ".wav")

    def register_voice(self, reference):
        reference = Path(reference)
        audio = reference.read_bytes()
        if not valid_wav(audio):
            raise ValueError("参考音频不是有效的WAV文件")
        checksum = hashlib.sha256(audio).hexdigest()
        cfg = self.config["tts"]
        if cfg.get("reference_sha256") == checksum and cfg.get("voice", "").startswith("speech:"):
            return
        base = cfg["url"].removesuffix("/audio/speech")
        name = "mr-li-" + checksum[:12]
        # Check before upload so rerunning setup cannot create duplicate voices.
        voices = json.loads(self.request("TTS", base + "/audio/voice/list", limit=1024 * 1024))
        voice = next((v.get("uri") for v in voices.get("results", [])
                      if v.get("customName") == name and v.get("model") == cfg["model"]), None)
        if not voice:
            transcript = self.transcribe(audio)
            body, content_type = multipart({"model": cfg["model"], "customName": name, "text": transcript}, audio)
            result = json.loads(self.request("TTS", base + "/uploads/audio/voice", body, content_type, limit=65536))
            voice = result.get("uri")
        if not isinstance(voice, str) or not voice.startswith("speech:"):
            raise ValueError("音色注册没有返回有效URI")
        cfg.update(voice=voice, reference_sha256=checksum)
        private_write(self.path, json.dumps(self.config, ensure_ascii=False, indent=2).encode())


class ApiStream:
    def accept_waveform(self, rate, samples):
        self.rate, self.samples = rate, samples


class ApiRecognizer:
    """VAD-segment adapter; the microphone frontend never owns Agent selection."""
    def __init__(self, api):
        self.api = api

    def create_stream(self):
        return ApiStream()

    def decode_stream(self, stream):
        pcm = array("h", (max(-32768, min(32767, round(float(v) * 32767))) for v in stream.samples))
        if sys.byteorder != "little":
            pcm.byteswap()
        audio = io.BytesIO()
        with wave.open(audio, "wb") as out:
            out.setnchannels(1)
            out.setsampwidth(2)
            out.setframerate(stream.rate)
            out.writeframes(pcm.tobytes())
        stream.result = SimpleNamespace(text=self.api.transcribe(audio.getvalue()))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--transcribe", type=Path)
    parser.add_argument("--register-voice", type=Path)
    parser.add_argument("--cache-only", action="store_true")
    parser.add_argument("-v", default="")
    parser.add_argument("-a", default="")
    parser.add_argument("-r", default="180")
    args = parser.parse_args()
    api = SpeechAPI(args.config)
    if args.transcribe:
        print(api.transcribe(args.transcribe.read_bytes()))
        return
    if args.register_voice:
        api.register_voice(args.register_voice)
        print("克隆音色已配置，沿用现有参考音频。")
        return
    text = sys.stdin.read().strip()
    if not text or len(text) > 500 or "\0" in text:
        raise ValueError("播报文字需为1–500字")
    # Serialize manual speech, scheduled news and posture playback as before.
    lock_path = api.path.parent / "playback.lock"
    lock = lock_path.open("a")
    os.chmod(lock_path, 0o600)
    fcntl.flock(lock, fcntl.LOCK_EX)
    output = api.cache_path(text)
    if not output.exists() or not valid_wav(output.read_bytes()):
        private_write(output, api.synthesize(text))
    if args.cache_only:
        print(output)
        return
    os.set_inheritable(lock.fileno(), True)
    os.execv("/usr/bin/afplay", ["afplay", str(output)])


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"语音服务失败：{error}", file=sys.stderr)
        sys.exit(1)
