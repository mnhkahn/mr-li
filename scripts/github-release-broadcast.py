#!/usr/bin/env python3
"""Receive GitHub deployment webhooks and play them through mr-li speech."""
import argparse
from contextlib import closing
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import plistlib
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import threading
import time
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parent.parent
VERSION = re.compile(r'v?\d+\.\d+(?:\.\d+)*(?:[-+][\w.+-]+)?', re.ASCII)
OUTCOMES = {
    'success': '成功', 'failure': '失败', 'timed_out': '超时',
    'cancelled': '已取消', 'startup_failure': '启动失败',
    'action_required': '需要人工处理', 'stale': '已失效',
}
# Repository, spoken name, workflow file, production branch (blank for tags),
# deployment job override.
WORKFLOWS = (
    ('cyeam_admin', 'Cyeam 管理端', 'deploy.yml', 'main', 'deploy'),
    ('cyeam_web', 'Cyeam 主站', 'fly.yml', 'master', ''),
    ('homework-gateway', '作业网关', 'deploy.yml', 'main', ''),
    ('cnote', 'C Note', 'release.yml', '', ''),
    ('mi-clip', 'Mi Clip', 'release.yml', '', ''),
    ('cyeam-cli', 'Cyeam 命令行工具', 'release.yml', '', ''),
    ('homework', '作业平板', 'android-release.yml', '', ''),
    ('xiaoli', '小李服务', 'fly-deploy.yml', 'main', ''),
    ('xiaoli', '小李', 'release.yml', '', ''),
    ('seal', 'Seal', 'android-release.yml', '', ''),
)
LOG_REASONS = (
    (r'undefined symbols for architecture', '链接失败，构建目标存在未解析的符号'),
    (r'failed to resolve:|could not resolve dependencies|eresolve', '依赖解析失败'),
    (r'no space left on device', '构建机磁盘空间不足'),
    (r'out of memory|javascript heap out of memory', '构建进程内存不足'),
    (r'缺少 VERCEL_TOKEN|missing (?:the )?vercel_token', '缺少 Vercel 部署凭据'),
    (r'permission denied|authentication failed|bad credentials', '权限或认证失败'),
    (r'test result: FAILED|[1-9]\d* tests? failed', '自动化测试失败'),
    (r'linker command failed|linking with .* failed', '编译产物链接失败'),
    (r'could not compile|compilation failed', '编译失败'),
)


def github(token, path, *, optional=False, log=False):
    url = 'https://api.github.com' + path
    request = Request(url, headers={'Accept': 'application/vnd.github+json', 'User-Agent': 'mr-li-release-broadcast'})
    if token:
        request.add_unredirected_header('Authorization', 'Bearer ' + token)
    try:
        with urlopen(request, timeout=12) as response:
            if log:
                return response.read(4 * 1024 * 1024).decode('utf-8', 'replace')
            return json.load(response)
    except HTTPError as error:
        if optional and error.code == 404:
            return None
        raise RuntimeError(f'GitHub HTTP {error.code}') from None
    except (OSError, URLError, ValueError):
        raise RuntimeError('无法读取 GitHub 响应') from None


def eligible(target, run):
    ref = run.get('head_branch') or ''
    branch = target[3]
    return (run.get('path', '').split('@')[0] == '.github/workflows/' + target[2]
            and run.get('event') in {'push', 'workflow_dispatch', 'release'}
            and (ref == branch if branch else VERSION.fullmatch(ref))
            and run.get('status') == 'completed' and run.get('conclusion') in OUTCOMES)


def notes(summary):
    lines = []
    for line in summary.splitlines():
        line = re.sub(r'\[([^\]]+)\]\([^)]*\)', r'\1', line)
        line = re.sub(r'https?://\S+', '', line)
        line = re.sub(r'^\s*(?:[-*#>]+|\d+[.)])\s*', '', line)
        line = re.sub(r'`|\*\*|__|[\x00-\x1f]', '', line).strip()
        if line and not re.match(r'^(?:what.s changed|full changelog|new contributors|更新内容|完整变更)\s*[:：]?$', line, re.I):
            if not line.lower().startswith('full changelog:'):
                lines.append(line)
    unique = list(dict.fromkeys(line.rstrip('。.!！ ') for line in lines))
    summary = '。'.join(unique)
    if len(summary) <= 90:
        return summary
    clipped = summary[:90].rstrip('。 ')
    return clipped.rsplit('。', 1)[0] if '。' in clipped else clipped


