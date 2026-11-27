#!/usr/bin/env python3
"""Prepare a cloned-voice morning bulletin and play the saved audio at 08:00."""
import argparse
import datetime as dt
import fcntl
from html.parser import HTMLParser
from http.client import HTTPException
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import urllib.parse
import urllib.request
import wave

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MUSIC = 'morning-mood.wav'
SNOW_CODES = {71, 73, 75, 77, 85, 86}
RAIN_CODES = {51, 53, 55, 56, 57, 61, 63, 65, 66, 67, 80, 81, 82, 95, 96, 97, 99}
WIND_GUST_THRESHOLD = 40  # km/h, used to choose music
BEIJING = dt.timezone(dt.timedelta(hours=8))
GEEK_URL = "https://www.cyeam.com/geek"
GEEK_FETCH_TIMEOUT = 30
PLAYBACK_HOUR = 8
PLAYBACK_LABEL = f'{PLAYBACK_HOUR:02d}:00'
STANDARD_SYNTH_BUDGET = 270  # seconds
WEATHER_URL = "https://api.open-meteo.com/v1/forecast"
WEATHER_CODES = {
    0: '晴', 1: '以晴为主', 2: '多云', 3: '阴', 45: '有雾', 48: '有雾',
    51: '有毛毛雨', 53: '有毛毛雨', 55: '有毛毛雨', 56: '有冻毛毛雨', 57: '有冻毛毛雨',
    61: '小雨', 63: '中雨', 65: '大雨', 66: '有冻雨', 67: '有冻雨',
    71: '小雪', 73: '中雪', 75: '大雪', 77: '有雪粒',
    80: '有阵雨', 81: '有阵雨', 82: '有强阵雨', 85: '有阵雪', 86: '有强阵雪',
    95: '有雷暴', 96: '有雷暴和冰雹', 97: '有强雷暴', 99: '有雷暴和冰雹',
}
ARABIC_DIGITS = '0123456789'
CHINESE_DIGITS = '零一二三四五六七八九'
DIGIT_NAMES = str.maketrans(ARABIC_DIGITS, CHINESE_DIGITS)
HALFWIDTH_NUMBERS = str.maketrans('０１２３４５６７８９％．＋－−', ARABIC_DIGITS + '%.+--')
SPOKEN_NUMBER = re.compile(
    r'(?P<sign>(?<![A-Za-z0-9])[+-])?'
    r'(?P<number>(?:[0-9]{1,3}(?:,[0-9]{3})+|[0-9]+)(?:\.[0-9]+)*)'
    r'(?P<percent>[ \t]*%)?'
)


def save(path, value):
    temp = path.with_suffix('.tmp')
    with temp.open('w') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(str(temp), str(path))


class GeekHeadlines(HTMLParser):
    # The page's article title links carry the edition, channel and displayed index.
    CHANNELS = {'ai': 'ai_news', 'tech': 'news'}

    def __init__(self, date):
        super().__init__()
        self.date = date
        self.items = {'ai_news': [], 'news': []}
        self.wrong_date = False
        self.article = None
        self.title_parts = []

    def handle_starttag(self, tag, attrs):
        if tag != 'a':
            return
        href = dict(attrs).get('href') or ''
        if not href.startswith('/geek/click?'):
            return
        query = urllib.parse.parse_qs(urllib.parse.urlsplit(href).query)
        section = self.CHANNELS.get(query.get('c', [''])[0])
        if section is None:
            return
        if query.get('d') != [self.date]:
            self.wrong_date = True
            return
        self.article = (section, href)
        self.title_parts = []

    def handle_data(self, data):
        if self.article is not None:
            self.title_parts.append(data)

    def handle_endtag(self, tag):
        if tag != 'a' or self.article is None:
            return
        section, href = self.article
        title = ' '.join(''.join(self.title_parts).split())
        if title:
            self.items[section].append({
                'title': title,
                'link': urllib.parse.urljoin(GEEK_URL, href),
            })
        self.article = None


def decode_news(page, date):
    parser = GeekHeadlines(date)
    parser.feed(page)
    parser.close()
    if not any(parser.items.values()):
        if parser.wrong_date:
            raise RuntimeError('Geek 头条日期与播报日期不一致')
        raise RuntimeError('当天 Geek 头条尚未发布')
    return {'date': date, **{section: {'news': items} for section, items in parser.items.items()}}


