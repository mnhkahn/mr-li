#!/usr/bin/env python3
"""Protocol fixture only: never connects to a provider or runs model tasks."""
import json, os, sys
raw = 'session-' + str(os.getpid())
pending = None
turn = 0
native = 'app-server' in sys.argv

def send(value):
    print(json.dumps(value), flush=True)
def reply(request, result):
    send({'jsonrpc':'2.0', 'id':request['id'], 'result':result})
def event(method, params):
    send({'jsonrpc':'2.0', 'method':method, 'params':params})
def finish_native():
    event('item/agentMessage/delta', {'threadId':raw,'turnId':str(turn),'itemId':'message','delta':'native '})
    event('item/completed', {'threadId':raw,'turnId':str(turn),'item':{'id':'message','type':'agentMessage','text':'native answer'}})
    event('turn/completed', {'threadId':raw,'turn':{'id':str(turn),'status':'completed','items':[]}})
for line in sys.stdin:
    r = json.loads(line)
    with open('calls.jsonl','a') as log:
        log.write(json.dumps(r)+'\n')
    m, p = r.get('method'), r.get('params',{})
    if m == 'initialize':
        if os.environ.get('CLINE_MCP_SETTINGS_PATH'):
            assert os.environ.get('CLINE_SESSION_BACKEND_MODE') == 'local'
            with open('cline-settings-path', 'w') as settings:
                settings.write(os.environ['CLINE_MCP_SETTINGS_PATH'])
        reply(r, {'protocolVersion':1,'agentCapabilities':{'loadSession':True},'authMethods':[{'id':'oauth-personal'}]})
    elif m == 'account/read':
        reply(r, {'account':{'type':'trae','userId':'test'}})
    elif m in ('initialized', 'session/cancel'):
        pass
    elif m in ('authenticate','session/set_model'):
        if m == 'session/set_model':
            assert p['modelId'] in ('kilo/kilo-auto/free', 'gemini-3.8-flash-high')
            if p['modelId']=='kilo/kilo-auto/free':
                config=json.loads(os.environ['KILO_CONFIG_CONTENT'])
                assert config['small_model']=='kilo/kilo-auto/free'
        reply(r,{})
    elif m in ('thread/start','thread/resume'):
        raw = p.get('threadId', raw)
        reply(r,{'thread':{'id':raw}})
    elif m in ('session/new','session/load'):
        raw = p.get('sessionId', raw)
        reply(r,{'sessionId':raw,'models':{'availableModels':[{'modelId':'gemini-3.8-flash-high'}]}})
    elif m == 'session/prompt':
        if p['prompt'][0]['text']=='wait':
            pending=r
            continue
        event('session/update', {'sessionId':raw, 'update':{'sessionUpdate':'agent_thought_chunk','content':{'type':'text','text':'PRIVATE THOUGHT'}}})
        event('session/update', {'sessionId':'foreign', 'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'FOREIGN'}}})
        event('session/update', {'sessionId':raw, 'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'hello '}}})
        send({'jsonrpc':'2.0','id':500,'method':'session/request_permission','params':{'sessionId':raw,'options':[{'kind':'allow_always','optionId':'never-always'},{'kind':'allow_once','optionId':'allow-once'}]}})
        pending=r
    elif m == 'turn/start':
        turn += 1
        assert p['threadId']==raw
        if p['input'][0]['text']=='quota':
            reply(r,{'turn':{'id':str(turn)}})
            event('turn/completed',{'threadId':raw,'turn':{'id':str(turn),'status':'failed','error':{'message':'free quota exhausted'}}})
        elif p['input'][0]['text']=='question':
            reply(r,{'turn':{'id':str(turn)}})
            send({'id':'question','method':'item/tool/requestUserInput','params':{'threadId':raw,'turnId':str(turn),'itemId':'q','questions':[{'id':'choice','header':'选择','question':'选哪个？','options':[{'label':'A','description':'方案 A'}]}]}})
        else:
            # Deliberately emit completion before acknowledging turn/start.
            finish_native()
            reply(r,{'turn':{'id':str(turn)}})
    elif m is None and r['id']==500:
        assert r['result']['outcome']['optionId']=='allow-once'
        event('session/update', {'sessionId':raw,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'world'}}})
        reply(pending,{'stopReason':'end_turn'})
    elif m is None and r['id']=='question':
        assert r['result']['answers']['choice']['answers']==['A']
        finish_native()
    else:
        if 'id' in r: reply(r,{})