def failure_detail(get, base, jobs):
    job = next((job for job in jobs if job['conclusion'] in {'failure', 'timed_out'}), None)
    if not job:
        return 'GitHub 未提供失败步骤'
    step = next((step for step in job.get('steps', []) if step['conclusion'] in {'failure', 'timed_out'}), None)
    detail = f'任务 {job["name"][:48]}'
    if step:
        detail += f'，步骤 {step["name"][:48]}'
    try:
        log = get(f'{base}/actions/jobs/{job["id"]}/logs', log=True)
    except RuntimeError:
        log = ''
    for pattern, reason in LOG_REASONS:
        if re.search(pattern, log, re.I):
            return f'{detail}。{reason}'
    exit_codes = re.findall(r'Process completed with exit code (\d{1,4})\b', log)
    if not exit_codes:
        try:
            annotations = get(f'{base}/check-runs/{job["id"]}/annotations')
        except RuntimeError:
            annotations = []
        for annotation in annotations:
            if annotation.get('annotation_level') == 'failure':
                exit_codes.extend(re.findall(r'Process completed with exit code (\d{1,4})\b', annotation.get('message', '')))
    return f'{detail}，退出码 {exit_codes[-1]}' if exit_codes else detail


def announcement(get, target, run):
    repo, label, _, branch, deployment_job = target
    base = f'/repos/mnhkahn/{repo}'
    attempt = f'/attempts/{run["run_attempt"]}' if run.get('run_attempt') else ''
    jobs_path = f'{base}/actions/runs/{run["id"]}{attempt}/jobs?per_page=100'
    conclusion = run['conclusion']
    jobs = get(jobs_path)['jobs'] if deployment_job or conclusion in {'failure', 'timed_out'} else []
    if deployment_job:
        jobs = [job for job in jobs if job['name'] == deployment_job]
        if not jobs and conclusion != 'success':
            return f'{label}发布{OUTCOMES[conclusion]}。部署任务未启动。'
        if not jobs:
            raise RuntimeError('GitHub 未返回生产部署任务')
        conclusion = jobs[0]['conclusion']
        if conclusion not in OUTCOMES:
            return None
    ref = run['head_branch']
    version = f'提交 {run["head_sha"][:7]}' if branch else f'版本 {ref}'
    if conclusion == 'success':
        release = None if branch else get(f'{base}/releases/tags/{quote(ref, safe="")}', optional=True)
        change = (notes((release or {}).get('body') or '')
                  or notes((run.get('head_commit') or {}).get('message') or '')
                  or notes(run.get('display_title') or ''))
        return f'{label}发布成功。{version}。' + (f'更新内容，{change}。' if change else 'GitHub 未提供更新说明。')
    if conclusion in {'failure', 'timed_out', 'startup_failure'}:
        reason = failure_detail(get, base, jobs)
    else:
        reason = 'GitHub 工作流已结束'
    return f'{label}发布{OUTCOMES[conclusion]}。{version}。{reason}。'


def speak(root, message):
    # The Intel deployment can take more than the CLI's 180-second limit to
    # synthesize and play a long uncached utterance.
    remaining = message.strip()
    queued = False
    while remaining:
        end = len(remaining)
        if end > 60:
            boundary = max(remaining.rfind(mark, 25, 60) for mark in ('。', '，', '、', ' '))
            end = boundary + 1 if boundary >= 0 else 60
        result = subprocess.run([str(root / 'scripts/run.sh'), 'speak', remaining[:end]], check=True,
                                timeout=190, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                text=True)
        queued |= result.stdout.strip().startswith('queued ')
        remaining = remaining[end:].lstrip()
    return queued


def workflow_target(repo, run):
    return next((target for target in WORKFLOWS if repo == target[0] and eligible(target, run)), None)


