#!/usr/bin/env python3
"""Read GitHub deployment results and play them through mr-li's local speech."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
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
# deployment job override, private repository.
WORKFLOWS = (
    ('cyeam_admin', 'Cyeam 管理端', 'deploy.yml', 'main', 'deploy', True),
    ('cyeam_web', 'Cyeam 主站', 'fly.yml', 'master', '', True),
    ('homework-gateway', '作业网关', 'deploy.yml', 'main', '', True),
    ('cnote', 'C Note', 'release.yml', '', '', False),
    ('mi-clip', 'Mi Clip', 'release.yml', '', '', False),
    ('cyeam-cli', 'Cyeam 命令行工具', 'release.yml', '', '', False),
    ('homework', '作业平板', 'android-release.yml', '', '', False),
    ('xiaoli', '小李服务', 'fly-deploy.yml', 'main', '', False),
    ('xiaoli', '小李', 'release.yml', '', '', False),
    ('seal', 'Seal', 'android-release.yml', '', '', True),
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
    repo, label, _, branch, deployment_job, _ = target
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


def save(path, state):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(state, ensure_ascii=False) + '\n')
    temporary.replace(path)


def poll(root, directory, state, get, authenticated):
    path = directory / 'state.json'
    pending = []
    failures = False
    for target in WORKFLOWS:
        repo, _, workflow, _, _, private = target
        if private and not authenticated:
            continue
        key = f'{repo}/{workflow}'
        try:
            response = get(f'/repos/mnhkahn/{repo}/actions/workflows/{workflow}/runs?per_page=30')
            runs = [run for run in response['workflow_runs'] if eligible(target, run)]
        except (RuntimeError, KeyError) as error:
            print(f'{key}: {error}', file=sys.stderr)
            failures = True
            continue
        events = [(f'{run["id"]}:{run.get("run_attempt") or 1}', run) for run in runs]
        if key not in state:
            state[key] = [event for event, _ in events]
            save(path, state)
            print(f'{key}: 已建立基线')
            continue
        pending.extend((run['updated_at'], target, event, run) for event, run in events if event not in state[key])

    played = 0
    for _, target, event, run in sorted(pending):
        if played >= 3:
            break
        key = f'{target[0]}/{target[2]}'
        try:
            message = announcement(get, target, run)
            if message:
                played += 1
                subprocess.run([str(root / 'scripts/run.sh'), 'speak', message], check=True, timeout=190,
                               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            state[key] = (state[key] + [event])[-1000:]
            save(path, state)
            print(f'{key}: {event} ' + ('已播报' if message else '已跳过'))
        except (RuntimeError, subprocess.SubprocessError, OSError):
            print(f'{key}: {event} 无法生成或播放，将在下次检查重试', file=sys.stderr)
            failures = True
    return int(failures)


def install(root, directory):
    result = subprocess.run(['/usr/bin/crontab', '-l'], capture_output=True, text=True)
    if result.returncode and 'no crontab for' not in result.stderr:
        raise RuntimeError('无法读取当前 crontab')
    backup = directory / 'crontab.previous'
    if not backup.exists():
        backup.write_text(result.stdout)
    marker = '# MR-LI GITHUB RELEASE BROADCAST'
    lines = [line for line in result.stdout.splitlines() if not line.endswith(marker)]
    (root / 'logs').mkdir(exist_ok=True)
    command = ' '.join(shlex.quote(str(arg)) for arg in
                       (sys.executable, root / 'scripts/github-release-broadcast.py', 'once'))
    log = shlex.quote(str(root / 'logs/github-release-broadcast.log'))
    lines.append(f'*/10 * * * * {command} >> {log} 2>&1 {marker}')
    subprocess.run(['/usr/bin/crontab', '-'], text=True, input='\n'.join(lines) + '\n', check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['install', 'once', 'preview'])
    parser.add_argument('repo', nargs='?')
    parser.add_argument('run_id', type=int, nargs='?')
    args = parser.parse_args()
    os.umask(0o077)
    directory = ROOT / '.data/github-release-broadcast'
    if args.action == 'install':
        directory.mkdir(parents=True, exist_ok=True)
        install(ROOT, directory)
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

    directory.mkdir(parents=True, exist_ok=True)
    with (directory / 'lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 0
        path = directory / 'state.json'
        state = json.loads(path.read_text()) if path.exists() else {}
        return poll(ROOT, directory, state, get, bool(token))


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