def fetch(date, timeout=GEEK_FETCH_TIMEOUT):
    url = GEEK_URL + '?' + urllib.parse.urlencode({'d': date})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(url, timeout=timeout) as response:
        page = response.read().decode('utf-8')
    return decode_news(page, date)


def fetch_with_backoff(date, deadline):
    delay = 20
    final_probe = deadline - dt.timedelta(seconds=15)
    partial = None
    last_error = TimeoutError('已到取稿截止时间，未能取得当天 Geek 头条')
    while True:
        started = dt.datetime.now(BEIJING)
        remaining = (deadline - started).total_seconds()
        if remaining <= 0:
            break
        try:
            data = fetch(date, timeout=min(GEEK_FETCH_TIMEOUT, remaining))
        except (OSError, HTTPException, ValueError, RuntimeError) as error:
            last_error = error
        else:
            if dt.datetime.now(BEIJING) > deadline:
                break
            if partial is None:
                partial = data
            else:
                for section in ('ai_news', 'news'):
                    if data[section]['news']:
                        partial[section] = data[section]
            if partial['ai_news']['news'] and partial['news']['news']:
                return partial
        now = dt.datetime.now(BEIJING)
        if started >= final_probe or now >= deadline:
            break
        # Keep a final request near the cutoff without spending synthesis time.
        time.sleep(max(0, min(delay, (final_probe - now).total_seconds())))
        delay = min(delay * 2, 60)
    if partial is not None:
        return partial
    raise last_error


def music_for_weather(code, snowfall, gust):
    if snowfall > 0 or code in SNOW_CODES:
        return 'morning-snow.wav'
    if code in RAIN_CODES:
        return 'morning-rain.wav'
    if gust >= WIND_GUST_THRESHOLD:
        return 'morning-wind.wav'
    return DEFAULT_MUSIC


def weather_response(url):
    command = ['/usr/bin/curl', '-fsS', '--connect-timeout', '5', '--max-time', '10']
    proxy = urllib.request.getproxies().get('https', '')
    endpoint = urllib.parse.urlsplit(proxy)
    if endpoint.hostname in ('127.0.0.1', 'localhost', '::1') and endpoint.username is None:
        try:
            return subprocess.check_output(
                command + ['--noproxy', '', '--proxy', proxy, url], text=True, timeout=12,
            )
        except subprocess.SubprocessError:
            pass
    return subprocess.check_output(command + ['--retry', '1', url], text=True, timeout=24)


def weather(date):
    params = urllib.parse.urlencode({
        'latitude': '39.9042', 'longitude': '116.4074', 'timezone': 'Asia/Shanghai',
        'daily': 'weather_code,temperature_2m_min,temperature_2m_max,snowfall_sum,wind_gusts_10m_max',
        'wind_speed_unit': 'kmh', 'forecast_days': 1,
    })
    raw = weather_response(WEATHER_URL + '?' + params)
    daily = json.loads(raw)['daily']
    index = daily['time'].index(date)
    code = daily['weather_code'][index]
    conditions = WEATHER_CODES[code]
    low = round(daily['temperature_2m_min'][index])
    high = round(daily['temperature_2m_max'][index])
    missing = [None] * len(daily['time'])
    gust = daily.get('wind_gusts_10m_max', missing)[index] or 0
    snowfall = daily.get('snowfall_sum', missing)[index] or 0
    text = f'北京今天{conditions}，最低{low}度，最高{high}度。'
    if gust >= WIND_GUST_THRESHOLD:
        text += f'最大阵风预计每小时{round(gust)}公里。'
    return text, music_for_weather(code, snowfall, gust)


def calendar(date):
    cli = Path.home() / '.local/bin/cyeam'
    raw = subprocess.check_output(
        [str(cli), '--no-analytics', 'date', 'holiday', date, '--json'],
        env=dict(os.environ, CYEAM_CLI_NO_UPDATE_NOTIFIER='1'),
        text=True, timeout=15,
    )
    result = json.loads(raw)
    data = result['data']
    if not result['ok'] or data['date'] != date:
        raise ValueError('日历日期不一致')
    parts = []
    if data.get('lunar'):
        parts.append('农历' + data['lunar'])
    parts.extend(dict.fromkeys(filter(None, [data.get('term'), *data.get('festivals', [])])))
    return '，'.join(parts) + '。' if parts else ''


def optional(read, date, fallback, *args):
    try:
        return read(date, *args)
    except (OSError, HTTPException, ValueError, KeyError, TypeError, IndexError,
            RuntimeError, subprocess.SubprocessError) as error:
        print(f'{date}: {read.__name__}: {error}', file=sys.stderr, flush=True)
        return fallback


