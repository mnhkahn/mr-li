#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('auto_deploy', Path(__file__).with_name('auto-deploy.py'))
auto = importlib.util.module_from_spec(spec)
spec.loader.exec_module(auto)


class DeploymentTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.root = self.base / 'runtime'
        self.root.mkdir()
        self.state = self.root / '.data/auto-deploy'
        self.state.mkdir(parents=True)
        self.package = self.base / 'package'
        (self.package / 'bin').mkdir(parents=True)
        (self.package / 'bin/mr-li').write_text('#!/bin/sh\nexit 0\n')
        (self.package / 'bin/mr-li').chmod(0o755)
        self.source = self.base / 'source'
        self.source.mkdir()
        self.git('init', '-q')
        self.git('config', 'user.email', 'test@example.invalid')
        self.git('config', 'user.name', 'Test')
        (self.source / 'file').write_text('one')
        self.git('add', '.')
        self.git('commit', '-qm', 'initial')
        self.value = auto.manifest(self.package, self.source)
        self.old_binary = self.root / 'bin/mr-li'
        self.old_binary.parent.mkdir()
        self.old_binary.write_text('old executable')
        (self.root / '.env').write_text('secret remains')
        (self.root / 'scripts').mkdir()
        speaker = self.root / 'scripts/run.sh'
        speaker.write_text('#!/bin/sh\nprintf "%s\\n" "$2" >> .data/utterances\n')
        speaker.chmod(0o755)
        self.old_state = {'version': 'old', 'revision': self.value['revision']}
        auto.write_json(self.state / 'installed.json', self.old_state)

    def git(self, *args):
        return auto.output(['git', '-C', self.source, *args])

    def test_success_and_repeat_skips_restart(self):
        with patch.object(auto, 'restart') as restart, patch.object(auto, 'healthy'):
            auto.deploy(self.root, self.package, 'test/501')
            self.assertEqual(restart.call_count, 1)
            self.assertEqual(json.loads((self.state / 'installed.json').read_text()), self.value)
            auto.deploy(self.root, self.package, 'test/501')
            self.assertEqual(restart.call_count, 1)
        self.assertEqual((self.root / '.env').read_text(), 'secret remains')
        utterances = (self.root / '.data/utterances').read_text().splitlines()
        self.assertEqual(len(utterances), 2)
        self.assertEqual(utterances[-1], auto.SKIP_MESSAGE)

    def test_same_content_new_revision_updates_record_without_restart(self):
        with patch.object(auto, 'restart') as restart, patch.object(auto, 'healthy'):
            auto.deploy(self.root, self.package, 'test/501')
            self.git('commit', '--allow-empty', '-qm', 'empty release')
            updated = auto.manifest(self.package, self.source)
            self.assertEqual(updated['version'], self.value['version'])
            auto.deploy(self.root, self.package, 'test/501')
            self.assertEqual(restart.call_count, 1)
        self.assertEqual(json.loads((self.state / 'installed.json').read_text())['revision'], updated['revision'])
        announcement = json.loads((self.state / 'last-announcement.json').read_text())
        self.assertEqual(announcement['outcome'], 'skipped')
        self.assertEqual(announcement['revision'], updated['revision'])
        self.assertEqual(announcement['text'], auto.SKIP_MESSAGE)
        self.assertNotIn('升级成功', announcement['text'])

    def test_failed_health_rolls_back_files_and_state(self):
        (self.package / 'resource').mkdir()
        (self.package / 'resource/new.txt').write_text('new')
        auto.manifest(self.package, self.source)
        with patch.object(auto, 'restart') as restart, patch.object(auto, 'healthy', side_effect=RuntimeError('unhealthy')):
            with self.assertRaisesRegex(RuntimeError, 'unhealthy'):
                auto.deploy(self.root, self.package, 'test/501')
            self.assertEqual(restart.call_count, 2)
        self.assertEqual(self.old_binary.read_text(), 'old executable')
        self.assertFalse((self.root / 'resource/new.txt').exists())
        self.assertEqual(json.loads((self.state / 'installed.json').read_text()), self.old_state)
        self.assertFalse((self.root / '.data/utterances').exists())

    def test_tamper_rejected_before_restart(self):
        (self.package / 'bin/mr-li').write_text('tampered')
        with patch.object(auto, 'restart') as restart:
            with self.assertRaisesRegex(RuntimeError, 'hash mismatch'):
                auto.deploy(self.root, self.package, 'test/501')
            restart.assert_not_called()

    def test_package_cannot_replace_secrets_or_symlinks(self):
        (self.package / '.env').write_text('overwrite')
        with self.assertRaisesRegex(RuntimeError, 'Unexpected package'):
            auto.validate(self.package)
        (self.package / '.env').unlink()
        (self.package / 'bin/link').symlink_to(self.old_binary)
        with self.assertRaisesRegex(RuntimeError, 'Unexpected package'):
            auto.validate(self.package)

    def test_failed_config_check_leaves_running_files(self):
        (self.package / 'bin/mr-li').write_text('#!/bin/sh\nexit 1\n')
        auto.manifest(self.package, self.source)
        with patch.object(auto, 'restart') as restart:
            with self.assertRaises(subprocess.CalledProcessError):
                auto.deploy(self.root, self.package, 'test/501')
            restart.assert_not_called()
        self.assertEqual(self.old_binary.read_text(), 'old executable')

    def test_local_incoming_takes_priority(self):
        auto.write_json(self.state / 'config.json', {'domain': 'test/501'})
        self.package.rename(self.state / 'incoming')
        with patch.object(auto, 'restart'), patch.object(auto, 'healthy'), patch.object(auto, 'remote_package') as remote:
            auto.once(self.root)
            remote.assert_not_called()
        self.assertFalse((self.state / 'incoming').exists())

    def test_runner_checks_github_with_local_package_pending(self):
        auto.write_json(self.state / 'config.json', {'domain': 'test/501'})
        incoming = self.state / 'incoming'
        self.package.rename(incoming)
        with patch.object(auto, 'remote_package', return_value=None) as remote:
            auto.once(self.root, github_only=True)
            remote.assert_called_once()
        self.assertTrue(incoming.is_dir())

    def test_local_only_tick_does_not_check_github(self):
        auto.write_json(self.state / 'config.json', {'domain': 'test/501'})
        with patch.object(auto, 'remote_package') as remote:
            auto.once(self.root, local_only=True)
            remote.assert_not_called()

    def test_parallel_tick_skipped(self):
        with (self.state / 'lock').open('a') as lock:
            auto.fcntl.flock(lock, auto.fcntl.LOCK_EX | auto.fcntl.LOCK_NB)
            with patch.object(auto, 'remote_package') as remote:
                auto.once(self.root)
                remote.assert_not_called()

    def prepare_remote(self, revision):
        # Real local Git transport exercises fetching/ancestry without the network.
        remote = self.state / 'source'
        auto.run(['git', 'clone', '-q', self.source, remote])
        branch = self.git('branch', '--show-current')
        auto.write_json(self.state / 'installed.json', {'revision': revision})
        return remote, {'repo': str(self.source), 'branch': branch}

    def test_same_github_commit_does_not_build(self):
        _, config = self.prepare_remote(self.value['revision'])
        self.assertIsNone(auto.remote_package(self.root, config))

    def test_unchanged_github_polls_are_silent(self):
        _, config = self.prepare_remote(self.value['revision'])
        auto.write_json(self.state / 'config.json', dict(config, domain='test/501'))
        with patch.object(auto, 'restart') as restart:
            auto.once(self.root)
            auto.once(self.root)
            restart.assert_not_called()
        self.assertFalse((self.root / '.data/utterances').exists())
        self.assertFalse((self.state / 'last-announcement.json').exists())

    def test_new_github_commit_builds_in_isolated_checkout(self):
        remote, config = self.prepare_remote(self.value['revision'])
        (self.source / 'file').write_text('new GitHub code')
        self.git('commit', '-qam', 'new release')
        revision = self.git('rev-parse', 'HEAD')
        original_run = auto.run
        platform = auto.output(['uname', '-s']) + '-' + auto.output(['uname', '-m'])
        built = remote / 'dist' / ('mr-li-' + platform)

        def fake_build(args, **kwargs):
            if str(args[0]) == '/bin/bash':
                self.assertEqual((self.root / '.data/utterances').read_text().strip(), auto.START_MESSAGE)
                self.assertEqual(kwargs['cwd'], remote)
                self.assertEqual(auto.output(['git', '-C', remote, 'rev-parse', 'HEAD']), revision)
                auto.shutil.copytree(self.package, built)
                auto.manifest(built, remote)
                return None
            return original_run(args, **kwargs)

        with patch.object(auto, 'run', side_effect=fake_build):
            self.assertEqual(auto.remote_package(self.root, config), built)
        self.assertEqual(auto.validate(built)['revision'], revision)
        self.assertEqual(self.old_binary.read_text(), 'old executable')

    def test_start_announcement_failure_still_attempts_build(self):
        _, config = self.prepare_remote(self.value['revision'])
        (self.source / 'scripts').mkdir()
        (self.source / 'scripts/build-release.sh').write_text('exit 1\n')
        self.git('add', '.')
        self.git('commit', '-qm', 'failing build')
        with patch.object(auto, 'speak', side_effect=OSError('speaker unavailable')) as speak:
            with self.assertRaisesRegex(subprocess.CalledProcessError, 'build-release.sh'):
                auto.remote_package(self.root, config)
            speak.assert_called_once_with(self.root, auto.START_MESSAGE, timeout=60)

    def test_github_behind_local_does_not_downgrade(self):
        (self.source / 'file').write_text('two')
        self.git('commit', '-qam', 'second')
        second = self.git('rev-parse', 'HEAD')
        remote, config = self.prepare_remote(second)
        self.git('reset', '--hard', self.value['revision'])
        self.assertIsNone(auto.remote_package(self.root, config))
        self.assertEqual(auto.output(['git', '-C', remote, 'rev-parse', 'HEAD']), second)

    def test_local_file_change_requires_redeploy_even_same_manifest(self):
        with patch.object(auto, 'restart') as restart, patch.object(auto, 'healthy'):
            auto.deploy(self.root, self.package, 'test/501')
            self.old_binary.write_text('changed externally')
            auto.deploy(self.root, self.package, 'test/501')
            self.assertEqual(restart.call_count, 2)
        self.assertEqual(self.old_binary.read_bytes(), (self.package / 'bin/mr-li').read_bytes())

    def test_voice_failure_does_not_roll_back_or_restart(self):
        (self.root / 'scripts/run.sh').write_text('#!/bin/sh\nexit 1\n')
        with patch.object(auto, 'restart') as restart, patch.object(auto, 'healthy'):
            auto.deploy(self.root, self.package, 'test/501')
            auto.deploy(self.root, self.package, 'test/501')
            self.assertEqual(restart.call_count, 1)
        self.assertEqual(json.loads((self.state / 'installed.json').read_text()), self.value)
        self.assertEqual(json.loads((self.state / 'last-announcement.json').read_text())['status'], 'failed')

    def test_message_uses_only_new_commits_and_strips_conventional_prefix(self):
        remote, _ = self.prepare_remote(self.value['revision'])
        self.git('commit', '--allow-empty', '-qm', 'feat(deploy): 升级成功后播报更新内容')
        self.git('commit', '--allow-empty', '-qm', 'fix(voice): 修复语音超时')
        auto.run(['git', '-C', remote, 'fetch', '-q'])
        updated = auto.manifest(self.package, self.source)
        text = auto.upgrade_message(self.root, updated, self.value)
        self.assertIn('修复语音超时；升级成功后播报更新内容', text)
        self.assertNotIn('initial', text)
        self.assertNotIn('feat(', text)

    def test_local_package_has_change_summary_without_git_checkout(self):
        self.git('commit', '--allow-empty', '-qm', 'feat(deploy): 升级成功后播报更新内容')
        updated = auto.manifest(self.package, self.source)
        text = auto.upgrade_message(self.root, updated, self.value)
        self.assertIn('升级成功后播报更新内容', text)
        self.assertNotIn('initial', text)

    def test_same_revision_local_changes_do_not_claim_old_commit(self):
        self.assertEqual(auto.upgrade_message(self.root, self.value, self.value),
                         '升级成功。本次更新：更新了本机部署文件。')

    def test_release_notes_are_literal_and_bounded(self):
        updated = dict(self.value, release_notes='增加升级播报 $(touch /tmp/not-executed)\n' + '功能' * 300)
        text = auto.upgrade_message(self.root, updated, self.value)
        self.assertIn('$(touch /tmp/not-executed)', text)
        self.assertNotIn('\n', text)
        self.assertLess(len(text), 500)


if __name__ == '__main__':
    unittest.main()
