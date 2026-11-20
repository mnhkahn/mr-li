#!/usr/bin/env python3
"""Import only xiaoli ASR/TTS settings and their key into private mr-li state."""
import argparse
import json
import os
from pathlib import Path
import shlex
from speech_api import ROOT, private_write


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="xiaoli server directory containing settings.json and .env")
    args = parser.parse_args()
    source = args.source.resolve()
    models = json.loads((source / "settings.json").read_text())["models"]
    values = {}
    for line in (source / ".env").read_text().splitlines():
        key, sep, value = line.removeprefix("export ").partition("=")
        if sep:
            parts = shlex.split(value, comments=True)
            values[key.strip()] = " ".join(parts)
    keys = [os.environ.get(models[k]["api_key_env"]) or values.get(models[k]["api_key_env"], "") for k in ("asr", "tts")]
    if not keys[0] or keys[0] != keys[1]:
        raise ValueError("ASR/TTS需要同一有效API密钥；不会猜测或输出凭证")
    dest = ROOT / ".data/speech"
    dest.mkdir(parents=True, exist_ok=True, mode=0o700)
    path = dest / "config.json"
    existing = json.loads(path.read_text()) if path.exists() else {}
    config = {"api_key_file": "api-key", **{
        k: {"url": models[k]["base_url"], "model": models[k]["model"]} for k in ("asr", "tts")}}
    # A fixed demo voice (e.g. anna) must never replace the user's existing clone.
    if existing.get("tts", {}).get("model") == config["tts"]["model"]:
        for key in ("voice", "reference_sha256"):
            if key in existing["tts"]:
                config["tts"][key] = existing["tts"][key]
    private_write(dest / "api-key", keys[0].encode())
    private_write(path, json.dumps(config, ensure_ascii=False, indent=2).encode())
    print("已导入ASR/TTS模型与私有凭证；音色需用原参考音频注册。")


if __name__ == "__main__":
    main()
