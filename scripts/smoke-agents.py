#!/usr/bin/env python3
"""Verify the production Agent worker and optional real CLI handshakes, without model calls.

python3 scripts/smoke-agents.py target/debug/mr-li
python3 scripts/smoke-agents.py target/debug/mr-li --cli antigravity /path/to/agy

Uses isolated HOME/XDG directories. Never logs in or sends prompts to a model.
"""
import argparse
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time


def line(process, timeout=30):
    if not select.select([process.stdout], [], [], timeout)[0]:
        raise AssertionError('Agent handshake timed out')
    value = process.stdout.readline()
    assert value, 'Agent exited before replying'
    return json.loads(value)


def gone(pid):
    p = subprocess.run(['/bin/ps', '-o', 'stat=', '-p', str(pid)], capture_output=True, text=True)
    return p.returncode != 0 or not p.stdout.strip() or p.stdout.strip().startswith('Z')


def wait_gone(pids):
    for _ in range(100):
        if all(gone(pid) for pid in pids):
            return
        time.sleep(.02)
    raise AssertionError('Agent or tool survived loss of parent transport')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('bridge')
    parser.add_argument('--cli', nargs=2, action='append', default=[], metavar=('KIND', 'BINARY'))
    args = parser.parse_args()
    bridge = str(Path(args.bridge).resolve())
    with tempfile.TemporaryDirectory(prefix='mr-li-agent-smoke-') as temporary:
        root = Path(temporary)
        env = {'PATH': os.environ.get('PATH', '/usr/bin:/bin'), 'HOME':temporary,
               'XDG_CONFIG_HOME':str(root/'config'), 'XDG_DATA_HOME':str(root/'data'),
               'XDG_CACHE_HOME':str(root/'cache'), 'TMPDIR':temporary}
        code = """import json, os, subprocess, sys
child = subprocess.Popen([sys.executable, '-c', 'import time;time.sleep(3600)'])
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({'id':request['id'],'result':{'pid':os.getpid(),'tool':child.pid}}),flush=True)
"""
        worker = subprocess.Popen([bridge,'agent-worker','--',sys.executable,'-u','-c',code],
                                  stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                                  env=env,cwd=temporary,text=True,start_new_session=True)
        try:
            worker.stdin.write('{"id":1,"method":"probe"}\n');worker.stdin.flush()
            response=line(worker)
            assert response['id']==1
            pids=[response['result']['pid'],response['result']['tool']]
            worker.stdin.close()  # Exactly the EOF a crashed bridge leaves behind.
            worker.wait(timeout=5)
            wait_gone(pids)
        finally:
            if worker.poll() is None:
                os.killpg(worker.pid,9);worker.wait()
        print('PASS: production worker forwards NDJSON; parent EOF stops CLI and tool descendants')
        for kind,binary in args.cli:
            flags={'antigravity':['acp'],'kilo':['acp'],'cline':['--acp'],
                   'trae':['app-server','--listen','stdio://']}[kind]
            cli_env=dict(env)
            if kind=='kilo':cli_env['KILO_CONFIG_CONTENT']=json.dumps({'model':'kilo/kilo-auto/free','small_model':'kilo/kilo-auto/free'})
            process=subprocess.Popen([bridge,'agent-worker','--',str(Path(binary).resolve()),*flags],
                                     stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,
                                     env=cli_env,cwd=temporary,text=True,start_new_session=True)
            try:
                params={'clientInfo':{'name':'mr_li_smoke','version':'1'},'capabilities':{}} if kind=='trae' else {
                    'protocolVersion':1,'clientInfo':{'name':'mr-li-smoke','version':'1'},'clientCapabilities':{'fs':{'readTextFile':False,'writeTextFile':False},'terminal':False}}
                process.stdin.write(json.dumps({'jsonrpc':'2.0','id':1,'method':'initialize','params':params})+'\n');process.stdin.flush()
                response=line(process)
                assert response.get('id')==1 and 'result' in response, response
                if kind!='trae':assert response['result']['protocolVersion']==1
                if kind=='antigravity':
                    assert any(m['id']=='oauth-personal' for m in response['result']['authMethods'])
                if kind=='kilo':
                    process.stdin.write(json.dumps({'jsonrpc':'2.0','id':2,'method':'session/new','params':{'cwd':temporary,'mcpServers':[]}})+'\n');process.stdin.flush()
                    response=line(process)
                    while response.get('id')!=2: response=line(process)
                    assert 'result' in response, response
                    session=response['result']['sessionId']
                    process.stdin.write(json.dumps({'jsonrpc':'2.0','id':3,'method':'session/set_model','params':{'sessionId':session,'modelId':'kilo/kilo-auto/free'}})+'\n');process.stdin.flush()
                    response=line(process)
                    while response.get('id')!=3: response=line(process)
                    assert 'result' in response, response
                print(f'PASS: {kind} real CLI initialization; no authentication or model request')
            finally:
                process.stdin.close()
                try:process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid,9);process.wait()
    return 0

if __name__=='__main__':
    raise SystemExit(main())
