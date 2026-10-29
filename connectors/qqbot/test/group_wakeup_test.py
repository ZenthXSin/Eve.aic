"""生产 Rust→Node 桥接：明确呼叫绕过耗尽预算、失败 Jev 和插话限流，不重答。"""
from fixture_auth import configure_fixture_progress
import json
import os
from pathlib import Path
import signal
import subprocess
import unittest

import group_relation_test
import reminder_test


@unittest.skipIf(os.name == 'nt', '真实 SIGTERM 收尾由 Ubuntu 验收')
class GroupWakeupAcceptance(group_relation_test.GroupRelationAcceptance):
    for _name in dir(group_relation_test.GroupRelationAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def test_prefix_only_bridge_does_not_wake_on_at_or_plain_group_and_keeps_private_chat(self):
        root = self.work / 'prefix-bridge'
        root.mkdir()
        source = Path(__file__).resolve().parents[1]
        for name in ['bridge.mjs', 'bridge-core.mjs']:
            (root / name).write_bytes((source / name).read_bytes())
        sdk = root / 'node_modules/@tencent-connect/qqbot-nodejs'
        sdk.mkdir(parents=True)
        (sdk / 'package.json').write_text(json.dumps({'type':'module','exports':'./index.mjs'}))
        record = self.work / 'prefix-sends.jsonl'
        code = r'''import fs from "node:fs";
const record=RECORD;
export class QQBot {
 constructor(options){this.appId=options.appId;this.handlers=new Map();this.count=0;}
 on(name,handler){this.handlers.set(name,handler);}
 message(id,text,type="GROUP_MESSAGE_CREATE",kind="group"){
  this.handlers.get("message")({}, {kind,messageId:id,senderId:"member-1",content:text,
   rawEventType:type,groupOpenid:"prefix-group",replyTarget:{scope:kind,targetId:kind==="group"?"prefix-group":"member-1",msgId:id}});
 }
 start(){this.handlers.get("ready")();
  this.message("ambient","大家好");
  this.message("at-only","<@1904159860> 帮忙","GROUP_AT_MESSAGE_CREATE");
  this.message("prefix-1","  eVe 在不在");
  return new Promise(resolve=>{this.resolve=resolve;});
 }
 async sendText(target,text){fs.appendFileSync(record,JSON.stringify({target,text})+"\n");
  const n=this.count++;
  setTimeout(()=>{if(n===0)this.message("prefix-2","<@1904159860> EVE帮忙","GROUP_AT_MESSAGE_CREATE");
   if(n===1)this.message("private","私聊你好","C2C_MESSAGE_CREATE","c2c");},50);
  return {id:"sent-"+n};
 }
 stop(){this.resolve?.();}
}'''
        (sdk / 'index.mjs').write_text(code.replace('RECORD',json.dumps(str(record))))
        env = {key:os.environ[key] for key in ('PATH','SystemRoot','TEMP','TMP') if key in os.environ}
        env.update(QQBOT_APP_SECRET='test-app-secret',QQBOT_APP_ID='1904159860',
            EVE_OPENAI_API_KEY='test-model-secret',EVE_OPENAI_BASE_URL=f'http://127.0.0.1:{self.server.server_port}',
            EVE_OPENAI_PROTOCOL='chat',EVE_LLM_RESPONSE_MODE='complete',EVE_DEBOUNCE_MS='0',
            QQBOT_GROUP_PREFIX_ONLY='true',EVE_CHIME_ENABLED='false',EVE_HISTORY_DIGEST_ENABLED='false',
            EVE_JEV_API_KEY='group-jev-test-secret',EVE_JEV_BASE_URL=f'http://127.0.0.1:{self.jev_server.server_port}',
            EVE_MODELS_JEV_ENABLED='true',EVE_MODELS_JEV_PROVIDER='jev',EVE_MODELS_JEV_MODEL='group-jev-test-model')
        self.runs += 1
        child = subprocess.Popen([str(reminder_test.BINARY),'--group-chat','--message-judge','jev',
            '--state-dir',str(self.work/'state'),'--agent',str(source.parents[1]/'AGENT.md'),
            '--bridge-script',str(root/'bridge.mjs')],env=configure_fixture_progress(env),text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        try:
            self.wait_for(lambda:record.exists() and len(record.read_text().splitlines())==3,'仅前缀或私聊唤醒失败',25)
            self.quiet()
            child.send_signal(signal.SIGTERM)
            stdout,stderr=child.communicate(timeout=15)
            self.assertEqual(child.returncode,0,stderr)
            sends=[json.loads(line) for line in record.read_text().splitlines()]
            self.assertEqual([r['target']['msgId'] for r in sends],['prefix-1','prefix-2','private'])
            self.assertEqual(len(self.kind('chat')),3)
            self.assertEqual(self.jev_requests,[])
            self.assertEqual(self.kind('chime'),[])
            self.assertEqual(self.errors,[])
        finally:
            if child.poll() is None: child.kill();child.communicate(timeout=5)

    def test_prefix_wakes_with_exhausted_budget_broken_jev_and_hourly_limit(self):
        self.jev_status = 503
        pulse = self.work / 'budget-spent'
        original_reply = self.reply

        def reply(kind, decoded, latest):
            result = original_reply(kind, decoded, latest)
            if kind == 'chime':
                pulse.touch()
            return result

        self.reply = reply
        root = self.work / 'production-bridge'
        root.mkdir()
        source = Path(__file__).resolve().parents[1]
        for name in ['bridge.mjs', 'bridge-core.mjs']:
            (root / name).write_bytes((source / name).read_bytes())
        sdk = root / 'node_modules/@tencent-connect/qqbot-nodejs'
        sdk.mkdir(parents=True)
        (sdk / 'package.json').write_text(json.dumps({'type': 'module', 'exports': './index.mjs'}))
        record = self.work / 'sdk-sends.jsonl'
        code = r'''import fs from "node:fs";
const record=RECORD, pulse=PULSE;
const texts=["eve在不在","EVE","eVe帮我看看","  EvE 在吗"];
export class QQBot {
  constructor(options){this.appId=options.appId;this.handlers=new Map();this.count=0;}
  on(name,handler){this.handlers.set(name,handler);}
  message(id,text){this.handlers.get("message")({}, {kind:"group",messageId:id,senderId:"member-1",content:text,rawEventType:"GROUP_MESSAGE_CREATE",groupOpenid:"wake-group",replyTarget:{scope:"group",targetId:"wake-group",msgId:id}});}
  start(){
    this.handlers.get("ready")();
    this.message("ambient","大家好");
    this.timer=setInterval(()=>{if(fs.existsSync(pulse)){clearInterval(this.timer);this.message("wake-0",texts[0]);}},20);
    return new Promise(resolve=>{this.resolve=resolve;});
  }
  async sendText(target,text){
    fs.appendFileSync(record,JSON.stringify({target,text})+"\n");
    const index=this.count++;
    setTimeout(()=>{
      this.message("wake-"+index,texts[index]);
      if(index+1<texts.length)this.message("wake-"+(index+1),texts[index+1]);
    },50);
    return {id:"sent-"+index};
  }
  stop(){clearInterval(this.timer);this.resolve?.();}
}'''
        (sdk / 'index.mjs').write_text(code.replace('RECORD', json.dumps(str(record))).replace('PULSE', json.dumps(str(pulse))))
        env = {key: os.environ[key] for key in ('PATH', 'SystemRoot', 'TEMP', 'TMP') if key in os.environ}
        env.update(QQBOT_APP_SECRET='test-app-secret', QQBOT_APP_ID='1904159860',
                   EVE_OPENAI_API_KEY='test-model-secret', EVE_OPENAI_BASE_URL=f'http://127.0.0.1:{self.server.server_port}',
                   EVE_OPENAI_PROTOCOL='chat', EVE_LLM_RESPONSE_MODE='complete', EVE_DEBOUNCE_MS='0',
                   EVE_JEV_API_KEY='group-jev-test-secret', EVE_JEV_BASE_URL=f'http://127.0.0.1:{self.jev_server.server_port}',
                   EVE_MODELS_JEV_ENABLED='true', EVE_MODELS_JEV_PROVIDER='jev', EVE_MODELS_JEV_MODEL='group-jev-test-model',
                   EVE_HISTORY_DIGEST_ENABLED='false', EVE_GROUP_RELATION_LEARNING_ENABLED='false',
                   EVE_GROUP_RELATION_ACTIVE_INTERVAL_MS='1000', EVE_CHIME_COOLDOWN_MS='3600000',
                   EVE_CHIME_MAX_REPLIES_PER_HOUR='1')
        self.runs += 1
        child = subprocess.Popen([str(reminder_test.BINARY), '--group-chat', '--training', '--interest-learning',
            '--message-judge', 'jev', '--autonomy-budget', '1', '--state-dir', str(self.work / 'state'),
            '--agent', str(source.parents[1] / 'AGENT.md'), '--bridge-script', str(root / 'bridge.mjs')],
            env=configure_fixture_progress(env), text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            self.wait_for(lambda:child.poll() is not None or len(self.kind('chat')) >= 4, '名字呼叫未逐条进入普通会话', 25)
            self.wait_for(lambda:sum(e['state'] == 'Sent' for e in self.documents().get('eve.channel.qqbot', {}).get('receipts.v1', {}).get('entries', [])) == 4,
                          '名字呼叫未保存四条发送回执', 10)
            self.quiet()
            child.send_signal(signal.SIGTERM)
            stdout, stderr = child.communicate(timeout=15)
            self.assertEqual(child.returncode, 0, stderr)
            sends = [json.loads(line) for line in record.read_text().splitlines()]
            self.assertEqual([r['target']['msgId'] for r in sends], ['wake-0', 'wake-1', 'wake-2', 'wake-3'])
            self.assertTrue(all(r['text'].startswith('<@member-1> ') for r in sends))
            self.assertEqual(len(self.kind('chat')), 4)
            self.assertEqual(len(self.kind('chime')), 1)
            self.assertEqual(len(self.jev_requests), 1, '名字呼叫不能进入 Jev 关系判断')
            self.assertEqual(len(self.relation_rows('judgments')), 1, '名字呼叫不能积攒到待补答池')
            self.assertEqual(self.rows('outbound'), [], '名字呼叫只走普通回复，不产生主动补答')
            budget = self.documents()['eve.budget']['usage.v1']['current']
            self.assertEqual(budget['used'], 1)
            self.assertEqual(budget['by_category'], {'chime': 1})
            self.assertTrue(all(not json.loads(r['payload'])['mentions_me'] for r in self.rows('observations')))
            self.assertEqual(self.errors, [])
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate(timeout=5)


if __name__ == '__main__':
    unittest.main()