def serve(root, directory, port, secret, get):
    db_path = directory / 'webhooks.sqlite3'
    with closing(sqlite3.connect(db_path)) as db, db:
        db.execute("""
            CREATE TABLE IF NOT EXISTS deliveries (
                repository TEXT NOT NULL, run_id INTEGER NOT NULL, attempt INTEGER NOT NULL,
                payload TEXT, failures INTEGER NOT NULL DEFAULT 0, next_attempt INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (repository, run_id, attempt))
        """)
    state_path = directory / 'state.json'
    previously_spoken = json.loads(state_path.read_text()) if state_path.exists() else {}
    wake = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(6)

        def log_message(self, *args):
            pass

        def reply(self, code):
            self.send_response(code)
            self.send_header('Content-Length', '0')
            self.end_headers()

        def do_GET(self):
            self.reply(204 if self.path == '/healthz' else 404)

        def do_POST(self):
            if self.path != '/github':
                self.reply(404)
                return
            try:
                length = int(self.headers.get('Content-Length') or '')
            except ValueError:
                self.reply(400)
                return
            if not 0 < length <= 1048576:
                self.reply(413)
                return
            try:
                body = self.rfile.read(length)
            except OSError:
                self.reply(408)
                return
            signature = 'sha256=' + hmac.new(secret, body, hashlib.sha256).hexdigest()
            provided = self.headers.get('X-Hub-Signature-256', '')
            if not provided.isascii() or not hmac.compare_digest(signature, provided):
                self.reply(401)
                return
            if self.headers.get('X-GitHub-Event') != 'workflow_run':
                self.reply(204)
                return
            try:
                event = json.loads(body)
                if event['action'] != 'completed':
                    self.reply(204)
                    return
                owner, repo = event['repository']['full_name'].split('/')
                run = event['workflow_run']
                target = workflow_target(repo, run) if owner == 'mnhkahn' else None
                if not target:
                    self.reply(204)
                    return
                run_id, attempt = run['id'], run.get('run_attempt') or 1
                if not isinstance(run_id, int) or not isinstance(attempt, int):
                    raise ValueError('invalid run id')
            except (AttributeError, KeyError, TypeError, ValueError):
                self.reply(400)
                return
            if f'{run_id}:{attempt}' in previously_spoken.get(f'{repo}/{target[2]}', []):
                self.reply(204)
                return
            try:
                with closing(sqlite3.connect(db_path)) as db, db:
                    db.execute('INSERT OR IGNORE INTO deliveries(repository, run_id, attempt, payload) VALUES (?, ?, ?, ?)',
                               (repo, run_id, attempt, json.dumps(run)))
            except (sqlite3.Error, OverflowError):
                self.reply(503)
                return
            wake.set()
            self.reply(202)

    def process():
        with closing(sqlite3.connect(db_path)) as db:
            while True:
                wake.clear()
                row = db.execute(
                    """SELECT repository, run_id, attempt, payload, failures FROM deliveries
                       WHERE payload IS NOT NULL AND next_attempt <= ? ORDER BY rowid LIMIT 1""",
                    (int(time.time()),)).fetchone()
                if row is None:
                    next_attempt, = db.execute('SELECT MIN(next_attempt) FROM deliveries WHERE payload IS NOT NULL').fetchone()
                    wake.wait(max(0, next_attempt - time.time()) if next_attempt is not None else None)
                    continue
                repo, run_id, attempt, payload, failures = row
                try:
                    run = json.loads(payload)
                    target = workflow_target(repo, run)
                    message = announcement(get, target, run) if target else None
                    queued = speak(root, message) if message else False
                    with db:
                        db.execute('UPDATE deliveries SET payload = NULL WHERE repository = ? AND run_id = ? AND attempt = ?',
                                   (repo, run_id, attempt))
                    status = '已存入收件箱' if queued else ('已播报' if message else '已跳过')
                    print(f'{repo}: {run_id}:{attempt} {status}', flush=True)
                except (KeyError, RuntimeError, OSError, ValueError, subprocess.SubprocessError):
                    delay = min(900, 30 * (2 ** min(failures, 5)))
                    with db:
                        db.execute("""UPDATE deliveries SET failures = ?, next_attempt = ?
                                      WHERE repository = ? AND run_id = ? AND attempt = ?""",
                                   (failures + 1, int(time.time()) + delay, repo, run_id, attempt))
                    print(f'{repo}: {run_id}:{attempt} 播报失败，{delay} 秒后重试', file=sys.stderr, flush=True)

    with ThreadingHTTPServer(('127.0.0.1', port), Handler) as server:
        threading.Thread(target=process, daemon=True).start()
        print(f'GitHub Webhook listening on 127.0.0.1:{port}', flush=True)
        server.serve_forever()


