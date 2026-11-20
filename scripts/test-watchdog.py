#!/usr/bin/env python3
"""Exercise system watchdog restart isolation using unprivileged mocks."""
import os
from pathlib import Path
import subprocess
import tempfile

source = (Path(__file__).parent/'watchdog-system.sh').read_text()
with tempfile.TemporaryDirectory(prefix='mr-li-watchdog-') as directory:
    root=Path(directory)
    (root/'scripts').mkdir()
    for name, content in {
        'sudo': '#!/bin/bash\nwhile [[ "$1" != -- ]]; do shift; done\nshift\nexec "$@"\n',
        'launchctl': '#!/bin/bash\nprintf "%s\\n" "$*" >> "$TRACE_FILE"\n',
        'scripts/run.sh': '#!/bin/bash\nif [[ "$1" == health ]]; then exit "$FAIL_OPEN"; else exit "$FAIL_BRIDGE"; fi\n',
    }.items():
        file=root/name;file.write_text(content);file.chmod(0o700)
    source=source.replace('/usr/bin/sudo',str(root/'sudo')).replace('/bin/launchctl',str(root/'launchctl')).replace('/Library/Application Support/com.cyeam.mr-li/health-failures',str(root/'state'))
    wrapper=root/'watchdog.sh';wrapper.write_text(source)
    trace=root/'trace'
    env={**os.environ,'TRACE_FILE':str(trace),'FAIL_OPEN':'1','FAIL_BRIDGE':'0'}
    for _ in range(3):subprocess.run(['/bin/bash',str(wrapper),str(root),'test-user'],env=env,check=True)
    assert trace.read_text().splitlines()==['kickstart -k system/com.cyeam.mr-li.opencode']
    env.update(FAIL_OPEN='0',FAIL_BRIDGE='1')
    for _ in range(3):subprocess.run(['/bin/bash',str(wrapper),str(root),'test-user'],env=env,check=True)
    assert trace.read_text().splitlines()==['kickstart -k system/com.cyeam.mr-li.opencode','kickstart -k system/com.cyeam.mr-li.bridge']
print('PASS: OpenCode and bridge failures restart only the failed service; no sudo/launchd used')
