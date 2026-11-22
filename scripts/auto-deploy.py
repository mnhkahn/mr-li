#!/usr/bin/env python3
"""Unattended GitHub/local-package deployment. Uses only the Python standard library."""
import argparse
import fcntl
import hashlib
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
ALLOWED = {'bin', 'scripts', 'resource', 'models', 'README.md', '.env.example', 'opencode.example.json'}
MANIFEST = 'release.json'
START_MESSAGE = '开始升级。'
SKIP_MESSAGE = '程序内容与当前版本相同，本次已跳过升级。'


def run(args, **kwargs):
    return subprocess.run([str(x) for x in args], check=True, **kwargs)


def output(args, **kwargs):
    return subprocess.check_output([str(x) for x in args], text=True, **kwargs).strip()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix('.tmp')
    tmp.write_text(json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2) + '\n')
    os.replace(tmp, path)


def package_files(package):
    files = {}
    for path in sorted(package.rglob('*')):
        relative = path.relative_to(package)
        if relative.as_posix() == MANIFEST:
            continue
        if path.is_symlink() or relative.parts[0] not in ALLOWED:
            raise RuntimeError('Unexpected package entry: ' + str(relative))
        if path.is_file():
            files[relative.as_posix()] = digest(path)
    return files


def manifest(package, source):
    files = package_files(package)
    revision = output(['git', '-C', source, 'rev-parse', 'HEAD'])
    dirty = bool(output(['git', '-C', source, 'status', '--porcelain', '--untracked-files=normal']))
    changes = output(['git', '-C', source, 'log', '-20', '--format=%H%x09%s']).splitlines()
    value = {'schema': 1, 'revision': revision, 'dirty': dirty, 'files': files,
             'version': hashlib.sha256(json.dumps(files, sort_keys=True).encode()).hexdigest(),
             'changes': [dict(zip(('revision', 'subject'), line.split('\t', 1))) for line in changes],
             'release_notes': os.environ.get('MR_LI_RELEASE_NOTES', '').strip()[:300]}
    write_json(package / MANIFEST, value)
    return value


def validate(package):
    value = json.loads((package / MANIFEST).read_text())
    files = package_files(package)
    if value.get('schema') != 1 or value.get('files') != files:
        raise RuntimeError('Package manifest/hash mismatch')
    version = hashlib.sha256(json.dumps(files, sort_keys=True).encode()).hexdigest()
    if (value.get('version') != version or 'bin/mr-li' not in files
            or not re.fullmatch(r'[0-9a-f]{40,64}', value.get('revision', ''))
            or not os.access(package / 'bin/mr-li', os.X_OK)):
        raise RuntimeError('Invalid package version or missing binary')
    return value


def restart(domain):
    for name in ('opencode', 'bridge'):
        run(['/bin/launchctl', 'kickstart', '-k', domain + '/com.cyeam.mr-li.' + name])