def chinese_integer(value, leading=True):
    if value < 10:
        return CHINESE_DIGITS[value]
    for scale, unit in ((10**12, '万亿'), (10**8, '亿'), (10**4, '万'),
                        (1000, '千'), (100, '百'), (10, '十')):
        if value >= scale:
            head, tail = divmod(value, scale)
            words = unit if leading and value < 20 else chinese_integer(head) + unit
            if tail:
                gap = '零' if tail < scale // 10 else ''
                words += gap + chinese_integer(tail, leading=False)
            return words


def spoken_numbers(text):
    text = text.translate(HALFWIDTH_NUMBERS)

    def read(match):
        integer, *fractions = match['number'].replace(',', '').split('.')
        year = len(integer) == 4 and not fractions and text[match.end():].startswith('年')
        if year or len(integer) > 16 or (len(integer) > 1 and integer.startswith('0')):
            words = integer.translate(DIGIT_NAMES)
        else:
            words = chinese_integer(int(integer))
        words += ''.join('点' + part.translate(DIGIT_NAMES) for part in fractions)
        sign = {'-': '负', '+': '正', None: ''}[match['sign']]
        return sign + ('百分之' if match['percent'] else '') + words

    return SPOKEN_NUMBER.sub(read, text)


def make_script(data, date, calendar_text='', weather_text=''):
    day = dt.date.fromisoformat(date)
    weekday = '一二三四五六日'[day.weekday()]
    intro = spoken_numbers(
        f'早上好，该起床啦。今天是{day.year}年{day.month}月{day.day}日，星期{weekday}。'
        + calendar_text + weather_text
    )
    closing = '以上是今天的早间头条。'
    ai = data['ai_news']['news'] if data else []
    tech = data['news']['news'] if data else []
    choices = (
        ('AI 前沿', ai[:max(3, 5 - len(tech))]),
        ('技术雷达', tech[:max(2, 5 - len(ai))]),
    )
    lines = [intro, '接下来是早间 Geek 头条。']
    length = sum(len(line) + 1 for line in lines) + len(closing)
    for label, items in choices:
        headline = label + '。'
        for item in items:
            title = spoken_numbers(item['title'])
            punctuation = '' if title.endswith(('。', '！', '？', '!', '?')) else '。'
            line = headline + title + punctuation
            if length + len(line) + 1 <= 500:
                lines.append(line)
                length += len(line) + 1
                headline = ''
    if len(lines) == 2:
        return intro
    return '\n'.join(lines + [closing])


def music_frames(name, voice_format):
    with wave.open(str(ROOT / 'resource' / name), 'rb') as music:
        if voice_format != music.getparams()[:3] or not music.getnframes():
            raise ValueError('起床音乐与克隆语音须为格式一致的非空 WAV')
        frames = music.readframes(music.getnframes())
        if len(frames) != music.getnframes() * music.getnchannels() * music.getsampwidth():
            raise ValueError('起床音乐文件不完整')
        return frames


def write_audio(speech, output, music_name):
    with wave.open(str(speech), 'rb') as voice:
        if not voice.getnframes():
            raise ValueError('克隆语音为空')
        voice_format = voice.getparams()[:3]
        try:
            music = music_frames(music_name, voice_format)
        except (OSError, EOFError, wave.Error, ValueError) as error:
            if music_name == DEFAULT_MUSIC:
                raise
            print(f'{music_name}: {error}，使用默认起床音乐', file=sys.stderr, flush=True)
            music_name = DEFAULT_MUSIC
            music = music_frames(music_name, voice_format)
        temp = output.with_suffix('.wav.tmp')
        with wave.open(str(temp), 'wb') as audio:
            audio.setparams(voice.getparams())
            audio.writeframes(music)
            audio.writeframes(voice.readframes(voice.getnframes()))
        os.replace(str(temp), str(output))
    return music_name