def install(root, directory, port):
    directory.mkdir(parents=True, exist_ok=True)
    if not (directory / 'webhook-secret').is_file():
        raise RuntimeError('请先将 GitHub Webhook 密钥保存到 .data/github-release-broadcast/webhook-secret')
    logs = root / 'logs'
    logs.mkdir(exist_ok=True)
    worker = directory / 'worker.py'
    temporary = worker.with_suffix('.tmp')
    shutil.copyfile(__file__, temporary)
    temporary.chmod(0o600)
    temporary.replace(worker)
    label = 'com.cyeam.github-release-broadcast'
    domain = f'user/{os.getuid()}'
    service = directory / f'{label}.plist'
    service.write_bytes(plistlib.dumps({
        'Label': label, 'LimitLoadToSessionType': 'Background',
        'ProgramArguments': [sys.executable, str(worker), 'serve', '--root', str(root), '--port', str(port)],
        'WorkingDirectory': str(root), 'RunAtLoad': True, 'KeepAlive': True, 'ThrottleInterval': 10,
        'StandardOutPath': str(logs / 'github-release-broadcast.log'),
        'StandardErrorPath': str(logs / 'github-release-broadcast.log'),
    }))
    launchctl = '/bin/launchctl'
    subprocess.run([launchctl, 'bootout', f'{domain}/{label}'], capture_output=True)
    subprocess.run([launchctl, 'bootstrap', domain, str(service)], check=True)
    crontab = '/usr/bin/crontab'
    result = subprocess.run([crontab, '-l'], capture_output=True, text=True)
    if result.returncode and 'no crontab for' not in result.stderr:
        raise RuntimeError('无法读取当前 crontab')
    marker = '# MR-LI GITHUB RELEASE BROADCAST'
    lines = [line for line in result.stdout.splitlines() if not line.endswith(marker)]
    lines.append(f'@reboot {launchctl} bootstrap {domain} {shlex.quote(str(service))} {marker}')
    subprocess.run([crontab, '-'], text=True, input='\n'.join(lines) + '\n', check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['install', 'serve', 'preview'])
    parser.add_argument('repo', nargs='?')
    parser.add_argument('run_id', type=int, nargs='?')
    parser.add_argument('--root', type=Path, default=ROOT)
    parser.add_argument('--port', type=int, default=17847)
    args = parser.parse_args()
    os.umask(0o077)
    root = args.root.resolve()
    directory = root / '.data/github-release-broadcast'
    if args.action == 'install':
        install(root, directory, args.port)
        return 0
    token_file = directory / 'token'
    token = os.environ.get('GH_TOKEN') or (token_file.read_text().strip() if token_file.is_file() else '')
    def get(path, **kwargs):
        return github(token, path, **kwargs)
    if args.action == 'preview':
        targets = [target for target in WORKFLOWS if target[0] == args.repo]
        if not targets or not args.run_id or args.run_id < 0:
            parser.error('preview 需要已配置的仓库和 GitHub Actions run ID')
        run = get(f'/repos/mnhkahn/{args.repo}/actions/runs/{args.run_id}')
        target = next((target for target in targets if eligible(target, run)), None)
        if not target:
            parser.error('该运行不属于已监控的发布流程，或者尚未结束')
        print(announcement(get, target, run) or '已跳过该发布')
        return 0

    secret = (directory / 'webhook-secret').read_bytes().strip()
    if not secret:
        raise RuntimeError('GitHub Webhook 密钥为空')
    return serve(root, directory, args.port, secret, get)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
