#!/usr/bin/env python3
"""Install the daily 08:30 Beijing news job on the deployment Mac."""
import datetime as dt
import os
from pathlib import Path
import shlex
import subprocess
import sys

root = Path(__file__).resolve().parent.parent
if sys.platform != 'darwin' or dt.datetime.now().astimezone().utcoffset() != dt.timedelta(hours=8):
    raise SystemExit('Install on the Mac mini configured for Beijing time (UTC+8).')
os.umask(0o077)
state = root / '.data/news-broadcast'
state.mkdir(parents=True, exist_ok=True)
(root / 'logs').mkdir(exist_ok=True)
previous = subprocess.run(['/usr/bin/crontab', '-l'], capture_output=True, text=True)
if previous.returncode and 'no crontab for' not in previous.stderr:
    raise SystemExit('Cannot read existing crontab: ' + previous.stderr)
backup = state / ('crontab-backup-' + dt.datetime.now().strftime('%Y%m%d-%H%M%S'))
backup.write_text(previous.stdout)
lines, skip = [], False
for line in previous.stdout.splitlines():
    if line == '# BEGIN MR-LI NEWS':
        skip = True
    elif line == '# END MR-LI NEWS':
        skip = False
    elif not skip:
        lines.append(line)
lines += ['# BEGIN MR-LI NEWS',
          '30 8 * * * /usr/bin/python3 ' + shlex.quote(str(root / 'scripts/news-broadcast.py')) +
          ' >> ' + shlex.quote(str(root / 'logs/news-broadcast.log')) + ' 2>&1',
          '# END MR-LI NEWS']
subprocess.run(['/usr/bin/crontab', '-'], input='\n'.join(lines) + '\n', text=True, check=True)
print('Installed on this Mac: daily 08:30 Beijing time. Backup: ' + str(backup))