def prepare(state, date, preview):
    record_path = state / (date + '.json')
    if not preview and record_path.exists():
        previous = json.loads(record_path.read_text())
        if previous['status'] in ('ready', 'sending', 'done', 'queued', 'uncertain', 'failed'):
            return
    record = {'date': date, 'status': 'preparing'}
    if not preview:
        save(record_path, record)
    try:
        now = dt.datetime.now(BEIJING)
        deadline = now.replace(hour=PLAYBACK_HOUR, minute=0, second=0, microsecond=0)
        fetch_deadline = deadline - dt.timedelta(minutes=3)
        calendar_text = optional(calendar, date, '')
        weather_text, music_name = optional(weather, date, ('', DEFAULT_MUSIC))
        data = optional(fetch, date, None) if preview else optional(fetch_with_backoff, date, None, fetch_deadline)
        suffix = '-preview' if preview else ''
        save(state / (date + suffix + '-news.json'),
             data or {'date': date, 'ai_news': {'news': []}, 'news': {'news': []}})
        text = make_script(data, date, calendar_text, weather_text)
        budget = 540 if preview else (deadline - dt.datetime.now(BEIJING)).total_seconds()
        if budget <= 0:
            raise TimeoutError(f'已到 {PLAYBACK_LABEL}，未能提前完成备播')
        command = [str(ROOT / 'scripts/local-voice.sh'), '--cache-only']
        if budget < STANDARD_SYNTH_BUDGET:
            command += ['--max-steps', '4']
            record['tts_max_steps'] = 4
        result = subprocess.run(
            command, input=text, text=True, stdout=subprocess.PIPE, check=True, timeout=budget,
        )
        speech = Path(result.stdout.strip().splitlines()[-1])
        audio_dir = state / 'audio'
        if preview:
            audio_dir = audio_dir / 'previews'
        audio_dir.mkdir(parents=True, exist_ok=True)
        music_name = write_audio(speech, audio_dir / (date + suffix + '.wav'), music_name)
        finished = dt.datetime.now(BEIJING)
        if not preview and finished >= deadline:
            raise TimeoutError(f'已到 {PLAYBACK_LABEL}，未能提前完成备播')
        record.update(status='ready', text=text, intro_music=music_name, prepared_at=finished.isoformat())
        save(state / (date + suffix + '.json'), record)
        if preview:
            print(text, flush=True)
        else:
            print(date + ': news audio ready', flush=True)
    except Exception as error:
        record.update(status='failed', error=str(error))
        save(state / (date + '-preview-error.json') if preview else record_path, record)
        raise


def play(state, date):
    path = state / (date + '.json')
    audio = state / 'audio' / (date + '.wav')
    record = json.loads(path.read_text()) if path.exists() else {'date': date}
    if record.get('status') in ('sending', 'done', 'queued', 'uncertain'):
        return
    legacy_audio = state / (date + '.wav')
    if record.get('status') == 'ready' and not audio.exists() and legacy_audio.is_file():
        audio.parent.mkdir(parents=True, exist_ok=True)
        legacy_audio.rename(audio)
    if record.get('status') != 'ready' or not audio.is_file():
        if record.get('status') != 'failed':
            save(path, dict(record, status='failed', error=f'{PLAYBACK_LABEL} 未找到当天准备好的音频'))
        raise RuntimeError(f'{PLAYBACK_LABEL} 未找到当天准备好的音频')
    record.update(status='sending', started_at=dt.datetime.now(BEIJING).isoformat())
    save(path, record)
    try:
        result = subprocess.run([str(ROOT / 'scripts/run.sh'), 'play-news-audio',
                                 str(audio.resolve()), record['text']],
                                stdout=subprocess.PIPE, text=True, check=True, timeout=300)
        outcome = result.stdout.strip().split()
        if len(outcome) == 2 and outcome[0] == 'queued':
            record.update(status='queued', inbox_id=int(outcome[1]))
        else:
            record.update(status='done', finished_at=dt.datetime.now(BEIJING).isoformat())
        save(path, record)
    except Exception as error:
        record.update(status='uncertain', error=str(error))
        save(path, record)
        raise


def main():
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--preview', action='store_true', help='prepare a preview WAV without playback')
    mode.add_argument('--prepare', action='store_true', help=f'prepare the scheduled {PLAYBACK_LABEL} WAV')
    mode.add_argument('--play', action='store_true', help='play only the prepared WAV')
    data_dir = Path(os.environ.get('MR_LI_DATA_DIR') or ROOT / '.data')
    parser.add_argument('--state-dir', type=Path, default=data_dir / 'news-broadcast')
    args = parser.parse_args()
    os.umask(0o077)
    state = args.state_dir
    state.mkdir(parents=True, exist_ok=True)
    date = dt.datetime.now(BEIJING).date().isoformat()
    lock_name = 'preview.lock' if args.preview else 'broadcast.lock'
    with (state / lock_name).open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            if args.play:
                play(state, date)
            else:
                prepare(state, date, args.preview)
        except Exception as error:
            print(f'{date}: {error}', file=sys.stderr, flush=True)
            return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