def healthy(root, since, data_dir=None):
    # A fresh heartbeat confirms the NEW process started, unlike probe alone.
    heartbeat = (data_dir or root / '.data') / 'heartbeat'
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        if heartbeat.exists() and heartbeat.stat().st_mtime > since:
            try:
                result = subprocess.run([str(root / 'scripts/run.sh'), 'health'], cwd=root, timeout=10,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                if result.returncode == 0:
                    return
            except subprocess.TimeoutExpired:
                pass
        time.sleep(2)
    raise RuntimeError('New service failed health check within 90 seconds')


def replace_file(source, target):
    target.parent.mkdir(parents=True, exist_ok=True)
    # Never copy through a runtime symlink outside the deployment directory.
    if target.is_symlink() or any(p.is_symlink() for p in target.parents):
        raise RuntimeError('Refusing symlink deployment target: ' + str(target))
    temp = target.with_name(target.name + '.deploy-tmp')
    shutil.copy2(source, temp)
    os.replace(temp, target)


def upgrade_message(root, value, previous):
    notes = value.get('release_notes', '')
    subjects = []
    if not notes and value['revision'] != previous.get('revision'):
        source = root / '.data/auto-deploy/source'
        before = previous.get('revision', '')
        if (source / '.git').exists() and re.fullmatch(r'[0-9a-f]{40,64}', before):
            try:
                subjects = output(['git', '-C', source, 'log', '--no-merges', '--format=%s',
                                   before + '..' + value['revision']], timeout=10,
                                  stderr=subprocess.DEVNULL).splitlines()
            except (subprocess.SubprocessError, OSError):
                pass
        if not subjects:
            for change in value.get('changes', []):
                if change.get('revision') == before:
                    break
                subjects.append(change.get('subject', ''))
    if not notes:
        # Commit subjects describe changes without relying on a model or network.
        subjects = [re.sub(r'^[a-z]+(?:\([^)]*\))?!?:\s*', '', s.strip()) for s in subjects]
        subjects = list(dict.fromkeys(s for s in subjects if s))
        notes = '；'.join(s[:70] for s in subjects[:3]) or '更新了本机部署文件'
        if len(subjects) > 3:
            notes += '等更新'
    notes = re.sub(r'[\x00-\x1f\x7f]', ' ', notes)
    return '升级成功。本次更新：' + notes[:240].rstrip('；。 ') + '。'


def speak(root, text, *, timeout=200):
    env = dict(os.environ, MR_LI_ENV=str(root / '.env'))
    result = run([root / 'scripts/run.sh', 'speak', text], cwd=root, env=env, timeout=timeout,
                 stdout=subprocess.PIPE, text=True)
    return result.stdout.strip().startswith('queued ')


def announce_upgrade(root, value, previous, *, skipped=False):
    # Run after recording either a healthy upgrade or an unchanged package.
    # A speaker failure must neither roll back nor cause another deployment.
    record = root / '.data/auto-deploy/last-announcement.json'
    status = {'version': value['version'], 'revision': value['revision'],
              'outcome': 'skipped' if skipped else 'upgraded', 'text': '', 'status': 'started'}
    try:
        status['text'] = SKIP_MESSAGE if skipped else upgrade_message(root, value, previous)
        write_json(record, status)
        queued = speak(root, status['text'])
        status['status'] = 'queued' if queued else 'completed'
        write_json(record, status)
        label = 'Skipped deployment' if skipped else 'Upgrade'
        print(label + ' announcement ' + ('queued' if queued else 'completed'), flush=True)
    except Exception as error:
        status['status'] = 'failed'
        status['error'] = str(error)
        try:
            write_json(record, status)
        except OSError:
            pass
        outcome = 'Deployment skipped' if skipped else 'Upgrade succeeded'
        print(outcome + ', but voice announcement failed: ' + str(error), flush=True)


def deploy(root, package, domain, data_dir=None):
    value = validate(package)
    installed = root / '.data/auto-deploy/installed.json'
    previous = json.loads(installed.read_text()) if installed.exists() else {}
    # Check actual installed files too: a direct/manual copy may have changed them.
    if previous.get('version') == value['version'] and all(
            (root / f).is_file() and digest(root / f) == h for f, h in value['files'].items()):
        # Empty commits can have identical artifacts. Remember the revision too,
        # so the next GitHub tick doesn't build that commit again.
        write_json(installed, value)
        print('Same version; skipped: ' + value['version'][:12], flush=True)
        announce_upgrade(root, value, previous, skipped=True)
        return
    env = dict(os.environ, MR_LI_ENV=str(root / '.env'))
    run([package / 'bin/mr-li', 'check'], cwd=root, env=env, timeout=30)
    with tempfile.TemporaryDirectory(prefix='rollback-', dir=installed.parent) as backup_dir:
        backup = Path(backup_dir)
        touched = []
        try:
            for name in value['files']:
                target = root / name
                existed = target.exists()
                if existed:
                    replace_file(target, backup / name)
                touched.append((name, existed))
                replace_file(package / name, target)
            started = time.time()
            restart(domain)
            healthy(root, started, data_dir)
            write_json(installed, value)
        except BaseException:
            for name, existed in reversed(touched):
                if existed:
                    replace_file(backup / name, root / name)
                else:
                    (root / name).unlink(missing_ok=True)
            restart(domain)
            print('Deployment failed; restored previous files and restarted services.', flush=True)
            raise
    print('Deployed: ' + value['version'][:12] + ' commit ' + value['revision'], flush=True)
    announce_upgrade(root, value, previous)


def remote_package(root, config):
    state = root / '.data/auto-deploy'
    source = state / 'source'
    env = dict(os.environ, GIT_TERMINAL_PROMPT='0')
    branch = config['branch']
    if not source.exists():
        run(['git', 'clone', '--single-branch', '--branch', branch, config['repo'], source], env=env, timeout=300)
    run(['git', '-C', source, 'remote', 'set-url', 'origin', config['repo']])
    run(['git', '-C', source, 'fetch', '--no-tags', 'origin', branch], env=env, timeout=180)
    revision = output(['git', '-C', source, 'rev-parse', 'FETCH_HEAD'])
    installed = state / 'installed.json'
    if installed.exists() and json.loads(installed.read_text()).get('revision') == revision:
        print('GitHub commit unchanged; skipped: ' + revision, flush=True)
        return None
    if installed.exists():
        previous = json.loads(installed.read_text())['revision']
        ancestry = subprocess.run(['git', '-C', str(source), 'merge-base', '--is-ancestor', previous, revision],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if ancestry.returncode:
            print('GitHub is behind/diverged from the local deployment; skipped.', flush=True)
            return None
    # Dedicated checkout; never reset the user's working tree or deployment data.
    run(['git', '-C', source, 'reset', '--hard', revision])
    run(['git', '-C', source, 'clean', '-fd'])
    build_env = dict(os.environ)
    build_env['PATH'] = str(Path.home() / '.cargo/bin') + os.pathsep + os.environ.get('PATH', '')
    if config.get('pose'):
        build_env['MR_LI_BUILD_POSE'] = '1'
    try:
        speak(root, START_MESSAGE, timeout=60)
    except (subprocess.SubprocessError, OSError) as error:
        print('Start announcement failed: ' + str(error), flush=True)
    run(['/bin/bash', source / 'scripts/build-release.sh'], cwd=source, env=build_env)
    platform = output(['uname', '-s']) + '-' + output(['uname', '-m'])
    package = source / 'dist' / ('mr-li-' + platform)
    validate(package)
    return package


def once(root, package=None, *, github_only=False, local_only=False):
    state = root / '.data/auto-deploy'
    state.mkdir(parents=True, exist_ok=True)
    with (state / 'lock').open('a') as lock:
        try:
            mode = fcntl.LOCK_EX if github_only else fcntl.LOCK_EX | fcntl.LOCK_NB
            fcntl.flock(lock, mode)
        except BlockingIOError:
            print('Deployment already running; skipped.', flush=True)
            return
        config = json.loads((state / 'config.json').read_text())
        data_dir = Path(config.get('data_dir', str(root / '.data')))
        incoming = state / 'incoming'
        if package is not None:
            deploy(root, package.resolve(), config['domain'], data_dir)
            return
        # Locally supplied packages take priority; rename incoming only after copying finishes.
        if not github_only and incoming.exists():
            deploy(root, incoming, config['domain'], data_dir)
            shutil.rmtree(incoming)
            return
        if not local_only:
            package = remote_package(root, config)
            if package:
                deploy(root, package, config['domain'], data_dir)


def install(root, args):
    if sys.platform != 'darwin' or os.geteuid() == 0:
        raise RuntimeError('Install as the deployment user on macOS, without sudo')
    domain = args.domain or 'user/' + str(os.getuid())
    for name in ('opencode', 'bridge'):
        run(['/bin/launchctl', 'print', domain + '/com.cyeam.mr-li.' + name], stdout=subprocess.DEVNULL)
    state = root / '.data/auto-deploy'
    state.mkdir(parents=True, exist_ok=True)
    (root / 'logs').mkdir(exist_ok=True)
    write_json(state / 'config.json', {'repo': args.repo, 'branch': args.branch,
                                      'domain': domain, 'pose': args.pose,
                                      'data_dir': str((args.data_dir or root / '.data').resolve())})
    current = subprocess.run(['/usr/bin/crontab', '-l'], capture_output=True, text=True)
    if current.returncode and 'no crontab for' not in current.stderr:
        raise RuntimeError('Cannot read crontab: ' + current.stderr)
    (state / ('crontab-backup-' + str(time.time_ns()))).write_text(current.stdout)
    lines = [line for line in current.stdout.splitlines() if '# MR-LI AUTO-DEPLOY' not in line]
    import shlex
    # Cron treats percent specially even inside shell quotes.
    args_for_check = [sys.executable, root / 'scripts/auto-deploy.py', 'once', '--root', root]
    if args.runner:
        args_for_check.append('--local-only')
    command = ' '.join(shlex.quote(str(x)) for x in args_for_check)
    log = shlex.quote(str(root / 'logs/auto-deploy.log'))
    lines.append(('* * * * * ' + command + ' >> ' + log + ' 2>&1 # MR-LI AUTO-DEPLOY').replace('%', r'\%'))
    run(['/usr/bin/crontab', '-'], input='\n'.join(lines) + '\n', text=True)
    check = 'local package' if args.runner else 'GitHub/local package'
    print('Installed: ' + check + ' check every minute; ' + str(state / 'config.json'))


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['manifest', 'install', 'once', 'status'])
    parser.add_argument('--root', type=Path, default=ROOT)
    parser.add_argument('--package', type=Path)
    parser.add_argument('--source', type=Path, default=ROOT)
    parser.add_argument('--repo', default='https://github.com/mnhkahn/mr-li.git')
    parser.add_argument('--branch', default='main')
    parser.add_argument('--domain', help='launchd domain, e.g. user/501, gui/501, system')
    parser.add_argument('--pose', action='store_true', help='also build optional pose binary')
    parser.add_argument('--data-dir', type=Path, help='actual MR_LI_DATA_DIR, if customized')
    parser.add_argument('--runner', action='store_true', help='install cron for local packages only')
    source_mode = parser.add_mutually_exclusive_group()
    source_mode.add_argument('--github-only', action='store_true', help='wait for the lock and check GitHub only')
    source_mode.add_argument('--local-only', action='store_true', help='check for a local package only')
    args = parser.parse_args()
    root = args.root.resolve()
    if args.action == 'manifest':
        if args.package is None:
            parser.error('manifest requires --package')
        print(manifest(args.package, args.source)['version'])
    elif args.action == 'install':
        install(root, args)
    elif args.action == 'once':
        once(root, args.package, github_only=args.github_only, local_only=args.local_only)
    else:
        state = root / '.data/auto-deploy'
        for name in ('config.json', 'installed.json'):
            print(name + ': ' + ((state / name).read_text() if (state / name).exists() else 'not installed'))


if __name__ == '__main__':
    main()
