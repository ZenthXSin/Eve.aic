"""创建自己的真实子进程，验证升级协议；不连接 QQ 或调用真实模型。"""
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import sysconfig
import tempfile
import time
import unittest

SCRIPT = Path(__file__).with_name('self_upgrade_runtime.py')
spec = importlib.util.spec_from_file_location('upgrade_runtime', SCRIPT)
runtime = importlib.util.module_from_spec(spec); spec.loader.exec_module(runtime)

class DigestTests(unittest.TestCase):
    def test_large_cargo_debug_binary_is_hashed_without_the_old_256_mib_rejection(self):
        with tempfile.TemporaryDirectory(prefix='eve-upgrade-digest-') as directory:
            path=Path(directory)/'debug-binary'
            with path.open('wb') as stream:stream.truncate(256*1024*1024+1)
            self.assertEqual(len(runtime.digest(path)),64)
            with path.open('ab') as stream:stream.truncate(2*1024*1024*1024+1)
            with self.assertRaises(ValueError):runtime.digest(path)

class UpgradeRuntimeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if os.name == 'nt':
            # 只为本测试进程分配独立控制台；信号精确发送到自己创建的进程组。
            console = ctypes.WinDLL('kernel32', use_last_error=True)
            console.FreeConsole()
            if not console.AllocConsole(): raise OSError('cannot allocate test console')
            console.SetConsoleCtrlHandler(None, False)
            cls.console = console

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='eve-upgrade-')
        self.root = Path(self.temporary.name)
        self.queue = self.root / 'queue'; self.queue.mkdir()
        self.candidates = self.root / 'candidates'; self.candidates.mkdir()
        self.state = self.root / 'state'; self.state.mkdir()
        self.events = self.root / 'events'
        self.initial = self.root / 'initial'; self.initial.mkdir()
        self.helper = self.root / 'child.py'
        self.helper.write_text('''import json,signal,sys,time
from pathlib import Path
if '--help' in sys.argv: raise SystemExit(0)
agent=Path(sys.argv[sys.argv.index('--agent')+1]);tag=agent.read_text()
state=Path(sys.argv[sys.argv.index('--state-dir')+1])/'state.json'
events=Path(sys.argv[sys.argv.index('--events')+1])
with events.open('a') as f:f.write(tag+'\\n')
signal.signal(signal.SIGINT,lambda *_:sys.exit(0))
if hasattr(signal,'SIGBREAK'):signal.signal(signal.SIGBREAK,lambda *_:sys.exit(0))
if tag=='fail':
 data=json.loads(state.read_text());data['entries']['synthetic.candidate']={'marker':[1]}
 state.write_text(json.dumps(data));raise SystemExit(2)
print('EVE_QQBOT_READY',file=sys.stderr,flush=True)
if tag=='ready-fail':time.sleep(.02);raise SystemExit(3)
while True:time.sleep(.05)
''', encoding='utf8')
        self.old = self.root / ('old.exe' if os.name == 'nt' else 'old')
        self.old.write_bytes(Path(sys.executable).read_bytes())
        self.old.chmod(0o700)
        for name in ['AGENT.md', 'connectors/qqbot/bridge.mjs', 'connectors/qqbot/bridge-core.mjs',
                     'connectors/qqbot/package.json', 'connectors/qqbot/package-lock.json']:
            p=self.initial/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text('old' if name=='AGENT.md' else '{}')
        (self.initial/'connectors/qqbot/node_modules').mkdir()
        identity=json.dumps(['app','c2c','person','person'],separators=(',',':')).encode()
        self.session='qq:'+hashlib.sha256(identity).hexdigest()
        self.write_state('Completed','Sent')
        self.command=[str(self.old),str(self.helper),'--agent',str(self.initial/'AGENT.md'),
            '--bridge-script',str(self.initial/'connectors/qqbot/bridge.mjs'),'--state-dir',str(self.state),'--events',str(self.events)]
        self.process=None
        self.env=dict(os.environ,PYTHONHOME=sys.base_prefix,PYTHONUTF8='1')
        lib=sysconfig.get_config_var('LIBDIR')
        if lib and os.name!='nt':self.env['LD_LIBRARY_PATH']=lib+os.pathsep+self.env.get('LD_LIBRARY_PATH','')
        self.env['PATH']=str(Path(sys.executable).parent)+os.pathsep+self.env.get('PATH','')

    def tearDown(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name=='nt' else signal.SIGINT)
            try:self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:self.process.kill();self.process.wait()
        if hasattr(self,'log'):self.log.close()
        self.temporary.cleanup()

    def write_state(self,status,receipt):
        turn={'id':1,'input':'升级','status':{'state':status}}
        if status=='Completed':turn['status']['messages']=[{'role':'assistant','text':'已提交升级'}]
        doc={'format_version':1,'sessions':{self.session:{'key':{'user_id':self.session,'session_id':self.session},'turns':[turn]}}}
        ledger={'version':2,'entries':[{'app_id':'app','state':receipt,'reply':'已提交升级',
            'message':{'scope':'c2c','target_id':'person','user_id':'person','text':'升级'}}]}
        state={'version':1,'entries':{'eve.session':{'sessions.v1':list(json.dumps(doc).encode())},
              'eve.channel.qqbot':{'receipts.v1':list(json.dumps(ledger).encode())}}}
        runtime.atomic(self.state/'state.json',state)

    def test_progress_ledger_three_accepts_settled_delivery_but_not_unknown_or_corrupt_progress(self):
        path=self.state/'state.json'
        state=runtime.read_json(path)
        ledger=json.loads(bytes(state['entries']['eve.channel.qqbot']['receipts.v1']))
        ledger['version']=3
        ledger['entries'][0]['progress']=[{'text':'工具回执已返回','state':'Sent'}]
        request={'session_id':self.session,'requested_by':self.session,'turn_id':1}
        def save():
            state['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
            runtime.atomic(path,state)
        save();self.assertTrue(runtime.delivery_gate(path,request))
        ledger['entries'][0]['progress'][0]['state']='Sending'
        save();self.assertFalse(runtime.delivery_gate(path,request))
        ledger['entries'][0]['progress'][0]['state']='Failed'
        save();self.assertTrue(runtime.delivery_gate(path,request))
        ledger['entries'][0]['progress'][0]['state']='invalid'
        save()
        with self.assertRaisesRegex(ValueError,'invalid_progress_ledger'):runtime.delivery_gate(path,request)

    def candidate(self,tag):
        resources={}
        for p in self.initial.rglob('*'):
            if p.is_file():resources[p.relative_to(self.initial).as_posix()]=hashlib.sha256((tag if p.name=='AGENT.md' else p.read_text()).encode()).hexdigest()
        # 尾部附加数据不改变原生解释器执行，却使二进制摘要可区分。
        binary=Path(sys.executable).read_bytes()+tag.encode()
        binary_sha=hashlib.sha256(binary).hexdigest()
        source={'head':'0'*40,'diff':'0'*64,'untracked':'0'*64}
        identity={'source':source,'binary_sha256':binary_sha,'resources':resources}
        key=hashlib.sha256(json.dumps(identity,ensure_ascii=False,sort_keys=True,separators=(',',':')).encode()).hexdigest()
        directory=self.candidates/key;directory.mkdir()
        target=directory/('eve-qqbot.exe' if os.name=='nt' else 'eve-qqbot');target.write_bytes(binary);target.chmod(0o700)
        for name in resources:
            p=directory/'resources'/name;p.parent.mkdir(parents=True,exist_ok=True)
            p.write_text(tag if name=='AGENT.md' else (self.initial/name).read_text())
        manifest=dict(format_version=1,prepared=True,success=True,applied=False,candidate_sha256=key,
            candidate_path=str(target),binary_sha256=binary_sha,source=source,resources=resources,
            checks=[{'success':True}]*4)
        runtime.atomic(directory/'manifest.json',manifest)
        return dict(format_version=1,candidate_sha256=key,candidate_directory=str(directory),
            session_id=self.session,requested_by=self.session,turn_id=1)

    def launch(self):
        self.log=(self.root/'supervisor.log').open('ab')
        self.process=subprocess.Popen([sys.executable,str(SCRIPT),'--directory',str(self.queue),
            '--candidates',str(self.candidates),'--ready-timeout','3','--stop-timeout','3',
            '--drain-timeout','5','--quiet-seconds','.1','--health-seconds','.1','--',*self.command],env=self.env,
            stdout=self.log,stderr=self.log,
            **({'creationflags':subprocess.CREATE_NEW_PROCESS_GROUP} if os.name=='nt' else {}))
        self.wait(lambda d:d['phase']=='running')

    def wait(self,predicate,timeout=10):
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            p=self.queue/'supervisor.json'
            if p.exists():
                d=runtime.read_json(p)
                if predicate(d):return d
            if self.process.poll() is not None:self.fail((self.root/'supervisor.log').read_text(errors='replace'))
            time.sleep(.02)
        self.fail('supervisor timeout: '+(self.root/'supervisor.log').read_text(errors='replace'))

    def test_actual_switch_waits_for_sent_ack_and_keeps_state_and_active_pointer(self):
        self.write_state('Pending','Processing');self.launch();request=self.candidate('new')
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['phase']=='waiting')
        self.assertEqual(self.events.read_text().splitlines(),['old'])
        self.write_state('Completed','Sent');before=(self.state/'state.json').read_bytes()
        d=self.wait(lambda d:d['result']=='applied')
        self.assertEqual(d['candidate_sha256'],request['candidate_sha256'])
        self.assertEqual(self.events.read_text().splitlines(),['old','new'])
        self.assertEqual((self.state/'state.json').read_bytes(),before)
        self.assertTrue((self.queue/'active-command.json').is_file())
        self.assertTrue(list((self.queue/'backups').glob('*/state.json')))

    def test_failed_candidate_rolls_back_binary_without_restoring_old_state(self):
        self.launch();request=self.candidate('fail');before=(self.state/'state.json').read_bytes()
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='rolled_back_binary_state_preserved')
        self.assertEqual(self.events.read_text().splitlines(),['old','fail','old'])
        self.assertIn('synthetic.candidate',runtime.read_json(self.state/'state.json')['entries'])
        self.assertEqual(next((self.queue/'backups').glob('*/state.json')).read_bytes(),before)

    def add_receipt(self, state='Processing', progress=None):
        path=self.state/'state.json';doc=runtime.read_json(path)
        ledger=json.loads(bytes(doc['entries']['eve.channel.qqbot']['receipts.v1']))
        receipt={'app_id':'app','state':state,'message':{'id':f'historical-message-{len(ledger["entries"])}','scope':'c2c',
            'target_id':'other','user_id':'other','text':'旧的未决消息'}}
        if state in ('Sent','ReplyPending'):receipt['reply']='旧的回复记录'
        if progress is not None:
            ledger['version']=3;receipt['progress']=progress
        ledger['entries'].append(receipt)
        doc['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
        runtime.atomic(path,doc)
        return doc,ledger

    def test_inherited_unknown_receipts_do_not_block_and_are_preserved_byte_for_byte(self):
        self.add_receipt('Processing')
        self.add_receipt('ReplyPending')
        self.add_receipt('Sent',[{'text':'旧的发送结果未知','state':'Sending'}])
        before=(self.state/'state.json').read_bytes()
        self.launch();request=self.candidate('new')
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='applied')
        self.assertEqual(self.events.read_text().splitlines(),['old','new'])
        self.assertEqual((self.state/'state.json').read_bytes(),before)

    def test_new_inflight_receipt_still_blocks_after_startup(self):
        self.launch();doc,ledger=self.add_receipt()
        runtime.atomic(self.queue/'request.json',self.candidate('new'))
        self.wait(lambda d:d['phase']=='waiting')
        time.sleep(.2)
        self.assertEqual(self.events.read_text().splitlines(),['old'])
        ledger['entries'][-1]['state']='Failed'
        doc['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
        runtime.atomic(self.state/'state.json',doc)
        self.wait(lambda d:d['result']=='applied')

    def test_changed_inherited_receipt_is_no_longer_exempt(self):
        doc,ledger=self.add_receipt();self.launch()
        ledger['entries'][-1]['message']['text']='当前进程修改后的消息'
        doc['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
        runtime.atomic(self.state/'state.json',doc)
        runtime.atomic(self.queue/'request.json',self.candidate('new'))
        self.wait(lambda d:d['phase']=='waiting');time.sleep(.2)
        self.assertEqual(self.events.read_text().splitlines(),['old'])
        ledger['entries'][-1]['state']='Failed'
        doc['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
        runtime.atomic(self.state/'state.json',doc)
        self.wait(lambda d:d['result']=='applied')

    def test_inherited_unknown_cannot_satisfy_request_delivery_or_hide_corruption(self):
        self.write_state('Completed','Processing');path=self.state/'state.json'
        inherited=runtime.inherited_receipts(path)
        request={'session_id':self.session,'requested_by':self.session,'turn_id':1}
        self.assertFalse(runtime.delivery_gate(path,request,inherited))
        self.write_state('Completed','Sent')
        doc,ledger=self.add_receipt('Sent',[{'text':'坏记录','state':'invalid'}])
        token=runtime.receipt_fingerprint(ledger['entries'][-1])
        with self.assertRaisesRegex(ValueError,'invalid_progress_ledger'):
            runtime.delivery_gate(path,request,frozenset([token]))

    def test_tampered_candidate_never_stops_original_process(self):
        self.launch();request=self.candidate('new')
        (Path(request['candidate_directory'])/'resources/AGENT.md').write_text('changed')
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='rejected')
        self.assertEqual(self.events.read_text().splitlines(),['old'])

    def test_cancelled_request_turn_is_not_applied(self):
        self.launch();self.write_state('Interrupted','Failed');request=self.candidate('new')
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='rejected')
        self.assertEqual(self.events.read_text().splitlines(),['old'])

    def test_stale_pending_request_is_preserved_and_not_replayed_on_restart(self):
        runtime.atomic(self.queue/'request.json',self.candidate('new'));self.launch()
        self.assertFalse((self.queue/'request.json').exists())
        self.assertEqual(len(list((self.queue/'history').iterdir())),1)
        self.assertEqual(self.events.read_text().splitlines(),['old'])

    def test_candidate_that_exits_after_ready_marker_is_rolled_back(self):
        self.launch();request=self.candidate('ready-fail')
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='rolled_back_binary_state_preserved')
        self.assertEqual(self.events.read_text().splitlines(),['old','ready-fail','old'])

    def test_malformed_request_does_not_stop_active_instance(self):
        self.launch();runtime.atomic(self.queue/'request.json',['invalid'])
        self.wait(lambda d:d['result']=='rejected')
        self.assertEqual(self.events.read_text().splitlines(),['old'])

    def test_restart_uses_verified_active_pointer_without_replaying_request(self):
        self.launch();request=self.candidate('new');runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='applied')
        self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name=='nt' else signal.SIGINT)
        self.assertEqual(self.process.wait(timeout=10),0)
        self.log.close();self.launch()
        self.assertEqual(self.events.read_text().splitlines(),['old','new','new'])
        self.assertFalse((self.queue/'request.json').exists())

if __name__=='__main__':unittest.main()
