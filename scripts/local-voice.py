#!/usr/bin/env python3
"""Say-compatible offline voice cloning. Reference voice is never substituted."""
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
STATE = ROOT / ".data/local-tts"


def valid_audio(path):
    try:
        with wave.open(str(path)) as w:
            return w.getnframes() > 0 and w.getframerate() > 0
    except (OSError, wave.Error, EOFError):
        return False


def play_cached(output, cache_only):
    if cache_only:
        print(output)
        return
    with (STATE / "playback.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        os.set_inheritable(lock.fileno(), True)
        os.execv("/usr/bin/afplay", ["afplay", str(output)])


def generate(engine, text, output, steps, mode):
    if not engine.is_file():
        raise FileNotFoundError("本地克隆引擎尚未安装")
    # Each synthesis owns its request even when multiple calls miss the same cache entry.
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", suffix=".request",
                                     dir=output.parent, delete=False) as request:
        request.write(text)
    # Keep the same PID through synthesis and playback so /speak stop cancels both.
    os.execv(str(engine), [str(engine), str(STATE), request.name, str(output), str(steps), mode])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("-v", default="")
    parser.add_argument("-a", default="")
    parser.add_argument("-r", default="180")
    parser.add_argument("--cache-only", action="store_true")
    parser.add_argument("--steps", type=int)
    args = parser.parse_args()
    text = sys.stdin.read().strip()
    if not text or len(text) > 500 or "\0" in text:
        raise ValueError("播报文字需为1–500字")
    os.umask(0o077)
    cfg = json.loads((STATE / "config.json").read_text())
    steps = args.steps if args.steps is not None else cfg.get("steps", 8)
    if not 1 <= steps <= 16:
        raise ValueError("本地克隆步数需为1–16")
    reference = STATE / "reference.wav"
    if not valid_audio(reference):
        raise ValueError("本地克隆参考音频无效")
    identity = hashlib.sha256(reference.read_bytes() + (STATE / "reference.txt").read_bytes()
                              + json.dumps([cfg["model_id"], "chunks20-preserve-silence-v2", steps, text], ensure_ascii=False).encode()).hexdigest()
    cache = STATE / "cache"
    cache.mkdir(parents=True, exist_ok=True)
    output = cache / (identity + ".wav")
    engine = STATE / "bin/mr-li-local-voice"
    if engine.with_suffix(".lock-v2").is_file():
        if engine.with_suffix(".pause-v1").is_file():
            previous = output
            output = cache / (identity + ".pause-v1.wav")
            if not valid_audio(output) and valid_audio(previous):
                mode = "compact-cache" if args.cache_only else "compact-play"
                os.execv(str(engine), [str(engine), str(STATE), str(previous), str(output), str(steps), mode])
        if valid_audio(output):
            play_cached(output, args.cache_only)
        else:
            generate(engine, text, output, steps, "cache" if args.cache_only else "play-locked")
        return

    # An older native worker relies on the inherited lock to serialize playback.
    with (STATE / "playback.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if valid_audio(output):
            if args.cache_only:
                print(output)
                return
            os.set_inheritable(lock.fileno(), True)
            os.execv("/usr/bin/afplay", ["afplay", str(output)])
        if not engine.is_file():
            raise FileNotFoundError("本地克隆引擎尚未安装")
        os.set_inheritable(lock.fileno(), True)
        generate(engine, text, output, steps, "cache" if args.cache_only else "play")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"本地克隆播报失败：{error}", file=sys.stderr)
        sys.exit(1)
