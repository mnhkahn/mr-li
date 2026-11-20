#!/usr/bin/env python3
"""Say-compatible Qwen voice cloning worker; text is stdin and cache is private."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import wave

ROOT = Path(__file__).resolve().parent.parent
STATE = ROOT / '.data/voice-clone'


def cache_path(text, reference, model):
    digest = hashlib.sha256(reference.read_bytes() + model.encode() + b'v1' + text.encode()).hexdigest()
    return STATE / 'cache' / (digest + '.wav')


def valid_audio(path):
    try:
        with wave.open(str(path), 'rb') as audio:
            return audio.getnframes() > 0 and audio.getframerate() > 0
    except (OSError, wave.Error, EOFError):
        return False


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('-v', default='')
    parser.add_argument('-a', default='')
    parser.add_argument('-r', default='180')
    parser.add_argument('--cache-only', action='store_true')
    args = parser.parse_args()
    text = sys.stdin.read().strip()
    if not text or len(text) > 500 or '\0' in text:
        raise RuntimeError('播报文字需为1–500字')
    os.umask(0o077)
    cfg = json.loads((STATE / 'config.json').read_text())
    reference = STATE / 'reference.wav'
    output = cache_path(text, reference, cfg['model_size'])
    output.parent.mkdir(parents=True, exist_ok=True)
    # One player for manual speech, news and posture. exec preserves cancellation.
    lock = (STATE / 'playback.lock').open('a')
    fcntl.flock(lock, fcntl.LOCK_EX)
    if not valid_audio(output):
        # The deployment shell may contain an obsolete proxy. This worker uses direct access.
        for key in ('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy'):
            os.environ.pop(key, None)
        os.environ['NO_PROXY'] = os.environ['no_proxy'] = '*'
        from gradio_client import Client, handle_file
        from huggingface_hub import get_token
        client = Client(cfg['endpoint'], verbose=False, token=get_token())
        job = client.submit(handle_file(str(reference)), '', text, 'Chinese', True,
                            cfg['model_size'], api_name='/generate_voice_clone')
        try:
            result = job.result(timeout=120)
        except Exception:
            job.cancel()
            raise
        if not result or not result[0] or not valid_audio(Path(result[0])):
            raise RuntimeError('音色克隆未返回有效声音')
        # Atomic cache writes; never play a partial file.
        with tempfile.NamedTemporaryFile(dir=output.parent, suffix='.wav', delete=False) as temp:
            temp.write(Path(result[0]).read_bytes())
            temp.flush()
            os.fsync(temp.fileno())
            temporary = temp.name
        os.replace(temporary, str(output))
    if args.cache_only:
        print(output)
        return
    # afplay uses the Mac mini system output device (currently Built-in Output).
    os.set_inheritable(lock.fileno(), True)
    os.execv('/usr/bin/afplay', ['afplay', str(output)])


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print('克隆音色播报失败：' + str(error), file=sys.stderr)
        sys.exit(1)
