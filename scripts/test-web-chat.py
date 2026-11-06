"""真实 eve-web/QQ 宿主和本地模型替身；不需要 QQ、Node 或真实密钥。"""
import concurrent.futures
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import unittest
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'connectors/qqbot/test'))
import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response
from memory_test import PANEL_TOKEN
import memory_test

WEB = Path(os.environ.get('EVE_WEB_BINARY', ROOT / 'target/debug' / ('eve-web.exe' if os.name == 'nt' else 'eve-web'))).resolve()
BROWSER = '--browser' in sys.argv
if BROWSER: sys.argv.remove('--browser')


class WebChatAcceptance(developer.DeveloperAcceptance):
    panel = memory_test.MemoryAcceptance.panel
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.child = None
        self.run_number = 0
        self.review_reply = lambda _: {'risk':'low','authorization':'high','outcome':'allow','rationale':'主人明确授权测试工作区操作'}

    def start_web(self):
        self.run_number += 1
        self.log_path = self.work / f'web-{self.run_number}.log'
        self.log = self.log_path.open('w')
        env = {k: os.environ[k] for k in ('PATH','SystemRoot','TEMP','TMP') if k in os.environ}
        env.update(EVE_WEB_TOKEN=PANEL_TOKEN, EVE_OPENAI_API_KEY='test-model-secret',
                   EVE_OPENAI_BASE_URL=f'http://127.0.0.1:{self.server.server_port}',
                   EVE_OPENAI_PROTOCOL='chat', EVE_DEVELOPER_CONFIG=str(self.developer_config),
                   EVE_CONTEXT_ADAPTIVE_LIMIT_TOKENS='0', EVE_CONTEXT_COMPACT_AT_TOKENS='0')
        self.child = subprocess.Popen([str(WEB), '--listen','127.0.0.1:0', '--state-dir',str(self.work/'state'),
                                     '--agent',str(ROOT/'AGENT.md')], env=env, stdout=subprocess.PIPE, stderr=self.log)
        self.wait_web_ready()

    def wait_web_ready(self, timeout=30):
        # stderr 的格式化写入可能拆成多个片段；未换行的端口不能作为就绪地址。
        deadline = time.monotonic()+timeout
        while time.monotonic()<deadline:
            lines=self.log_path.read_text().splitlines(keepends=True)
            ready=next((line.split(' ',1)[1].strip() for line in lines
                        if line.endswith('\n') and line.startswith('EVE_WEB_READY ')),None)
            if self.child.poll() is not None:
                self.fail(f'Web 宿主提前退出 {self.child.returncode}：'+self.log_path.read_text())
            if ready:
                self.assert_startup_complete(lines)
                self.url=ready
                try:
                    status, _ = self.web('/api/chat')
                    self.assertEqual(status, 200, self.log_path.read_text())
                    return
                except (urllib.error.URLError, TimeoutError):
                    pass
            time.sleep(.02)
        self.fail(f'Web 宿主没有就绪，退出码 {self.child.poll()}：'+self.log_path.read_text())

    def startup_events(self, lines):
        events = []
        for line in lines:
            if line.startswith('EVE_WEB_READY '):
                break
            if line.startswith('EVE_WEB_STARTUP ') and line.endswith('\n'):
                event = json.loads(line.split(' ', 1)[1])
                self.assertEqual(set(event), {'stage', 'event', 'elapsed_ms'}, event)
                self.assertIs(type(event['elapsed_ms']), int)
                self.assertGreaterEqual(event['elapsed_ms'], 0)
                events.append((event['stage'], event['event']))
        return events

    def assert_startup_complete(self, lines):
        expected = []
        for stage in ('bootstrap', 'state_store', 'ingress', 'context_plugin'):
            expected.extend([(stage, 'started'), (stage, 'completed')])
        expected.append(('autonomy_install', 'started'))
        for stage in ('resource_discovery', 'browser_runtime'):
            expected.extend([(stage, 'started'), (stage, 'completed')])
        expected.append(('autonomy_install', 'completed'))
        for stage in ('core_services', 'background_start', 'restore_receipts', 'panel_bind', 'activate'):
            expected.extend([(stage, 'started'), (stage, 'completed')])
        self.assertEqual(self.startup_events(lines), expected, self.log_path.read_text())

    def stop_web(self, graceful=False, signum=signal.SIGINT):
        if self.child and self.child.poll() is None:
            if graceful and os.name != 'nt': self.child.send_signal(signum)
            else: self.child.kill()
            self.child.wait(timeout=12)
        if self.child:
            self.child.stdout.close()
            self.child=None
            self.log.close()

    def tearDown(self):
        self.stop_web()
        super().tearDown()

    def web(self,path,body=None,token=PANEL_TOKEN,origin=None):
        headers={'Authorization':'Bearer '+token}
        if origin:headers['Origin']=origin
        encoded=None if body is None else json.dumps(body,ensure_ascii=False).encode()
        if body is not None:headers['Content-Type']='application/json'
        request=urllib.request.Request(self.url+path,data=encoded,headers=headers)
        try:
            with urllib.request.urlopen(request,timeout=8) as response:return response.status,json.load(response)
        except urllib.error.HTTPError as error:return error.code,json.load(error)

    def request(self,text,conversation='a'*32,request='b'*32):
        return {'conversation_id':conversation,'request_id':request,'text':text}

    def finish_web(self,request):
        lookup={key:request[key] for key in ('conversation_id','request_id')}
        deadline=time.monotonic()+12
        while time.monotonic()<deadline:
            status, state=self.web('/api/chat/read',lookup)
            self.assertEqual(status,200,state)
            if state['status'] in ('completed','failed','unknown'):return state
            time.sleep(.03)
        self.fail('Web 对话未结束：'+json.dumps(state,ensure_ascii=False))

    def test_corrupt_state_preserves_last_startup_stage_and_never_reports_ready(self):
        state = self.work / 'state'
        state.mkdir()
        snapshot = state / 'state.json'
        snapshot.write_bytes(b'{broken')
        with self.assertRaisesRegex(AssertionError, 'Web 宿主提前退出'):
            self.start_web()
        lines = self.log_path.read_text().splitlines(keepends=True)
        self.assertEqual(self.startup_events(lines), [
            ('bootstrap', 'started'), ('bootstrap', 'completed'), ('state_store', 'started')])
        self.assertFalse(any(line.startswith('EVE_WEB_READY ') for line in lines))
        self.assertIsNotNone(self.child.poll())
        self.assertNotEqual(self.child.returncode, 0)
        self.assertEqual(snapshot.read_bytes(), b'{broken')
        self.assertEqual(self.chat_rounds, 0)

    def test_owner_tools_duplicate_requests_restart_and_history(self):
        def respond(body):
            self.assertTrue(any('当前用户是主人' in (m.get('content') or '') for m in body['messages']))
            if not self.results(body):
                return tool_response([('run_command',{'program':sys.executable,'args':['-c',
                    "from pathlib import Path; p=Path('web-once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('真实写入回执')"]})])
            self.assertIn('真实写入回执',json.dumps(self.results(body),ensure_ascii=False))
            return text_response('已完成真实工具操作\n第二行也保留')
        self.callback=respond
        self.start_web()
        self.assertEqual(self.web('/api/chat')[1]['role'],'owner')
        request=self.request('请在工作区写入一次文件并核验')
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            replies=list(pool.map(lambda _:self.web('/api/chat/send',request),range(4)))
        self.assertTrue(all(status==202 for status,_ in replies),replies)
        state=self.finish_web(request)
        self.assertEqual(state['status'],'completed',state)
        self.assertEqual(state['text'],'已完成真实工具操作\n第二行也保留')
        self.assertEqual((self.source/'web-once.txt').read_text(),'x')
        self.assertEqual(self.chat_rounds,2)
        self.assertEqual(self.web('/api/chat/send',{**request,'text':'篡改原请求'})[0],409)
        self.stop_web(graceful=True)
        self.start_web()
        self.assertEqual(self.web('/api/chat/send',request)[1]['status'],'completed')
        self.assertEqual(self.chat_rounds,2)
        history=self.web('/api/session',{'key':state['generation']['session'],'limit':10})[1]
        self.assertEqual(history['session']['turns'][0]['status']['state'],'Completed')
        self.assertEqual((self.source/'web-once.txt').read_text(),'x')

    def test_turn_history_and_conversations_are_isolated(self):
        seen=[]
        def respond(body):
            users=[m['content'] for m in body['messages'] if m['role']=='user' and not m['content'].startswith('宿主表达方式：')]
            seen.append(users)
            return text_response('收到：'+users[-1])
        self.callback=respond; self.start_web()
        for i,(conv,content) in enumerate([('a'*32,'私有话题甲'),('a'*32,'继续甲'),('c'*32,'独立话题乙')]):
            request=self.request(content,conv,format(i+1,'032x'))
            self.assertEqual(self.web('/api/chat/send',request)[0],202)
            self.assertEqual(self.finish_web(request)['status'],'completed')
        self.assertIn('私有话题甲',seen[1])
        self.assertEqual(seen[2],['独立话题乙'])

    def test_authentication_origin_identity_and_byte_limits_prevent_execution(self):
        self.start_web(); good=self.request('你好')
        self.assertEqual(self.web('/api/chat/send',good,token='invalid')[0],401)
        self.assertEqual(self.web('/api/chat/send',good,origin='https://other.example')[0],403)
        for invalid in [{**good,'user_id':'owner'},{**good,'role':'owner'}, {**good,'conversation_id':'qq:other'},
                        {**good,'text':'文'*12000}, {**good,'text':' \n'}]:
            self.assertEqual(self.web('/api/chat/send',invalid)[0],400)
        self.assertEqual(self.chat_rounds,0)
        self.assertFalse(list((self.work/'state/web-chat-requests').glob('*.json')))

    def test_cancel_and_crash_never_automatically_resubmit(self):
        self.response_gates[('chat',1)]='hold-model'
        self.start_web(); request=self.request('等待并测试取消')
        self.assertEqual(self.web('/api/chat/send',request)[0],202)
        self.wait_until(lambda:len(self.kind('chat'))==1,'模型未开始')
        lookup=request.copy();lookup.pop('text')
        self.assertEqual(self.web('/api/chat/cancel',lookup)[0],200)
        state=self.finish_web(request)
        self.assertEqual(state['status'],'failed',state)
        self.gate('hold-model').touch()
        self.wait_until(lambda:self.chat_rounds==1,'取消后的本地模型替身未收尾')
        self.assertEqual(self.web('/api/chat/send',request)[1]['status'],'failed')
        self.assertEqual(self.chat_rounds,1)
        self.response_gates[('chat',2)]='hold-model-again'
        request=self.request('重启时仍在执行',request='c'*32)
        self.assertEqual(self.web('/api/chat/send',request)[0],202)
        self.wait_until(lambda:len(self.kind('chat'))==2,'第二轮模型未开始')
        self.stop_web();self.gate('hold-model-again').touch();self.start_web()
        self.assertEqual(self.web('/api/chat/send',request)[1]['status'],'unknown')
        self.assertEqual(self.chat_rounds,2)

    def test_corrupt_request_receipt_is_preserved_and_never_replayed(self):
        self.start_web();request=self.request('保存一次回复')
        self.assertEqual(self.web('/api/chat/send',request)[0],202)
        self.assertEqual(self.finish_web(request)['status'],'completed')
        path=next((self.work/'state/web-chat-requests').glob('*.json'))
        path.write_bytes(b'{broken')
        self.assertEqual(self.web('/api/chat/send',request)[0],503)
        self.assertEqual(path.read_bytes(),b'{broken')
        self.assertEqual(self.chat_rounds,1)

    def test_missing_claimed_receipt_is_not_treated_as_new_work(self):
        self.start_web();request=self.request('仅执行一次')
        self.assertEqual(self.web('/api/chat/send',request)[0],202)
        self.assertEqual(self.finish_web(request)['status'],'completed')
        path=next((self.work/'state/web-chat-requests').glob('*.json'))
        path.unlink()
        self.assertEqual(self.web('/api/chat/send',request)[0],503)
        self.assertEqual(self.chat_rounds,1)

    def test_completion_is_saved_without_polling_and_live_report_is_released(self):
        self.start_web();request=self.request('完成后不轮询')
        self.assertEqual(self.web('/api/chat/send',request)[0],202)
        path=next((self.work/'state/web-chat-requests').glob('*.json'))
        self.wait_until(lambda:json.loads(path.read_text())['state']['status']=='completed','浏览器没有轮询时结果未保存')
        self.wait_until(lambda:self.web('/api/tasks')[1]['tasks']==[],'完成后的完整报告仍留在控制器内存')
        self.stop_web();self.start_web()
        self.assertEqual(self.web('/api/chat/send',request)[1]['status'],'completed')
        self.assertEqual(self.chat_rounds,1)

    @unittest.skipIf(os.name=='nt','SIGINT 由 Linux 进程验收；Windows 覆盖取消与重启')
    def test_graceful_shutdown_cancels_actual_running_tool_before_kernel_stop(self):
        self.callback=lambda body:tool_response([('run_command',{'program':sys.executable,'args':['-c',
            "from pathlib import Path; import time; Path('shutdown-once.txt').write_text('x'); time.sleep(30)"]})])
        self.start_web();request=self.request('启动本地核验并测试退出收尾')
        self.assertEqual(self.web('/api/chat/send',request)[0],202)
        self.wait_until(lambda:(self.source/'shutdown-once.txt').exists(),'工具没有进入实际运行')
        self.stop_web(graceful=True, signum=signal.SIGTERM)
        receipt=json.loads(next((self.work/'state/web-chat-requests').glob('*.json')).read_text())
        self.assertEqual(receipt['state']['status'],'failed',receipt['state'])
        self.assertEqual(receipt['state']['started_tools'],1)
        self.assertEqual((self.source/'shutdown-once.txt').read_text(),'x')

    def test_existing_qq_panel_also_runs_an_independent_web_owner_conversation(self):
        self.callback=lambda body:text_response('Web 里的主人已确认')
        def chat():
            request=self.request('来自 Web 的聊天')
            status,receipt=self.panel('/api/chat/send',request)
            self.assertEqual(status,202,receipt)
            deadline=time.monotonic()+10
            while time.monotonic()<deadline:
                status,state=self.panel('/api/chat/read',{k:request[k] for k in ('conversation_id','request_id')})
                if state.get('status') in ('completed','failed','unknown'):break
                time.sleep(.03)
            self.assertEqual(state['status'],'completed',state)
            self.assertEqual(state['text'],'Web 里的主人已确认')
        self.run_eve(self.checkpoint('web-chat'),interest=False,memory=True,research=False,practice=False,panel=True,
                     env_extra={'EVE_DEVELOPER_CONFIG':str(self.developer_config)},checkpoints={'web-chat':chat})

    @unittest.skipUnless(BROWSER, '显式 --browser 运行真实浏览器验收')
    def test_browser_conversation_reload_cancellation_and_mobile(self):
        def respond(body):
            latest=next(m['content'] for m in reversed(body['messages']) if m['role']=='user')
            if latest=='等待取消':time.sleep(.8)
            return text_response('网页回复：'+latest)
        self.callback=respond;self.start_web()
        env=os.environ.copy()
        env.update(EVE_WEB_TEST_URL=self.url,EVE_WEB_TEST_TOKEN=PANEL_TOKEN)
        completed=subprocess.run(['node',str(ROOT/'scripts/test-web-chat-browser.mjs')],env=env,capture_output=True,text=True,timeout=45)
        self.assertEqual(completed.returncode,0,completed.stdout+completed.stderr)
        self.assertNotIn(PANEL_TOKEN,completed.stdout+completed.stderr)


if __name__=='__main__':
    unittest.main()
