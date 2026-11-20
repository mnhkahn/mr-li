#!/usr/bin/env python3
"""Download pinned offline models; verify SHA-256 before installing each file."""
import hashlib
import argparse
import base64
import json
from pathlib import Path
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parent.parent / ".data/voice/models"
KWS_REPO = "pkufool/icefall-kws-zipformer-zh-en-3M-2025-12-20"
ASR_REPO = "burgess123/sherpa-onnx-SenseVoice-Small-onnx"
# The ASR mirror is byte-identical to csukuangfj's official HF conversion at
# revision 2365baeacb507f821a0c8120fcee3d484dba7a07. Hashes pin both weights and tokens.


def model_url(repo, path):
    query = urllib.parse.urlencode({"Revision": "master", "FilePath": path})
    return f"https://modelscope.cn/api/v1/models/{repo}/repo?{query}"


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest() if hasattr(hashlib, "file_digest") else hash_stream(source)


def hash_stream(source):
    checksum = hashlib.sha256()
    for chunk in iter(lambda: source.read(1024 * 1024), b""):
        checksum.update(chunk)
    return checksum.hexdigest()


def fetch(url, path, sha256=None):
    if path.is_file() and (sha256 is None or digest(path) == sha256):
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    partial = path.with_suffix(path.suffix + ".part")
    print(f"Downloading {path.relative_to(ROOT)}", flush=True)
    checksum = hashlib.sha256()
    with urllib.request.urlopen(url, timeout=120) as source, partial.open("wb") as dest:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
            dest.write(chunk)
    if sha256 and checksum.hexdigest() != sha256:
        raise ValueError(f"模型校验失败：{path.name}，没有安装此文件")
    partial.replace(path)


def install_vad():
    # The unversioned sherpa release asset changed its input/output contract.
    # Pin Silero v5.1 for compatibility with the macOS12 CPU runtime.
    path = ROOT / "silero_vad.onnx"
    checksum = "2623a2953f6ff3d2c1e61740c6cdb7168133479b267dfef114a4a3cc5bdd788f"
    if path.is_file() and digest(path) == checksum:
        return
    url = "https://api.github.com/repos/snakers4/silero-vad/git/blobs/b3e3a900c0d70e67b5e2b90a33ad856ee7947930"
    request = urllib.request.Request(url, headers={"User-Agent":"mr-li-local-voice"})
    with urllib.request.urlopen(request, timeout=60) as source:
        payload = json.loads(source.read(5 * 1024 * 1024))
    data = base64.b64decode(payload["content"], validate=False)
    if hashlib.sha256(data).hexdigest() != checksum:
        raise ValueError("Silero VAD v5.1模型校验失败")
    partial = path.with_suffix(".part")
    partial.write_bytes(data)
    partial.replace(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    backend = parser.add_mutually_exclusive_group()
    backend.add_argument("--local-asr", dest="local_asr", action="store_true", default=True, help="下载离线ASR模型（默认）")
    backend.add_argument("--cloud-asr", dest="local_asr", action="store_false", help="只下载唤醒与VAD模型，在线ASR需单独配置")
    args = parser.parse_args()
    ROOT.mkdir(parents=True, exist_ok=True)
    suffix = "epoch-13-avg-2-chunk-16-left-64"
    files = [
        (f"onnx/encoder-{suffix}.int8.onnx", "encoder.onnx", "408bbd740838c42d5bf6d1c5b80b3c88b616c7860b92d980328b5b068c76ae48"),
        (f"onnx/decoder-{suffix}.onnx", "decoder.onnx", "63a22dd60f40fff082ac3e09afa507f6787da36df76ded2fbe145fa233e22c21"),
        (f"onnx/joiner-{suffix}.int8.onnx", "joiner.onnx", "190d4067b4cc20b72a42a1916e69d92052000fb7051a427ebb1bc72a69207dc1"),
        ("data/lang_phone/tokens.txt", "tokens.txt", "2d3f32311f9b692b964da3c90e830258d3e78e013cb0c992dbfb15cd5a1a71b0"),
    ]
    for source, target, checksum in files:
        fetch(model_url(KWS_REPO, source), ROOT / "kws" / target, checksum)
    keyword = "n ǐ h ǎo l ǐ l ǎo sh ī"
    tokens = {line.split()[0] for line in (ROOT / "kws/tokens.txt").read_text().splitlines() if line.split()}
    if not set(keyword.split()) <= tokens:
        raise ValueError("唤醒词拼音不在模型词表中")
    (ROOT / "kws/keywords.txt").write_text(f"{keyword} @你好李老师\n", encoding="utf-8")
    for name, checksum in ([
        ("model.int8.onnx", "c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51"),
        ("tokens.txt", "f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc"),
    ] if args.local_asr else []):
        fetch(model_url(ASR_REPO, name), ROOT / "asr" / name, checksum)
    install_vad()
    print("语音模型就绪。启动bridge后运行 scripts/run.sh voice-check，再运行 scripts/run.sh voice。")


if __name__ == "__main__":
    main()
