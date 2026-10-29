"""真实 Eve→工具请求→监督器→第二个 Eve 进程，QQ 与模型均使用本地替身。"""
from fixture_auth import configure_fixture_progress
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import unittest

import developer_tools_test as developer
from dialogue_tools_test import tool_response, text_response
from memory_test import BINARY, FAKE, ROOT

spec=importlib.util.spec_from_file_location('upgrade_runtime',ROOT/'scripts/self_upgrade_runtime.py')
runtime=importlib.util.module_from_spec(spec);spec.loader.exec_module(runtime)

@unittest.skipUnless(os.name=='posix','本集成用 POSIX 定向信号；Windows 的进程切换由独立监督器测试覆盖')
class LiveUpgradeAcceptance(developer.DeveloperAcceptance):
    for name in dir(developer.DeveloperAcceptance):
        if name.startswith('test_'):locals()[name]=None
    del name

    def wait_until(self, predicate, message, timeout=25):
        deadline=time.monotonic()+timeout
        while not predicate():
            if time.monotonic()>=deadline:
                status=self.work/'queue/supervisor.json'
                log=self.work/'stderr'
                self.fail(message+'; status='+(status.read_text() if status.exists() else 'missing')+
                    '; stderr='+(log.read_text()[-3000:] if log.exists() else 'missing')+'; server_errors='+repr(self.errors))
            time.sleep(.02)

    def test_real_qq_turn_applies_after_sent_and_second_process_does_not_replay_tools(self):
        queue=self.work/'queue';queue.mkdir()
        storage=self.work/'developer-storage';(storage/'candidates').mkdir(parents=True)
        config=json.loads(self.developer_config.read_text())
        config.update(storage_directory=str(storage),live_upgrade=str(queue))
        self.developer_config.write_text(json.dumps(config))
        source={'head':'0'*40,'diff':'0'*64,'untracked':'0'*64}
        files={name:(ROOT/name).read_bytes() for name in ['AGENT.md','connectors/qqbot/bridge.mjs',
            'connectors/qqbot/bridge-core.mjs','connectors/qqbot/package.json','connectors/qqbot/package-lock.json']}
        resources={name:hashlib.sha256(data).hexdigest() for name,data in files.items()}
        binary=Path(BINARY).read_bytes();binary_sha=hashlib.sha256(binary).hexdigest()
        identity={'source':source,'binary_sha256':binary_sha,'resources':resources}
        candidate_id=hashlib.sha256(json.dumps(identity,sort_keys=True,ensure_ascii=False,separators=(',',':')).encode()).hexdigest()
        candidate=storage/'candidates'/candidate_id;candidate.mkdir()
        target=candidate/'eve-qqbot';target.write_bytes(binary);target.chmod(0o700)
        for name,data in files.items():
            p=candidate/'resources'/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(data)
        runtime.atomic(candidate/'manifest.json',dict(format_version=1,success=True,prepared=True,applied=False,
            candidate_sha256=candidate_id,candidate_path=str(target),binary_sha256=binary_sha,
            source=source,resources=resources,checks=[{'success':True}]*4))
        manifest=runtime.read_json(candidate/'manifest.json')
        (storage/'leases').mkdir()
        runtime.atomic(storage/'leases'/(candidate_id+'.json'),runtime.prepare_lease(candidate,manifest))
        def respond(body):
            results=self.results(body)
            if not results:return tool_response([('apply_self_upgrade',{'candidate_sha256':candidate_id})])
            self.assertIn('queued',results[0],repr(results[0]))
            self.assertTrue(results[0]['queued']);self.assertFalse(results[0]['applied'])
            return text_response('已排队，回复送达后切换')
        self.callback=respond
        scenario=self.work/'scenario.json'
        scenario.write_text(json.dumps({'wait_timeout_ms':30000,'events_file':str(self.work/'events.jsonl'),
            'script':[{'send':{'id':'apply-once','scope':'c2c','target_id':'user-1','user_id':'user-1',
                'text':'应用合成候选','accept_any':True}},
                {'wait_receipt':{'path':str(self.work/'state/state.json'),'id':'apply-once','state':'Sent'}},
                {'wait_file':str(self.work/'never-touched')}]}))
        env={k:os.environ[k] for k in ['PATH','SystemRoot','TEMP','TMP'] if k in os.environ}
        env.update(QQBOT_APP_ID='1904159860',QQBOT_APP_SECRET='test-app-secret',EVE_OPENAI_API_KEY='test-model-secret',
            EVE_OPENAI_BASE_URL=f'http://127.0.0.1:{self.server.server_port}',EVE_OPENAI_PROTOCOL='chat',
            EVE_LLM_RESPONSE_MODE='complete',EVE_DEVELOPER_CONFIG=str(self.developer_config))
        command=[sys.executable,str(ROOT/'scripts/self_upgrade_runtime.py'),'--directory',str(queue),
            '--candidates',str(storage/'candidates'),'--ready-timeout','10','--stop-timeout','10',
            '--quiet-seconds','.1','--health-seconds','.2','--',str(BINARY),
            '--state-dir',str(self.work/'state'),'--agent',str(ROOT/'AGENT.md'),
            '--bridge-script',str(FAKE),'--bridge-arg',str(scenario),'--memory']
        with (self.work/'stdout').open('w') as out,(self.work/'stderr').open('w') as err:
            child=subprocess.Popen(command,env=configure_fixture_progress(env),stdout=out,stderr=err)
            try:
                self.wait_until(lambda:(queue/'supervisor.json').exists() and
                    runtime.read_json(queue/'supervisor.json').get('result')=='applied','upgrade never applied',timeout=25)
                # 新进程收到同一个 message id，保留的 Sent 回执使其只 finish，不再调用模型或工具。
                self.wait_until(lambda:len([json.loads(line) for line in (self.work/'events.jsonl').read_text().splitlines()
                    if json.loads(line).get('type')=='finish'])>=1,'new process did not deduplicate',timeout=10)
                self.assertEqual(self.chat_rounds,2)
                sessions=self.documents()['eve.session']['sessions.v1']['sessions']
                turns=[t for s in sessions.values() for t in s['turns']]
                self.assertEqual(len(turns),1)
                self.assertEqual(turns[0]['status']['state'],'Completed')
                self.assertEqual(self.receipt('apply-once')['state'],'Sent')
                # 监督器先发布 applied，随后才把升级请求归档到 history；等归档完成再核对数量。
                self.wait_until(lambda:(queue/'history').is_dir() and any((queue/'history').glob('*.json')),
                    'upgrade request was not archived',timeout=10)
                self.assertEqual(len(list((queue/'history').glob('*.json'))),1)
                child.send_signal(signal.SIGINT)
                self.assertEqual(child.wait(timeout=15),0)
            finally:
                if child.poll() is None:child.send_signal(signal.SIGINT);child.wait(timeout=15)
        log=(self.work/'stderr').read_text()
        self.assertGreaterEqual(log.count('EVE_QQBOT_READY'),2)
        self.assertNotIn('test-model-secret',log)

if __name__=='__main__':unittest.main()
