"""真实 Eve→工具请求→监督器→第二个 Eve 进程，QQ 与模型均使用本地替身。"""
from fixture_auth import configure_fixture_progress
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
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

@unittest.skipUnless(sys.platform.startswith('linux'),'完整运行包交接需要 Linux 固定宿主桥')
class LiveUpgradeAcceptance(developer.DeveloperAcceptance):
    for name in dir(developer.DeveloperAcceptance):
        if name.startswith('test_'):locals()[name]=None
    del name

    def lifecycle(self):
        fields = {
            'supervisor.json': ('phase', 'pid', 'active_candidate_sha256', 'active_source_revision', 'result', 'error'),
            'host-status.json': ('phase', 'pid', 'child_pid', 'candidate_sha256', 'trial_candidate_sha256', 'result', 'error'),
            'host-handoff.json': ('phase', 'error'),
            'last-failure.json': ('phase', 'error', 'candidate_sha256'),
        }
        result = {}
        for name, keys in fields.items():
            path = self.work / 'queue' / name
            if not path.exists():
                continue
            try:
                record = runtime.read_json(path)
            except FileNotFoundError:
                continue
            except (OSError, ValueError) as error:
                result[name] = {'read_error': type(error).__name__}
            else:
                result[name] = ({key: record[key] for key in keys if key in record}
                    if isinstance(record, dict) else {'read_error': 'invalid_status_shape'})
        return result

    def wait_until(self, predicate, message, timeout=25):
        started=time.monotonic()
        deadline=started+timeout
        previous=None
        next_snapshot=started
        while not predicate():
            now=time.monotonic()
            if now>=next_snapshot:
                current=self.lifecycle()
                if current!=previous:
                    print('EVE_UPGRADE_TEST '+json.dumps({'waiting_for':message,
                        'elapsed_ms':round((now-started)*1000),'lifecycle':current},sort_keys=True),
                        file=sys.stderr,flush=True)
                    previous=current
                next_snapshot=now+.1
            if now>=deadline:
                log=self.work/'stderr'
                self.fail(message+'; lifecycle='+json.dumps(self.lifecycle(),sort_keys=True)+
                    '; stderr='+(log.read_text()[-3000:] if log.exists() else 'missing')+'; server_errors='+repr(self.errors))
            time.sleep(.02)
        print('EVE_UPGRADE_TEST '+json.dumps({'completed':message,
            'elapsed_ms':round((time.monotonic()-started)*1000),'lifecycle':self.lifecycle()},sort_keys=True),
            file=sys.stderr,flush=True)

    def test_real_qq_turn_restarts_same_version_after_sent_without_replay(self):
        queue=self.work/'queue';queue.mkdir()
        storage=self.work/'developer-storage';(storage/'candidates').mkdir(parents=True)
        config=json.loads(self.developer_config.read_text())
        config.update(storage_directory=str(storage),live_upgrade=str(queue))
        self.developer_config.write_text(json.dumps(config))
        reviews=[]
        def review(request):
            reviews.append(request)
            allowed=request['user_messages']==[] and request['program']=='eve.restart_self'
            return {'risk':'medium','authorization':'unknown',
                    'outcome':'allow' if allowed else 'deny','rationale':'受监督器保护的短暂重启不造成状态丢失或持续不可用'}
        self.review_reply=review
        def respond(body):
            results=self.results(body)
            if not results:return tool_response([('restart_self',{})])
            self.assertTrue(results[0]['queued'],repr(results[0]))
            self.assertFalse(results[0]['restarted'])
            if len(results)==1:
                response=tool_response([('send_message',{'text':'这轮说完就重启。'})])
                response['choices'][0]['message']['tool_calls'][0]['id']='restart-expression'
                return response
            self.assertTrue(results[1]['sent'])
            return text_response('内部处理结束。')
        self.callback=respond
        scenario=self.work/'scenario.json'
        scenario.write_text(json.dumps({'wait_timeout_ms':30000,'events_file':str(self.work/'events.jsonl'),
            'script':[{'send':{**self.message('restart-once','重启一次'),'expected_type':'finish'}},
                {'wait_receipt':{'path':str(self.work/'state/state.json'),'id':'restart-once','state':'Handled'}},
                {'wait_file':str(self.work/'never-touched')}]}))
        env={k:os.environ[k] for k in ['PATH','SystemRoot','TEMP','TMP'] if k in os.environ}
        env.update(EVE_QQ_LIVE_INTERACTION='true',QQBOT_APP_ID='1904159860',QQBOT_APP_SECRET='test-app-secret',EVE_OPENAI_API_KEY='test-model-secret',
            EVE_OPENAI_BASE_URL=f'http://127.0.0.1:{self.server.server_port}',EVE_OPENAI_PROTOCOL='chat',
            EVE_LLM_RESPONSE_MODE='complete',EVE_DEVELOPER_CONFIG=str(self.developer_config))
        # 只验证真实回执后的交接；显式隔离组合避免默认浏览器探测占用就绪期限。
        # 开发工具权限来自本组配置，记忆与完整候选校验仍由真实宿主执行。
        command=[sys.executable,str(ROOT/'scripts/self_upgrade_runtime.py'),'--directory',str(queue),
            '--candidates',str(storage/'candidates'),'--ready-timeout','10','--stop-timeout','10',
            '--quiet-seconds','.1','--health-seconds','.2','--',str(BINARY),'--minimal-profile',
            '--state-dir',str(self.work/'state'),'--agent',str(ROOT/'AGENT.md'),
            '--bridge-script',str(FAKE),'--bridge-arg',str(scenario),'--memory']
        with (self.work/'stdout').open('w') as out,(self.work/'stderr').open('w') as err:
            child=subprocess.Popen(command,env=configure_fixture_progress(env),stdout=out,stderr=err)
            try:
                self.wait_until(lambda:(queue/'supervisor.json').exists() and
                    runtime.read_json(queue/'supervisor.json').get('result')=='restarted','restart never completed',timeout=25)
                self.wait_until(lambda:len([json.loads(line) for line in (self.work/'events.jsonl').read_text().splitlines()
                    if json.loads(line).get('type')=='finish'])>=1,'new process did not deduplicate',timeout=10)
                self.assertEqual(self.chat_rounds,3)
                self.assertEqual(len(reviews),1)
                self.assertFalse(reviews[0]['delegated'])
                turns=[t for s in self.documents()['eve.session']['sessions.v1']['sessions'].values() for t in s['turns']]
                self.assertEqual(len(turns),1)
                self.assertEqual(turns[0]['status']['state'],'Completed')
                receipt=self.receipt('restart-once')
                self.assertEqual(receipt['state'],'Handled')
                self.assertEqual(receipt['progress'][0]['state'],'Sent')
                self.assertTrue(receipt['progress'][0]['platform_message_id'])
                # restarted 先于请求归档发布；等待归档完成后再核对唯一性。
                self.wait_until(lambda:any((queue/'history').glob('*-restarted.json')),
                    'restart request was not archived',timeout=10)
                self.assertEqual(len(list((queue/'history').glob('*-restarted.json'))),1)
                child.send_signal(signal.SIGINT)
                self.assertEqual(child.wait(timeout=15),0)
            finally:
                if child.poll() is None:child.send_signal(signal.SIGINT);child.wait(timeout=15)
        log=(self.work/'stderr').read_text()
        self.assertGreaterEqual(log.count('EVE_QQBOT_READY'),2)
        self.assertNotIn('test-model-secret',log)

    def test_real_qq_turn_applies_after_sent_and_second_process_does_not_replay_tools(self):
        queue=self.work/'queue';queue.mkdir()
        storage=self.work/'developer-storage';(storage/'candidates').mkdir(parents=True)
        config=json.loads(self.developer_config.read_text())
        config.update(storage_directory=str(storage),live_upgrade=str(queue))
        self.developer_config.write_text(json.dumps(config))
        source={'head':'2'*40,'diff':hashlib.sha256(b'').hexdigest(),'untracked':hashlib.sha256(b'').hexdigest()}
        files={name:(ROOT/name).read_bytes() for name in ['AGENT.md','connectors/qqbot/bridge.mjs',
            'connectors/qqbot/bridge-core.mjs','connectors/qqbot/package.json','connectors/qqbot/package-lock.json',
            *runtime.RUNTIME_SCRIPTS]}
        resources={name:hashlib.sha256(data).hexdigest() for name,data in files.items()}
        binary=Path(BINARY);binary_sha=runtime.digest(binary)
        print('EVE_UPGRADE_TEST '+json.dumps({'binary_bytes':binary.stat().st_size,'binary_sha256':binary_sha}),
            file=sys.stderr,flush=True)
        patch=b'diff --git a/AGENT.md b/AGENT.md\n--- a/AGENT.md\n+++ b/AGENT.md\n@@ -1 +1 @@\n-old fixture\n+new fixture\n'
        source_diff=dict(path='source-diff.patch',sha256=hashlib.sha256(patch).hexdigest(),
                         base_revision='1'*40,target_revision=source['head'])
        identity={'source':source,'binary_sha256':binary_sha,'resources':resources,
                  'source_diff':source_diff,'runtime':runtime.RUNTIME_CONTRACT}
        candidate_id=hashlib.sha256(json.dumps(identity,sort_keys=True,ensure_ascii=False,separators=(',',':')).encode()).hexdigest()
        candidate=storage/'candidates'/candidate_id;candidate.mkdir()
        target=candidate/'eve-qqbot';shutil.copyfile(binary,target);target.chmod(0o700)
        (candidate/'source-diff.patch').write_bytes(patch)
        for name,data in files.items():
            p=candidate/'resources'/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(data)
        runtime.atomic(candidate/'manifest.json',dict(format_version=1,success=True,prepared=True,applied=False,
            candidate_sha256=candidate_id,candidate_path=str(target),binary_sha256=binary_sha,
            source=source,resources=resources,checks=[{'success':True}]*4,
            source_diff=source_diff,runtime=runtime.RUNTIME_CONTRACT))
        manifest=runtime.read_json(candidate/'manifest.json')
        (storage/'leases').mkdir()
        runtime.atomic(storage/'leases'/(candidate_id+'.json'),runtime.prepare_lease(candidate,manifest))
        reviews=[]
        def review(request):
            reviews.append(request)
            operation=json.loads(request['arguments'][0])
            allowed=(request['user_messages']==[] and request['program']=='eve.apply_self_upgrade'
                     and operation.get('source_diff_content')==patch.decode())
            return {'risk':'medium','authorization':'unknown',
                    'outcome':'allow' if allowed else 'deny','rationale':'依据完整冻结源码差异核对实际系统影响，不要求逐步授权'}
        self.review_reply=review
        def respond(body):
            results=self.results(body)
            if not results:return tool_response([('apply_self_upgrade',{'candidate_sha256':candidate_id})])
            self.assertIn('queued',results[0],repr(results[0]))
            self.assertTrue(results[0]['queued']);self.assertFalse(results[0]['applied'])
            if len(results)==1:
                response=tool_response([('send_message',{'text':'这轮说完就切换。'})])
                response['choices'][0]['message']['tool_calls'][0]['id']='upgrade-expression'
                return response
            self.assertTrue(results[1]['sent'])
            return text_response('内部处理结束。')
        self.callback=respond
        scenario=self.work/'scenario.json'
        scenario.write_text(json.dumps({'wait_timeout_ms':30000,'events_file':str(self.work/'events.jsonl'),
            'script':[{'send':{**self.message('apply-once','应用合成候选'),'expected_type':'finish'}},
                {'wait_receipt':{'path':str(self.work/'state/state.json'),'id':'apply-once','state':'Handled'}},
                {'wait_file':str(self.work/'never-touched')}]}))
        env={k:os.environ[k] for k in ['PATH','SystemRoot','TEMP','TMP'] if k in os.environ}
        env.update(EVE_QQ_LIVE_INTERACTION='true',QQBOT_APP_ID='1904159860',QQBOT_APP_SECRET='test-app-secret',EVE_OPENAI_API_KEY='test-model-secret',
            EVE_OPENAI_BASE_URL=f'http://127.0.0.1:{self.server.server_port}',EVE_OPENAI_PROTOCOL='chat',
            EVE_LLM_RESPONSE_MODE='complete',EVE_DEVELOPER_CONFIG=str(self.developer_config))
        # 与同版本重启保持相同显式组合，候选仍接续全部工具与平台回执。
        command=[sys.executable,'-I','-B',str(ROOT/'scripts/host_runtime_bridge.py'),
            '--supervisor',str(ROOT/'scripts/self_upgrade_runtime.py'),'--source-revision','1'*40,'--directory',str(queue),
            '--candidates',str(storage/'candidates'),'--ready-timeout','10','--stop-timeout','10',
            '--quiet-seconds','.1','--health-seconds','.2','--',str(BINARY),'--minimal-profile',
            '--state-dir',str(self.work/'state'),'--agent',str(ROOT/'AGENT.md'),
            '--bridge-script',str(FAKE),'--bridge-arg',str(scenario),'--memory']
        with (self.work/'stdout').open('w') as out,(self.work/'stderr').open('w') as err:
            child=subprocess.Popen(command,env=configure_fixture_progress(env),stdout=out,stderr=err)
            try:
                self.wait_until(lambda:(queue/'supervisor.json').exists() and
                    runtime.read_json(queue/'supervisor.json').get('active_candidate_sha256')==candidate_id and
                    (queue/'host-handoff.json').exists() and runtime.read_json(queue/'host-handoff.json').get('phase')=='committed',
                    'upgrade never committed',timeout=35)
                # 新进程收到同一个 message id，只 finish，不再调用模型、工具或补发。
                self.wait_until(lambda:len([json.loads(line) for line in (self.work/'events.jsonl').read_text().splitlines()
                    if json.loads(line).get('type')=='finish'])>=1,'new process did not deduplicate',timeout=10)
                self.assertEqual(self.chat_rounds,3)
                self.assertEqual(len(reviews),1)
                self.assertFalse(reviews[0]['delegated'])
                self.assertEqual(runtime.read_json(queue/'host-active.json')['descriptor']['candidate_request']['candidate_sha256'],candidate_id)
                self.assertEqual(runtime.read_json(queue/'host-handoff.json')['phase'],'committed')
                status=runtime.read_json(queue/'supervisor.json')
                self.assertEqual(status['active_candidate_sha256'],candidate_id)
                self.assertEqual(status['active_source_revision'],source['head'])
                sessions=self.documents()['eve.session']['sessions.v1']['sessions']
                turns=[t for s in sessions.values() for t in s['turns']]
                self.assertEqual(len(turns),1)
                self.assertEqual(turns[0]['status']['state'],'Completed')
                receipt=self.receipt('apply-once')
                self.assertEqual(receipt['state'],'Handled')
                self.assertEqual(receipt['progress'][0]['state'],'Sent')
                self.assertTrue(receipt['progress'][0]['platform_message_id'])
                # 监督器先发布 applied，随后才把升级请求归档到 history；等归档完成再核对数量。
                self.wait_until(lambda:(queue/'history').is_dir() and any((queue/'history').glob('*.json')),
                    'upgrade request was not archived',timeout=10)
                self.assertEqual(len(list((queue/'history').glob('*-host-applied-processing.json'))),1)
                child.send_signal(signal.SIGINT)
                self.assertEqual(child.wait(timeout=15),0)
            finally:
                if child.poll() is None:child.send_signal(signal.SIGINT);child.wait(timeout=15)
        log=(self.work/'stderr').read_text()
        self.assertGreaterEqual(log.count('EVE_QQBOT_READY'),2)
        self.assertNotIn('test-model-secret',log)

if __name__=='__main__':unittest.main()
