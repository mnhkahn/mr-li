import datetime as dt
import json
from pathlib import Path
import runpy
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch


retry = runpy.run_path(str(Path(__file__).with_name('news-broadcast.py')))['fetch_with_backoff']


class NewsRetryTest(unittest.TestCase):
    date = '2026-11-24'
    start = dt.datetime.fromisoformat(date + 'T07:10:00+08:00')
    deadline = start.replace(minute=57)

    def setUp(self):
        self.now = self.start
        self.attempts = []

    def advance(self, seconds):
        self.now += dt.timedelta(seconds=seconds)

    def run_retry(self, fetch):
        def record_fetch(date, timeout):
            self.attempts.append((self.now, timeout))
            self.assertEqual(date, self.date)
            return fetch(timeout)

        clock = SimpleNamespace(now=lambda tz: self.now)
        with patch.dict(retry.__globals__, dt=SimpleNamespace(datetime=clock, timedelta=dt.timedelta),
                        time=SimpleNamespace(sleep=self.advance), fetch=record_fetch):
            return retry(self.date, self.deadline)

    def test_picks_up_news_published_at_075639(self):
        published = self.start.replace(minute=56, second=39)

        def fetch(timeout):
            if self.now < published:
                raise RuntimeError('栏目尚未发布')
            return {'date': self.date}

        self.assertEqual(self.run_retry(fetch), {'date': self.date})
        initial_attempts = [at for at, _ in self.attempts[:4]]
        delays = [(right - left).total_seconds() for left, right in zip(initial_attempts, initial_attempts[1:])]
        self.assertEqual(delays, [20, 40, 60])
        self.assertEqual(self.attempts[-1][0], self.start.replace(minute=56, second=45))

    def test_final_probe_has_shorter_timeout_and_surfaces_last_error(self):
        def fetch(timeout):
            raise OSError('网络不可用')

        with self.assertRaisesRegex(OSError, '网络不可用'):
            self.run_retry(fetch)
        self.assertEqual(self.attempts[-1], (self.start.replace(minute=56, second=45), 15))

    def test_late_result_does_not_take_synthesis_time(self):
        self.now = self.start.replace(minute=56, second=45)

        def fetch(timeout):
            self.advance(timeout + 1)
            return {'date': self.date}

        with self.assertRaises(TimeoutError):
            self.run_retry(fetch)
        self.assertEqual(len(self.attempts), 1)

    def test_only_late_scheduled_synthesis_uses_the_faster_setting(self):
        prepare = retry.__globals__['prepare']
        for fetched_at, preview, expected in (
            (self.start, False, []),
            (self.start.replace(minute=56, second=45), False, ['--max-steps', '4']),
            (self.start.replace(minute=56, second=45), True, []),
        ):
            with self.subTest(fetched_at=fetched_at, preview=preview), TemporaryDirectory() as tmp:
                self.now = self.start

                def fetch(date, *args):
                    self.now = fetched_at
                    return {'date': date}

                def synthesize(command, **kwargs):
                    self.assertEqual(command[1:], ['--cache-only'] + expected)
                    self.assertEqual(kwargs['timeout'], 540 if preview else
                                     (self.start.replace(hour=8, minute=0) - fetched_at).total_seconds())
                    return SimpleNamespace(stdout='/tmp/speech.wav\n')

                clock = SimpleNamespace(now=lambda tz: self.now)
                with patch.dict(prepare.__globals__,
                                dt=SimpleNamespace(datetime=clock, timedelta=dt.timedelta),
                                fetch=fetch, fetch_with_backoff=fetch, optional=lambda read, date, default: default,
                                make_script=lambda *args: '早上好。', weather=None, calendar=None,
                                write_audio=lambda *args: 'morning.wav'), \
                     patch('subprocess.run', new=synthesize), patch('builtins.print'):
                    prepare(Path(tmp), self.date, preview)
                suffix = '-preview' if preview else ''
                record = json.loads((Path(tmp) / (self.date + suffix + '.json')).read_text())
                self.assertEqual(record['status'], 'ready')
                self.assertEqual(record.get('tts_max_steps'), 4 if expected else None)


if __name__ == '__main__':
    unittest.main()
