#!/usr/bin/env python3
"""Exercise the wizard in a PTY with disposable config; no installs or live services."""
import os
from pathlib import Path
import pty
import select
import shutil
import subprocess
import tempfile
import time

project = Path(__file__).resolve().parent.parent

def drive(root, args, steps):
    master, slave = pty.openpty()
    proc = subprocess.Popen(['/bin/bash', str(root/'scripts/init.sh'), *args],
                            cwd=root, stdin=slave, stdout=slave, stderr=slave)
    os.close(slave)
    transcript = b''
    offset = 0
    try:
        for prompt, answer in steps:
            deadline = time.monotonic()+15
            while prompt.encode() not in transcript[offset:]:
                if time.monotonic()>deadline:
                    raise AssertionError('Wizard did not reach expected prompt: '+prompt)
                readable, _, _ = select.select([master], [], [], .1)
                if readable:
                    try:
                        block = os.read(master, 65536)
                    except OSError:
                        block = b''
                    if not block:
                        raise AssertionError('Wizard exited before prompt: '+prompt+'; output: '+transcript.decode(errors='replace'))
                    transcript += block
            offset = len(transcript)
            time.sleep(.04)  # let read -s disable terminal echo before sending the secret
            os.write(master, answer.encode()+b'\n')
        deadline = time.monotonic()+15
        while proc.poll() is None:
            if time.monotonic()>deadline:
                raise AssertionError('Wizard did not exit')
            if select.select([master], [], [], .1)[0]:
                try:
                    transcript += os.read(master, 65536)
                except OSError:
                    break
        assert proc.wait(timeout=5) == 0, 'Wizard exited unsuccessfully'
        return transcript.decode(errors='replace')
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        os.close(master)

with tempfile.TemporaryDirectory(prefix='mr-li wizard ') as temp:
    root = Path(temp)/'service'
    (root/'scripts').mkdir(parents=True)
    for name in ['init.sh', 'run.sh']:
        shutil.copy2(project/'scripts'/name, root/'scripts'/name)
    shutil.copy2(project/'opencode.example.json', root/'opencode.example.json')
    workspace = Path(temp)/'work space'
    secret = 'test-only-$UNDEFINED "quoted" \\end $(touch SHOULD_NOT_EXIST)'
    transcript = drive(root, ['--configure-only'], [
        ('应用区域：', 'lark'), ('飞书 App ID', ''), ('飞书 App Secret（隐藏输入）', secret),
        ('允许使用的用户 open_id', 'ou_test'), ('限定工作群 chat_id', 'oc_test'), ('OpenCode 本地服务密码', ''),
        ('Agent 任务工作目录', str(workspace)), ('OpenCode 免费模型 provider/model', ''),
        ('OpenCode V2.3.x 启动器绝对路径', '/tmp/test-opencode'),
    ])
    assert secret not in transcript, 'Secret was echoed'
    assert (root/'.env').stat().st_mode & 0o777 == 0o600
    assert (workspace/'opencode.json').is_file()
    assert not (root/'SHOULD_NOT_EXIST').exists()
    content = (root/'.env').read_bytes()
    # Actual Rust/dotenvy configuration parser must accept the escaped secret and spaced path.
    binary = project/'target/release/mr-li'
    if binary.is_file():
        subprocess.run([str(binary), 'check'], cwd=root,
                       env=dict(os.environ, MR_LI_ENV=str(root/'.env')), check=True)
    transcript = drive(root, ['--configure-only'], [('是否重新输入并替换现有 .env', '')])
    assert (root/'.env').read_bytes() == content, 'Existing config was overwritten'
    assert '飞书 App Secret（隐藏输入）' not in transcript
    # A packaged deployment must work without Rust, source, downloads or model login.
    if binary.is_file():
        (root/'bin').mkdir()
        shutil.copy2(binary, root/'bin/mr-li')
        transcript = drive(root, [], [('是否重新输入并替换现有 .env', ''), ('安装守护服务？', 'none')])
        assert '部署机无需安装 Rust' in transcript
        assert (root/'.env').read_bytes() == content
    print('PASS: hidden secret, dotenv parsing, file permissions, preserved config, packaged setup')
