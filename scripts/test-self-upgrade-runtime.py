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
    def test_state_reader_accepts_extended_receipt_storage_size(self):
        self.assertGreaterEqual(runtime.MAX_STATE_JSON, 256 * 1024 * 1024)
        # 使用空白模拟较大的宿主文档，旧 64 MiB 门禁会拒绝。
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'extended-state.json'
            with path.open('wb') as file:
                file.write(b'{"version":1,"entries":{}}')
                file.write(b' ' * (64 * 1024 * 1024))
            self.assertEqual(runtime.inherited_receipts(path), frozenset())

    def test_large_cargo_debug_binary_is_hashed_without_the_old_256_mib_rejection(self):
        with tempfile.TemporaryDirectory(prefix='eve-upgrade-digest-') as directory:
            path=Path(directory)/'debug-binary'
            with path.open('wb') as stream:stream.truncate(256*1024*1024+1)
            self.assertEqual(len(runtime.digest(path)),64)
            with path.open('ab') as stream:stream.truncate(2*1024*1024*1024+1)
            with self.assertRaises(ValueError):runtime.digest(path)

    def test_removed_json_precheck_flags_are_dropped_with_their_values(self):
        old=['eve','--interest-learning','--practice-mindustry-server','s.jar','--practice-mindustry-json-rules','rules',
             '--practice-python','py','--research-corpus','corpus']
        self.assertEqual(runtime.without_removed_flags(old),
                         ['eve','--interest-learning','--practice-mindustry-server','s.jar','--research-corpus','corpus'])
        self.assertEqual(runtime.without_removed_flags(['eve','--training']),['eve','--training'])

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
        self.helper.write_text('''import json,os,signal,subprocess,sys,time
from pathlib import Path
if '--help' in sys.argv: raise SystemExit(0)
agent=Path(sys.argv[sys.argv.index('--agent')+1]);tag=agent.read_text()
state=Path(sys.argv[sys.argv.index('--state-dir')+1])/'state.json'
events=Path(sys.argv[sys.argv.index('--events')+1])
with events.open('a') as f:f.write(tag+'\\n')
if '--spawn-child' in sys.argv:
 worker=subprocess.Popen([sys.executable,'-c','import time;time.sleep(120)'])
 with (state.parent/'workers').open('a') as f:f.write(str(worker.pid)+'\\n')
signal.signal(signal.SIGINT,lambda *_:sys.exit(0))
if hasattr(signal,'SIGBREAK'):signal.signal(signal.SIGBREAK,lambda *_:sys.exit(0))
if tag=='fail':
 data=json.loads(state.read_text());data['entries']['synthetic.candidate']={'marker':[1]}
 state.write_text(json.dumps(data));raise SystemExit(2)
print('EVE_QQBOT_READY',file=sys.stderr,flush=True)
if tag=='ready-fail':time.sleep(.02);raise SystemExit(3)
launched=time.monotonic()
while True:
 trigger=state.parent/(tag+'.crash')
 if trigger.exists():
  trigger.unlink()
  data=json.loads(state.read_text());data['entries']['synthetic.latest']={'marker':[7]}
  state.write_text(json.dumps(data));raise SystemExit(7)
 if (state.parent/'always-crash').exists() and time.monotonic()-launched>.25:raise SystemExit(7)
 if (state.parent/'corrupt-crash').exists():state.write_text('corrupt');raise SystemExit(7)
 time.sleep(.02)
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

    def candidate(self,tag,same_binary=False):
        resources={}
        for p in self.initial.rglob('*'):
            if p.is_file():resources[p.relative_to(self.initial).as_posix()]=hashlib.sha256((tag if p.name=='AGENT.md' else p.read_text()).encode()).hexdigest()
        # 尾部附加数据不改变原生解释器执行，却使二进制摘要可区分。
        binary=Path(sys.executable).read_bytes()+(b'' if same_binary else tag.encode())
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

    def merged_state(self):
        path=self.state/'state.json';state=runtime.read_json(path)
        ledger=json.loads(bytes(state['entries']['eve.channel.qqbot']['receipts.v1']))
        target=ledger['entries'][0];target['message']['id']='last'
        first=json.loads(json.dumps(target));first['message'].update(id='first',text='前一句')
        first.update(state='Merged',merged_into='last',reply=None)
        ledger.update(version=4,entries=[first,target])
        sessions=json.loads(bytes(state['entries']['eve.session']['sessions.v1']))
        sessions['sessions'][self.session]['turns'][0]['input']='前一句\n升级'
        state['entries']['eve.session']['sessions.v1']=list(json.dumps(sessions).encode())
        state['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
        runtime.atomic(path,state)
        return state,ledger

    def test_format_five_stickers_hold_the_gate_until_sent_and_bad_shapes_are_rejected(self):
        path=self.state/'state.json';state,ledger=self.merged_state()
        request={'session_id':self.session,'requested_by':self.session,'turn_id':1}
        def save():
            state['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
            runtime.atomic(path,state)
        ledger['version']=5;ledger['entries'][1]['sticker']={'path':'/state/stickers/blobs/a.png','state':'Pending'}
        save();self.assertFalse(runtime.delivery_gate(path,request),'表情包未发完时不算空闲')
        ledger['entries'][1]['sticker']['state']='Sent'
        save();self.assertTrue(runtime.delivery_gate(path,request))
        ledger['entries'][1]['sticker']['state']='Sending';save()
        self.assertEqual(len(runtime.inherited_receipts(path)),1,'重启前在途的表情包只作历史记录')
        self.assertTrue(runtime.delivery_gate(path,request,runtime.inherited_receipts(path)) is False)
        ledger['entries'][1]['sticker']={'path':'','state':'Sent'};save()
        with self.assertRaisesRegex(ValueError,'invalid_sticker_ledger'):runtime.delivery_gate(path,request)

    def test_format_six_requires_matching_archive_and_keeps_merge_sticker_gates(self):
        import sqlite3
        path=self.state/'state.json';state,ledger=self.merged_state()
        ledger['version']=6;ledger['archive']={'identity':'a'*32,'records':1}
        ledger['entries'][1]['sticker']={'path':'/state/stickers/blobs/a.png','state':'Sent'}
        state['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
        runtime.atomic(path,state)
        request={'session_id':self.session,'requested_by':self.session,'turn_id':1}
        with self.assertRaisesRegex(ValueError,'receipt_archive_missing'):runtime.delivery_gate(path,request)
        db=sqlite3.connect(self.state/'qq-receipts.sqlite3')
        db.executescript('CREATE TABLE metadata(version INTEGER,identity TEXT); CREATE TABLE receipts(payload BLOB);')
        db.execute('INSERT INTO metadata VALUES (1,?)',('b'*32,));db.execute('INSERT INTO receipts VALUES (?)',(b'cold',));db.commit()
        with self.assertRaisesRegex(ValueError,'receipt_archive_mismatch'):runtime.inherited_receipts(path)
        db.execute('UPDATE metadata SET identity=?',('a'*32,));db.commit()
        self.assertTrue(runtime.delivery_gate(path,request))
        db.execute('DELETE FROM receipts');db.commit();db.close()
        with self.assertRaisesRegex(ValueError,'receipt_archive_mismatch'):runtime.delivery_gate(path,request)

    def test_format_four_merges_match_the_actual_turn_and_bad_links_are_preserved(self):
        path=self.state/'state.json';state,ledger=self.merged_state()
        request={'session_id':self.session,'requested_by':self.session,'turn_id':1}
        self.assertTrue(runtime.delivery_gate(path,request))
        self.assertEqual(runtime.inherited_receipts(path),frozenset())
        ledger['entries'][1]['progress']=[{'text':'执行中','state':'Sending'}]
        state['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
        runtime.atomic(path,state);self.assertFalse(runtime.delivery_gate(path,request))
        ledger['entries'][1]['progress'][0]['state']='Sent'
        ledger['entries'][0]['merged_into']='missing'
        state['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
        runtime.atomic(path,state);before=path.read_bytes()
        with self.assertRaisesRegex(ValueError,'invalid_merged_receipt'):runtime.delivery_gate(path,request)
        with self.assertRaisesRegex(ValueError,'invalid_merged_receipt'):runtime.inherited_receipts(path)
        self.assertEqual(path.read_bytes(),before)

    def test_real_supervisor_accepts_format_four_and_switches_after_combined_turn_delivery(self):
        self.merged_state();before=(self.state/'state.json').read_bytes()
        self.launch();request=self.candidate('new')
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='applied')
        self.assertEqual(self.events.read_text().splitlines(),['old','new'])
        self.assertEqual((self.state/'state.json').read_bytes(),before)

    def launch(self):
        self.log=(self.root/'supervisor.log').open('ab')
        self.process=subprocess.Popen([sys.executable,str(SCRIPT),'--directory',str(self.queue),
            '--candidates',str(self.candidates),'--ready-timeout','3','--stop-timeout','3',
            '--drain-timeout','5','--quiet-seconds','.1','--health-seconds','.1',
            '--restart-delay','.05','--restart-window-seconds','10','--',*self.command],env=self.env,
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

    def test_runtime_crash_restarts_once_and_preserves_new_state(self):
        self.launch()
        (self.state/'old.crash').touch()
        self.wait(lambda d:d['result']=='runtime_restarted_state_preserved' and d['phase']=='running')
        self.assertEqual(self.events.read_text().splitlines(), ['old','old'])
        self.assertIn('synthetic.latest', runtime.read_json(self.state/'state.json')['entries'])
        self.assertEqual(len(runtime.read_json(self.queue/'recovery.json')['crashes']), 1)

    @unittest.skipUnless(sys.platform.startswith('linux'), 'Linux 子进程组清理')
    def test_crashed_runtime_closes_orphan_bridge_before_restart(self):
        self.command.append('--spawn-child')
        self.launch()
        old_worker=int((self.state/'workers').read_text().splitlines()[0])
        (self.state/'old.crash').touch()
        self.wait(lambda d:d['result']=='runtime_restarted_state_preserved' and d['phase']=='running')
        stat=Path(f'/proc/{old_worker}/stat')
        self.assertTrue(not stat.exists() or stat.read_text().rsplit(') ',1)[1].split()[0]=='Z')
        self.assertEqual(len((self.state/'workers').read_text().splitlines()),2)

    def test_runtime_crash_after_committed_upgrade_rolls_back_and_quarantines(self):
        self.launch();request=self.candidate('late-fail')
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='applied')
        (self.state/'late-fail.crash').touch()
        self.wait(lambda d:d['result']=='runtime_rolled_back_binary_state_preserved' and d['phase']=='running')
        self.assertEqual(self.events.read_text().splitlines(), ['old','late-fail','old'])
        self.assertIn('synthetic.latest', runtime.read_json(self.state/'state.json')['entries'])
        self.assertFalse((self.queue/'active-command.json').exists())
        self.assertTrue((self.queue/'quarantine'/ (request['candidate_sha256']+'.json')).is_file())
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='rejected' and d['error']=='candidate_quarantined')
        self.assertEqual(self.events.read_text().splitlines(), ['old','late-fail','old'])

    def test_crash_budget_is_bounded_and_manual_stop_never_restarts(self):
        self.launch();(self.state/'always-crash').touch()
        self.assertEqual(self.process.wait(timeout=10), runtime.RECOVERY_STOP)
        self.assertLessEqual(len(self.events.read_text().splitlines()), 4)
        self.assertEqual(runtime.read_json(self.queue/'recovery.json')['result'], 'restart_limit_exhausted')

    def test_corrupt_state_after_crash_stops_without_clearing_or_restarting(self):
        self.launch();(self.state/'corrupt-crash').touch()
        self.assertEqual(self.process.wait(timeout=10), runtime.RECOVERY_STOP)
        self.assertEqual((self.state/'state.json').read_bytes(), b'corrupt')
        self.assertEqual(self.events.read_text().splitlines(), ['old'])

    def test_changed_previous_resources_cannot_be_used_for_runtime_rollback(self):
        self.launch();request=self.candidate('late-fail');runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='applied')
        (self.initial/'AGENT.md').write_text('changed')
        (self.state/'late-fail.crash').touch()
        self.assertEqual(self.process.wait(timeout=10), runtime.RECOVERY_STOP)
        self.assertEqual(self.events.read_text().splitlines(), ['old','late-fail'])

    def test_resource_only_upgrade_can_roll_back_after_committed_crash(self):
        self.launch();request=self.candidate('resource-fail',same_binary=True)
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='applied')
        (self.state/'resource-fail.crash').touch()
        self.wait(lambda d:d['result']=='runtime_rolled_back_binary_state_preserved' and d['phase']=='running')
        self.assertEqual(self.events.read_text().splitlines(), ['old','resource-fail','old'])
        self.assertTrue((self.queue/'quarantine'/(request['candidate_sha256']+'.json')).is_file())

    def test_explicit_supervisor_stop_does_not_restart_child(self):
        self.launch()
        self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name=='nt' else signal.SIGTERM)
        self.assertEqual(self.process.wait(timeout=10), 0)
        self.assertEqual(self.events.read_text().splitlines(), ['old'])

    @unittest.skipUnless(sys.platform.startswith('linux'), 'Linux ELF loader')
    def test_explicit_linux_loader_keeps_candidate_digest_and_active_pointer(self):
        loader=Path('/lib64/ld-linux-x86-64.so.2')
        if not loader.is_file():self.skipTest('GNU Linux loader unavailable')
        self.env['EVE_RUNTIME_LOADER']=str(loader)
        self.env['EVE_RUNTIME_LIBRARY_PATH']=str(Path(sys.prefix)/'lib')
        self.launch();request=self.candidate('new')
        before=runtime.digest(Path(request['candidate_directory'])/'eve-qqbot')
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='applied')
        pointer=runtime.read_json(self.queue/'active-command.json')
        self.assertEqual(pointer['binary_sha256'],before)
        self.assertEqual(pointer['command'][0],str(Path(request['candidate_directory'])/'eve-qqbot'))

if __name__=='__main__':unittest.main()
