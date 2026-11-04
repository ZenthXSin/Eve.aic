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
from unittest.mock import patch

SCRIPT = Path(__file__).with_name('self_upgrade_runtime.py')
spec = importlib.util.spec_from_file_location('upgrade_runtime', SCRIPT)
runtime = importlib.util.module_from_spec(spec); spec.loader.exec_module(runtime)

class DigestTests(unittest.TestCase):
    def test_hash_and_json_reads_remain_bounded_when_open_file_grows(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'growing'
            path.write_bytes(b'{"value":1}')
            observed = []
            real = os.fstat
            def outdated(fd):
                metadata = real(fd)
                observed.append(fd)
                return SimpleNamespace(st_mode=metadata.st_mode, st_size=3)
            with patch.object(runtime.os, 'fstat', side_effect=outdated):
                with self.assertRaisesRegex(ValueError, 'binary_or_resource_changed_during_read'):
                    runtime.digest(path)
            # metadata initially fits; opening the stream appends bytes before the bounded read.
            path.write_bytes(b'{}')
            original = Path.open
            def growing(current, *args, **kwargs):
                stream = original(current, *args, **kwargs)
                if current == path and args == ('rb',):
                    with original(current, 'ab') as writer: writer.write(b' ' * 30)
                return stream
            with patch.object(Path, 'open', growing), patch.object(runtime.os, 'fstat', side_effect=outdated):
                with self.assertRaisesRegex(ValueError, 'json_file_grew_beyond_limit'):
                    runtime.read_json(path, 10)
            self.assertTrue(observed)

    @unittest.skipIf(os.name == 'nt', 'Windows runner 不保证有文件符号链接权限')
    def test_request_file_symlink_cannot_change_the_manual_request_gate(self):
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            automatic = root / 'auto-request.json'
            automatic.write_text('{}')
            (root / 'request.json').symlink_to(automatic)
            supervisor = SimpleNamespace(stopping=False, root=root)
            with self.assertRaisesRegex(ValueError, 'invalid_json_file'):
                runtime.Supervisor.switch(supervisor, {}, [], {})

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
if tag=='never-ready':raise SystemExit(9)
if tag=='startup-retry' and len(events.read_text().splitlines())<3:raise SystemExit(9)
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

    def test_unknown_and_necessary_progress_remain_readable_without_replay(self):
        path = self.state / 'state.json'
        state = runtime.read_json(path)
        ledger = json.loads(bytes(state['entries']['eve.channel.qqbot']['receipts.v1']))
        ledger['version'] = 8
        ledger['entries'][0]['message']['id'] = 'necessary-progress'
        ledger['entries'][0]['progress'] = [{'text': '必要追问', 'state': 'Unknown',
                                             'proactive': True, 'necessary': True}]
        request = {'session_id': self.session, 'requested_by': self.session, 'turn_id': 1}
        def save():
            state['entries']['eve.channel.qqbot']['receipts.v1'] = list(json.dumps(ledger).encode())
            runtime.atomic(path, state)
        save()
        before = path.read_bytes()
        self.assertTrue(runtime.delivery_gate(path, request))
        self.assertTrue(runtime.runtime_idle_gate(path))
        self.assertEqual(path.read_bytes(), before)
        ledger['entries'][0]['progress'][0]['state'] = 'Sending'
        save()
        self.assertFalse(runtime.runtime_idle_gate(path))
        ledger['entries'][0]['progress'][0]['necessary'] = 'true'
        save()
        with self.assertRaisesRegex(ValueError, 'invalid_progress_ledger'):
            runtime.delivery_gate(path, request)

    def test_proactive_progress_v8_blocks_switch_until_delivery_is_settled(self):
        self.write_state('Completed','Sent');path=self.state/'state.json'
        state=runtime.read_json(path)
        ledger=json.loads(bytes(state['entries']['eve.channel.qqbot']['receipts.v1']))
        ledger['version']=8
        ledger['entries'][0]['message']['id']='stage-v8'
        ledger['entries'][0]['progress']=[{'text':'真实进度','state':'Sent','proactive':True} for _ in range(6)]
        request={'session_id':self.session,'requested_by':self.session,'turn_id':1}
        def save():
            state['entries']['eve.channel.qqbot']['receipts.v1']=list(json.dumps(ledger).encode())
            runtime.atomic(path,state)
        save();self.assertTrue(runtime.delivery_gate(path,request))
        ledger['entries'][0]['progress'][3]['state']='Sending'
        save();self.assertFalse(runtime.delivery_gate(path,request))
        ledger['entries'][0]['progress'][3]['state']='Sent'
        ledger['entries'][0]['progress'][3]['proactive']='true'
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
            session_id=self.session,requested_by=self.session,turn_id=1,
            lease=runtime.prepare_lease(directory, manifest))

    @unittest.skipIf(os.name == 'nt', '需要创建目录符号链接')
    def test_candidate_parent_alias_accepts_same_file_without_rewriting_manifest(self):
        request = self.candidate('alias')
        directory = Path(request['candidate_directory'])
        alias = self.root / 'data-alias'
        alias.symlink_to(self.root, target_is_directory=True)
        manifest = runtime.read_json(directory / 'manifest.json')
        manifest['candidate_path'] = str(alias / directory.relative_to(self.root) / 'eve-qqbot')
        runtime.atomic(directory / 'manifest.json', manifest)
        request['lease'] = runtime.prepare_lease(directory, manifest)
        before = (directory / 'manifest.json').read_bytes()
        runtime.validate_candidate(request, self.candidates)
        self.assertEqual((directory / 'manifest.json').read_bytes(), before)
        self.launch()
        runtime.atomic(self.queue / 'request.json', request)
        self.wait(lambda d: d['result'] == 'applied')
        self.assertEqual(self.events.read_text().splitlines(), ['old', 'alias'])

    @unittest.skipIf(os.name == 'nt', '需要创建文件符号链接')
    def test_candidate_alias_does_not_allow_leaf_symlink_or_other_directory(self):
        request = self.candidate('boundaries')
        directory = Path(request['candidate_directory'])
        manifest = runtime.read_json(directory / 'manifest.json')
        outside = self.root / 'outside'
        outside.mkdir()
        leaf = outside / 'eve-qqbot'
        leaf.write_bytes((directory / 'eve-qqbot').read_bytes())
        manifest['candidate_path'] = str(leaf)
        runtime.atomic(directory / 'manifest.json', manifest)
        with self.assertRaisesRegex(ValueError, 'candidate_binary_changed'):
            runtime.validate_candidate(request, self.candidates)
        binary = directory / 'eve-qqbot'
        binary.unlink()
        binary.symlink_to(leaf)
        manifest['candidate_path'] = str(binary)
        runtime.atomic(directory / 'manifest.json', manifest)
        with self.assertRaisesRegex(ValueError, 'candidate_binary_changed'):
            runtime.validate_candidate(request, self.candidates)

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

    def launch(self,wait_running=True,script=SCRIPT):
        self.log=(self.root/'supervisor.log').open('ab')
        self.process=subprocess.Popen([sys.executable,str(script),'--directory',str(self.queue),
            '--candidates',str(self.candidates),'--ready-timeout','3','--stop-timeout','3',
            '--drain-timeout','5','--quiet-seconds','.1','--health-seconds','.1',
            '--restart-delay','.05','--restart-window-seconds','10','--',*self.command],env=self.env,
            stdout=self.log,stderr=self.log,
            **({'creationflags':subprocess.CREATE_NEW_PROCESS_GROUP} if os.name=='nt' else {}))
        if wait_running:self.wait(lambda d:d['phase']=='running')

    def wait(self,predicate,timeout=10):
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            p=self.queue/'supervisor.json'
            if p.exists():
                d=runtime.read_json(p)
                if predicate(d):return d
            if self.process.poll() is not None:self.fail((self.root/'supervisor.log').read_text(errors='replace'))
            time.sleep(.02)
        self.fail('supervisor timeout; phase/result: '+repr((d.get('phase'), d.get('result'), d.get('error')) if p.exists() else None)+'; '+(self.root/'supervisor.log').read_text(errors='replace'))

    def test_restart_request_restarts_same_child_only_after_delivery_and_records_result(self):
        self.launch()
        before = (self.state / 'state.json').read_bytes()
        request = {'format_version': runtime.VERSION, 'action': 'restart',
                   'session_id': self.session, 'requested_by': self.session, 'turn_id': 1}
        runtime.atomic(self.queue / 'request.json', request)
        self.wait(lambda d: d['result'] == 'restarted')
        self.assertEqual(self.events.read_text().splitlines(), ['old', 'old'])
        self.assertEqual((self.state / 'state.json').read_bytes(), before)
        self.assertFalse((self.queue / 'request.json').exists())
        self.assertEqual(runtime.read_json(self.queue / 'supervisor.json')['restart_supported'], True)

    def test_restart_request_waits_for_exact_turn_delivery(self):
        self.write_state('Pending', 'Processing')
        self.launch()
        request = {'format_version': runtime.VERSION, 'action': 'restart',
                   'session_id': self.session, 'requested_by': self.session, 'turn_id': 1}
        runtime.atomic(self.queue / 'request.json', request)
        self.wait(lambda d: d['phase'] == 'waiting')
        time.sleep(.2)
        self.assertEqual(self.events.read_text().splitlines(), ['old'])
        self.write_state('Completed', 'Sent')
        self.wait(lambda d: d['result'] == 'restarted')
        self.assertEqual(self.events.read_text().splitlines(), ['old', 'old'])

    def test_restart_request_rejects_bad_scope_without_stopping_child(self):
        self.launch()
        request = {'format_version': runtime.VERSION, 'action': 'restart',
                   'session_id': 'wrong', 'requested_by': self.session, 'turn_id': 1}
        runtime.atomic(self.queue / 'request.json', request)
        self.wait(lambda d: d['result'] == 'rejected')
        self.assertEqual(self.events.read_text().splitlines(), ['old'])

    def test_startup_readiness_failure_retries_with_the_same_bounded_recovery_budget(self):
        (self.initial/'AGENT.md').write_text('startup-retry')
        self.launch()
        self.assertEqual(self.events.read_text().splitlines(),['startup-retry']*3)
        history=runtime.read_json(self.queue/'recovery.json')
        self.assertEqual(len(history['crashes']),2)
        self.assertEqual(history['result'],'runtime_restarted_state_preserved')

    def test_repeated_pre_ready_failures_stop_after_restart_budget_without_clearing_state(self):
        (self.initial/'AGENT.md').write_text('never-ready')
        before=(self.state/'state.json').read_bytes()
        self.launch(wait_running=False)
        self.assertNotEqual(self.process.wait(timeout=10),0)
        self.assertEqual(len(self.events.read_text().splitlines()),4)
        self.assertEqual(runtime.read_json(self.queue/'recovery.json')['result'],'restart_limit_exhausted')
        self.assertEqual((self.state/'state.json').read_bytes(),before)

    def test_manual_switch_waits_for_task_and_side_file_delivery_without_state_stamp_change(self):
        self.launch()
        path=self.state/'state.json';state=runtime.read_json(path)
        state['entries']['eve.task']={'tasks.v1':list(json.dumps({'format_version':2,'tasks':[{'id':'task-1','status':'Running'}]}).encode())}
        runtime.atomic(path,state)
        request=self.candidate('new');runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['phase']=='waiting');time.sleep(.3)
        self.assertEqual(self.events.read_text().splitlines(),['old'])
        outbox=self.state/'qq-outbox';outbox.mkdir()
        runtime.atomic(outbox/'ledger.json',{'format_version':1,'records':[{'state':'Sending'}]})
        state['entries'].pop('eve.task');runtime.atomic(path,state)
        time.sleep(.3)
        self.assertEqual(self.events.read_text().splitlines(),['old'])
        stamp=path.stat().st_mtime_ns
        runtime.atomic(outbox/'ledger.json',{'format_version':1,'records':[{'state':'Sent'}]})
        self.wait(lambda d:d['result']=='applied')
        self.assertEqual(path.stat().st_mtime_ns,stamp)
        self.assertEqual(self.events.read_text().splitlines(),['old','new'])
        self.assertFalse((Path(request['candidate_directory'])/'resources/connectors/qqbot/node_modules').exists())

    def cold_task(self, status='Completed', notice='Delivered'):
        task = dict(id='task-1', owner={'session_id': self.session, 'user_id': self.session},
                    source='fixture', spec={'title': '验证升级', 'instructions': '真实隔离验收', 'max_steps': 10,
                    'context': {'owner': {'session_id': self.session, 'user_id': self.session}, 'messages_json': '[]'}},
                    created_at_ms=1, status=status, cancel_requested=False, started_at_ms=2,
                    finished_at_ms=3 if status == 'Completed' else None,
                    usage={'tool_calls': 1, 'model_requests': 1} if status == 'Completed' else None,
                    result='完成' if status == 'Completed' else None, failure=None,
                    notice={'status': notice, 'attempts': [], 'closed_at_ms': 3} if status == 'Completed' else None,
                    execution={'checkpoint': {'private_fixture': True}, 'recoveries': 0,
                               'resume_at_ms': None, 'deadline_at_ms': 100,
                               'review_messages': ['真实用户授权仅保存在完整冷正文']})
        raw = json.dumps(task, ensure_ascii=False).encode()
        directory = json.loads(raw)
        directory['spec'].pop('context')
        directory['result'] = directory['failure'] = None
        directory['execution']['checkpoint'] = None
        directory['execution'].pop('review_messages')
        archive = self.state / 'task-archive'; archive.mkdir(exist_ok=True)
        key = hashlib.sha256(raw).hexdigest()
        cold = archive / (key + '.json'); cold.write_bytes(raw)
        ledger = {'format_version': 4, 'tasks': [{'directory': directory, 'record': {'hash': key, 'bytes': len(raw)}}]}
        self.save_tasks(ledger)
        return cold, ledger

    def save_tasks(self, ledger):
        path = self.state / 'state.json'
        state = runtime.read_json(path)
        state['entries']['eve.task'] = {'tasks.v1': list(json.dumps(ledger).encode())}
        runtime.atomic(path, state)

    def test_cold_tasks_hold_manual_upgrade_and_preserve_complete_archive_after_switch(self):
        self.cold_task('Running')
        self.launch()
        request = self.candidate('new'); runtime.atomic(self.queue / 'request.json', request)
        self.wait(lambda d: d['phase'] == 'waiting')
        time.sleep(.2)
        self.assertEqual(self.events.read_text().splitlines(), ['old'])
        self.cold_task(notice='Delivering')
        time.sleep(.2)
        self.assertEqual(self.events.read_text().splitlines(), ['old'])
        archive, _ = self.cold_task()
        before = (self.state / 'state.json').read_bytes(), archive.read_bytes()
        self.wait(lambda d: d['result'] == 'applied')
        self.assertEqual(self.events.read_text().splitlines(), ['old', 'new'])
        self.assertEqual(((self.state / 'state.json').read_bytes(), archive.read_bytes()), before)
        self.assertEqual(next((self.queue / 'backups').glob('*/task-archive/' + archive.name)).read_bytes(), before[1])

    def test_cold_task_gate_rejects_missing_tampered_directory_unknown_version_and_duplicate_ids(self):
        archive, ledger = self.cold_task()
        path = self.state / 'state.json'
        original = archive.read_bytes()
        self.assertTrue(runtime.runtime_idle_gate(path))
        archive.unlink()
        with self.assertRaisesRegex(ValueError, 'invalid_task_archive_file'): runtime.runtime_idle_gate(path)
        archive.write_bytes(original[:-1] + b' ')
        with self.assertRaisesRegex(ValueError, 'task_archive_digest_mismatch'): runtime.runtime_idle_gate(path)
        archive.write_bytes(original)
        ledger['tasks'][0]['directory']['status'] = 'Running'; self.save_tasks(ledger)
        with self.assertRaisesRegex(ValueError, 'task_archive_directory_mismatch'): runtime.runtime_idle_gate(path)
        ledger['tasks'][0]['directory']['status'] = 'Completed'
        ledger['format_version'] = 5; self.save_tasks(ledger)
        with self.assertRaisesRegex(ValueError, 'unknown_task_ledger_version'): runtime.runtime_idle_gate(path)
        ledger['format_version'] = 4
        ledger['tasks'].append(ledger['tasks'][0]); self.save_tasks(ledger)
        before = path.read_bytes()
        with self.assertRaisesRegex(ValueError, 'invalid_task_ledger_record'): runtime.runtime_idle_gate(path)
        self.assertEqual(path.read_bytes(), before)
        self.assertEqual(archive.read_bytes(), original)

    @unittest.skipIf(os.name == 'nt', 'Windows runner 不保证有文件符号链接权限')
    def test_cold_task_archive_symlink_never_opens_idle_gate(self):
        archive, _ = self.cold_task()
        target = self.root / 'task-body'; archive.rename(target); archive.symlink_to(target)
        with self.assertRaisesRegex(ValueError, 'invalid_task_archive_file'):
            runtime.runtime_idle_gate(self.state / 'state.json')

    def managed_candidate(self, tag='managed'):
        request = self.candidate(tag)
        old = Path(request['candidate_directory'])
        manifest = runtime.read_json(old / 'manifest.json')
        manifest['source'].update(diff=hashlib.sha256(b'').hexdigest(), untracked=hashlib.sha256(b'').hexdigest())
        for name in runtime.RUNTIME_SCRIPTS:
            target = old / 'resources' / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes((SCRIPT.parent / Path(name).name).read_bytes())
            manifest['resources'][name] = runtime.digest(target)
        (old / 'source-diff.patch').write_bytes(b'')
        manifest['source_diff'] = dict(path='source-diff.patch', sha256=hashlib.sha256(b'').hexdigest(),
            base_revision='1' * 40, target_revision=manifest['source']['head'])
        manifest['runtime'] = runtime.RUNTIME_CONTRACT
        identity = {key: manifest[key] for key in ('source', 'binary_sha256', 'resources', 'runtime', 'source_diff')}
        key = runtime.value_digest(identity)
        new = old.with_name(key); old.rename(new)
        manifest.update(candidate_sha256=key, candidate_path=str(new / Path(manifest['candidate_path']).name))
        runtime.atomic(new / 'manifest.json', manifest)
        request.update(candidate_sha256=key, candidate_directory=str(new))
        request['lease'] = runtime.prepare_lease(new, manifest)
        return request

    def test_complete_source_diff_and_runtime_resources_are_bound_and_required_for_handoff(self):
        from types import SimpleNamespace
        request = self.managed_candidate()
        binary, _, manifest = runtime.validate_candidate(request, self.candidates)
        self.assertEqual(runtime.upgrade_plan(manifest)['operation'], 'switch_runtime_preserve_state')
        runtime.validate_lease(request, binary.parent, manifest)
        supervisor = runtime.Supervisor(SimpleNamespace(directory=self.queue, candidates=self.candidates,
            command=self.command))
        try:
            with self.assertRaisesRegex(ValueError, 'runtime_handoff_requires_host_bridge'):
                supervisor.candidate_command(request)
        finally:
            supervisor.lock.close()
        artifact = binary.parent / 'source-diff.patch'; artifact.write_text('unreviewed source')
        with self.assertRaisesRegex(ValueError, 'source_diff_changed'):
            runtime.validate_candidate(request, self.candidates)

    def test_managed_supervisor_hands_off_only_after_delivery_and_never_switches_locally(self):
        from types import SimpleNamespace
        request = self.managed_candidate()
        descriptor = dict(format_version=1, command=self.command, candidate_request=None, source_revision='1' * 40)
        launch = self.queue / 'launch.json'; runtime.atomic(launch, descriptor)
        nonce = 'a' * 64
        supervisor = runtime.Supervisor(SimpleNamespace(directory=self.queue, candidates=self.candidates,
            command=self.command, host_bridge=self.queue, host_nonce=nonce, launch_descriptor=launch,
            ready_timeout=3, stop_timeout=3, health_seconds=.05))
        supervisor.child = runtime.Child(self.command)
        try:
            supervisor.wait_ready(supervisor.child)
            self.write_state('Pending', 'Processing')
            runtime.atomic(self.queue / 'request.json', request)
            command, manifest = supervisor.candidate_command(request)
            with self.assertRaisesRegex(ValueError, 'runtime_not_idle'):
                supervisor.switch(request, command, manifest)
            self.assertIsNone(supervisor.child.process.poll())
            self.write_state('Completed', 'Sent')
            supervisor.switch(request, command, manifest)
            self.assertEqual(supervisor.status['result'], 'handed_off')
            self.assertTrue(supervisor.handoff)
            self.assertTrue(supervisor.stopping)
            self.assertEqual(self.events.read_text().splitlines(), ['old'])
            self.assertFalse((self.queue / 'active-command.json').exists())
            self.assertEqual(runtime.read_json(self.queue / 'handoff-request.json'),
                dict(format_version=1, nonce=nonce, base_candidate_sha256=None, request=request, command=command))
            self.assertTrue((self.queue / 'processing.json').exists())
        finally:
            supervisor.child.stop(3)
            supervisor.lock.close()

    def test_managed_runtime_waits_for_matching_host_commit_before_scheduling(self):
        from types import SimpleNamespace
        supervisor = SimpleNamespace(root=self.queue, host_bridge=self.queue, host_nonce='a' * 64,
                                     status={'active_candidate_sha256': 'b' * 64})
        check = lambda: runtime.Supervisor.host_activation_confirmed(supervisor)
        self.assertFalse(check())
        status = dict(format_version=1, phase='starting', child_pid=os.getpid(),
                      host_nonce=supervisor.host_nonce, candidate_sha256='b' * 64)
        runtime.atomic(self.queue / 'host-status.json', status)
        self.assertFalse(check())
        status['phase'] = 'running'
        runtime.atomic(self.queue / 'host-status.json', status)
        self.assertTrue(check())
        for key, value in [('child_pid', os.getpid() + 1), ('host_nonce', 'c' * 64),
                           ('candidate_sha256', 'd' * 64), ('format_version', True)]:
            runtime.atomic(self.queue / 'host-status.json', dict(status, **{key: value}))
            self.assertFalse(check())

    def test_managed_runtime_rejects_dirty_binary_and_submodule_source_evidence(self):
        request = self.managed_candidate()
        directory = Path(request['candidate_directory'])
        manifest = runtime.read_json(directory / 'manifest.json')
        manifest['source']['diff'] = '0' * 64
        runtime.atomic(directory / 'manifest.json', manifest)
        with self.assertRaisesRegex(ValueError, 'source_diff_requires_clean_committed_source'):
            runtime.validate_candidate(request, self.candidates)
        manifest['source']['diff'] = hashlib.sha256(b'').hexdigest()
        for patch in (b'GIT binary patch\n', b'Binary files a/x and b/x differ\n',
                      b'index 111..222 160000\n', b'new file mode 160000\n'):
            (directory / 'source-diff.patch').write_bytes(patch)
            manifest['source_diff']['sha256'] = hashlib.sha256(patch).hexdigest()
            runtime.atomic(directory / 'manifest.json', manifest)
            with self.assertRaisesRegex(ValueError, 'source_diff_not_reviewable'):
                runtime.validate_candidate(request, self.candidates)

    def test_managed_status_reports_only_committed_matching_host_result_once(self):
        from types import SimpleNamespace
        descriptor = dict(format_version=1, command=self.command, candidate_request=None, source_revision='1' * 40)
        receipt = dict(format_version=1, phase='rolled_back', previous={'descriptor': descriptor},
                       next={'descriptor': dict(descriptor, candidate_request={'candidate_sha256': 'a' * 64})},
                       error='host_managed_runtime_failed', updated_at_ms=1)
        status = {'active_source_revision': '1' * 40}
        supervisor = SimpleNamespace(root=self.queue, host_bridge=self.queue, host_confirmed=False,
                                     command=self.command, active_descriptor=descriptor, status=status,
                                     publish=lambda **changes: status.update(changes))
        runtime.atomic(self.queue / 'host-handoff.json', dict(receipt, phase='trial'))
        with self.assertRaisesRegex(ValueError, 'invalid_host_handoff_receipt'):
            runtime.Supervisor.confirm_host_result(supervisor)
        self.assertNotIn('result', status)
        runtime.atomic(self.queue / 'host-handoff.json', receipt)
        runtime.Supervisor.confirm_host_result(supervisor)
        self.assertEqual(status['result'], 'rolled_back_runtime_state_preserved')
        self.assertEqual(status['candidate_sha256'], 'a' * 64)
        status['result'] = 'restarted'
        runtime.Supervisor.confirm_host_result(supervisor)
        self.assertEqual(status['result'], 'restarted')
        supervisor.host_confirmed = False
        receipt['previous']['descriptor']['source_revision'] = '2' * 40
        runtime.atomic(self.queue / 'host-handoff.json', receipt)
        with self.assertRaisesRegex(ValueError, 'host_handoff_receipt_mismatch'):
            runtime.Supervisor.confirm_host_result(supervisor)

    def test_incomplete_node_dependency_cache_is_rebuilt_without_modifying_candidate(self):
        from types import SimpleNamespace
        request=self.candidate('new')
        original_modules=self.initial/'connectors/qqbot/node_modules'
        (original_modules/'complete-module').write_text('trusted-running-runtime')
        supervisor=SimpleNamespace(root=self.queue,options=SimpleNamespace(candidates=self.candidates),command=self.command)
        before=(Path(request['candidate_directory'])/'manifest.json').read_bytes()
        runtime.Supervisor.candidate_command(supervisor,request)
        cache=self.queue/'runtime-resources'/request['candidate_sha256']
        marker=cache/'dependencies-ready.json';self.assertTrue(marker.exists())
        marker.unlink()
        (cache/'qqbot/node_modules/partial-module').write_text('interrupted-copy')
        runtime.Supervisor.candidate_command(supervisor,request)
        self.assertFalse((cache/'qqbot/node_modules/partial-module').exists())
        self.assertEqual((cache/'qqbot/node_modules/complete-module').read_text(),'trusted-running-runtime')
        self.assertTrue(marker.exists())
        self.assertEqual((Path(request['candidate_directory'])/'manifest.json').read_bytes(),before)
        self.assertFalse((Path(request['candidate_directory'])/'resources/connectors/qqbot/node_modules').exists())

    def test_manual_switch_tracks_group_sqlite_wal_without_main_state_change(self):
        import sqlite3
        connection=sqlite3.connect(self.state/'group-observations.sqlite3')
        try:
            connection.execute('PRAGMA journal_mode=WAL')
            connection.execute('CREATE TABLE outbound(status TEXT)')
            connection.execute("INSERT INTO outbound VALUES ('sending')");connection.commit()
            self.launch();request=self.candidate('new');runtime.atomic(self.queue/'request.json',request)
            self.wait(lambda d:d['phase']=='waiting');time.sleep(.3)
            self.assertEqual(self.events.read_text().splitlines(),['old'])
            stamp=(self.state/'state.json').stat().st_mtime_ns
            connection.execute("UPDATE outbound SET status='sent'");connection.commit()
            self.wait(lambda d:d['result']=='applied')
            self.assertEqual((self.state/'state.json').stat().st_mtime_ns,stamp)
            self.assertEqual(self.events.read_text().splitlines(),['old','new'])
        finally:connection.close()

    def test_authored_speech_handled_turn_needs_no_automatic_final_but_waits_for_inflight_send(self):
        path = self.state / 'state.json'
        state = runtime.read_json(path)
        ledger = json.loads(bytes(state['entries']['eve.channel.qqbot']['receipts.v1']))
        ledger['version'] = 9
        receipt = ledger['entries'][0]
        receipt['message']['id'] = 'authored-input'
        receipt.update(state='Handled', reply=None, control_turn_id=1, progress=[dict(text='Eve 自己决定说的话', state='Sent',
            decision_id='speech-call', platform_message_id='platform-1')])
        def save():
            state['entries']['eve.channel.qqbot']['receipts.v1'] = list(json.dumps(ledger).encode())
            runtime.atomic(path, state)
        save()
        request = self.candidate('new')
        self.assertTrue(runtime.delivery_gate(path, request))
        self.assertTrue(runtime.runtime_idle_gate(path))
        receipt['progress'][0]['state'] = 'Sending'
        receipt['progress'][0].pop('platform_message_id')
        save()
        self.assertFalse(runtime.delivery_gate(path, request))
        inherited = runtime.inherited_receipts(path)
        self.assertEqual(len(inherited), 1)
        self.assertFalse(runtime.delivery_gate(path, request, inherited), '旧未决记录不能证明本次升级轮次已结束')
        receipt['progress'][0]['state'] = 'Unknown'
        save()
        self.assertTrue(runtime.delivery_gate(path, request), '不确定结果已保留，不能为等待成功而重发')
        receipt['progress'] = []
        save()
        self.assertTrue(runtime.delivery_gate(path, request), 'Eve 可选择安静结束，不能强制她发最终一句')
        receipt.pop('control_turn_id')
        save()
        self.assertFalse(runtime.delivery_gate(path, request), '同文宿主命令不能冒充当前对话轮已结束')

    def test_speech_sqlite_wal_and_missing_marker_are_part_of_upgrade_gate(self):
        import sqlite3
        path = self.state / 'speech.sqlite3'
        connection = sqlite3.connect(path)
        try:
            connection.execute('PRAGMA journal_mode=WAL')
            connection.execute('PRAGMA user_version=1')
            connection.execute('CREATE TABLE speech(state TEXT)')
            connection.execute("INSERT INTO speech VALUES ('evaluating')")
            connection.commit()
            before = runtime.drain_stamp(self.state / 'state.json')
            self.assertFalse(runtime.runtime_idle_gate(self.state / 'state.json'))
            connection.execute("UPDATE speech SET state='waiting'")
            connection.commit()
            self.assertNotEqual(before, runtime.drain_stamp(self.state / 'state.json'))
            self.assertTrue(runtime.runtime_idle_gate(self.state / 'state.json'), '持久等待可以跨升级继续')
            connection.execute("UPDATE speech SET state='sending'")
            connection.commit()
            self.assertFalse(runtime.runtime_idle_gate(self.state / 'state.json'))
        finally:
            connection.close()
        path.with_suffix('.required').touch()
        path.unlink()
        with self.assertRaisesRegex(ValueError, 'speech_database_missing'):
            runtime.runtime_idle_gate(self.state / 'state.json')

    def test_legacy_ledger_cannot_smuggle_authored_fields_and_invalid_platform_ids_are_refused(self):
        path = self.state / 'state.json'
        state = runtime.read_json(path)
        ledger = json.loads(bytes(state['entries']['eve.channel.qqbot']['receipts.v1']))
        part = dict(text='明确发言', state='Sent', decision_id='speech-call', platform_message_id='valid-platform')
        ledger['entries'][0]['message']['id'] = 'invalid-authored-input'
        ledger['entries'][0]['progress'] = [part]
        for version, platform in [(8, 'valid-platform'), (9, '\n'), (9, None)]:
            ledger['version'] = version
            part['platform_message_id'] = platform
            state['entries']['eve.channel.qqbot']['receipts.v1'] = list(json.dumps(ledger).encode())
            runtime.atomic(path, state)
            with self.assertRaises(ValueError):
                runtime.delivery_documents(path)

    def test_negative_idle_cache_is_rechecked_even_if_all_file_stamps_stay_equal(self):
        import sqlite3
        wrapper=self.root/'fixed-stamp-supervisor.py'
        wrapper.write_text('import sys\nsys.path.insert(0, '+repr(str(SCRIPT.parent))+')\nimport self_upgrade_runtime as runtime\nruntime.drain_stamp=lambda path: ()\nraise SystemExit(runtime.main())\n')
        connection=sqlite3.connect(self.state/'group-observations.sqlite3')
        try:
            connection.execute('PRAGMA journal_mode=WAL')
            connection.execute('CREATE TABLE outbound(status TEXT)')
            connection.execute("INSERT INTO outbound VALUES ('sending')");connection.commit()
            self.launch(script=wrapper)
            request=self.candidate('new');runtime.atomic(self.queue/'request.json',request)
            self.wait(lambda d:d['phase']=='waiting');time.sleep(.3)
            self.assertEqual(self.events.read_text().splitlines(),['old'])
            connection.execute("UPDATE outbound SET status='sent'");connection.commit()
            self.wait(lambda d:d['result']=='applied')
            self.assertEqual(self.events.read_text().splitlines(),['old','new'])
        finally:connection.close()

    def cold_state(self, pending=False):
        path=self.state/'state.json';state=runtime.read_json(path)
        document=json.loads(bytes(state['entries']['eve.session']['sessions.v1']))
        session=document['sessions'][self.session]
        turn=(next(runtime.session_turns(path,session,2)) if document['format_version']==2 else session['turns'][0])
        if pending:
            turn['status']={'state':'Pending'}
        else:
            turn['status']={'state':'Completed','messages':[{'role':'User','text':turn['input']},{'role':'Assistant','text':'已提交升级'}]}
        raw=json.dumps(turn,ensure_ascii=False).encode();digest=hashlib.sha256(raw).hexdigest()
        directory=self.state/'session-archive';directory.mkdir(exist_ok=True)
        (directory/(digest+'.json')).write_bytes(raw)
        session['turns']=[dict(hash=digest,bytes=len(raw))];session['revision']=1 if pending else 2
        document['format_version']=2
        state['entries']['eve.session']['sessions.v1']=list(json.dumps(document).encode())
        runtime.atomic(path,state)
        return directory/(digest+'.json')

    def test_cold_session_real_supervisor_switches_and_backs_up_exact_original_files(self):
        cold=self.cold_state();before=cold.read_bytes()
        self.launch();request=self.candidate('new');runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['result']=='applied')
        self.assertEqual(self.events.read_text().splitlines(),['old','new'])
        self.assertEqual(cold.read_bytes(),before)
        backups=list((self.queue/'backups').glob('*/session-archive/'+cold.name))
        self.assertEqual(len(backups),1);self.assertEqual(backups[0].read_bytes(),before)

    def test_cold_session_pending_missing_tampered_revision_and_symlink_never_open_gate(self):
        request={'session_id':self.session,'requested_by':self.session,'turn_id':1}
        path=self.state/'state.json'
        self.cold_state(pending=True)
        self.assertFalse(runtime.delivery_gate(path,request));self.assertFalse(runtime.runtime_idle_gate(path))
        cold=self.cold_state();raw=cold.read_bytes();before=path.read_bytes()
        cold.write_bytes(b'corrupt')
        with self.assertRaises(ValueError):runtime.delivery_gate(path,request)
        self.assertEqual(path.read_bytes(),before);cold.write_bytes(raw)
        cold.unlink()
        with self.assertRaises(ValueError):runtime.runtime_idle_gate(path)
        cold.write_bytes(raw)
        if os.name != 'nt':
            target=self.state/'original-turn.json';target.write_bytes(raw)
            cold.unlink();cold.symlink_to(target)
            with self.assertRaises(ValueError):runtime.runtime_idle_gate(path)
            cold.unlink();cold.write_bytes(raw)
        state=runtime.read_json(path);document=json.loads(bytes(state['entries']['eve.session']['sessions.v1']))
        document['sessions'][self.session]['revision']=3
        state['entries']['eve.session']['sessions.v1']=list(json.dumps(document).encode());runtime.atomic(path,state)
        with self.assertRaisesRegex(ValueError,'revision'):runtime.delivery_gate(path,request)
        self.assertEqual(cold.read_bytes(),raw)

    def test_malformed_request_is_rejected_with_minimal_evidence_and_old_process_alive(self):
        self.launch();(self.queue/'request.json').write_text('{corrupt')
        self.wait(lambda d:d['result']=='rejected')
        self.assertEqual(self.events.read_text().splitlines(),['old'])
        self.assertIsNone(runtime.read_json(self.queue/'last-failure.json')['candidate_sha256'])

    def test_lease_missing_changed_expired_are_deterministic_and_do_not_stop_old_process(self):
        self.launch();request=self.candidate('new')
        original=json.loads(json.dumps(request))
        for problem in ('missing','changed','expired'):
            request=json.loads(json.dumps(original))
            if problem=='missing':request.pop('lease')
            elif problem=='changed':request['lease']['validation_sha256']='f'*64
            else:
                request['lease']['created_at_ms']-=runtime.LEASE_SECONDS*1000+1
                request['lease']['expires_at_ms']-=runtime.LEASE_SECONDS*1000+1
            runtime.atomic(self.queue/'request.json',request)
            self.wait(lambda d:d['error']=='prepare_lease_'+problem)
            evidence=runtime.read_json(self.queue/'last-failure.json')
            self.assertEqual(evidence['error'],'prepare_lease_'+problem)
            self.assertNotIn('session_id',evidence)
        self.assertEqual(self.events.read_text().splitlines(),['old'])

    def test_prepare_lease_is_revalidated_after_waiting_for_delivery(self):
        self.write_state('Pending','Processing')
        # 等进入真实排空等待后才推进测试时钟；不依赖机器在 500ms 内完成准备。
        advance = self.root/'advance-clock'
        wrapper = self.root/'lease-clock-supervisor.py'
        wrapper.write_text('import sys\nsys.path.insert(0, '+repr(str(SCRIPT.parent))+')\n'
            'import self_upgrade_runtime as runtime\nfrom pathlib import Path\n'
            'real_time=runtime.time.time\n'
            'runtime.time.time=lambda: real_time() + (runtime.LEASE_SECONDS+1 if Path('
            +repr(str(advance))+').exists() else 0)\nraise SystemExit(runtime.main())\n', encoding='utf8')
        self.launch(script=wrapper)
        request=self.candidate('new')
        runtime.atomic(self.queue/'request.json',request)
        self.wait(lambda d:d['phase']=='waiting')
        advance.touch()
        self.write_state('Completed','Sent')
        self.wait(lambda d:d['error']=='prepare_lease_expired')
        self.assertEqual(self.events.read_text().splitlines(),['old'])

    def test_backup_retention_is_bounded_without_touching_state(self):
        from types import SimpleNamespace
        # 使用真实目录，直接验证清理策略，不启动额外监督器。
        backups=self.queue/'backups';backups.mkdir()
        for number in range(5):
            directory=backups/str(number);directory.mkdir();(directory/'state.json').write_text(str(number))
        before=(self.state/'state.json').read_bytes()
        runtime.Supervisor.prune_backups(SimpleNamespace(root=self.queue))
        self.assertEqual(sorted(p.name for p in backups.iterdir()),['3','4'])
        self.assertEqual((self.state/'state.json').read_bytes(),before)

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
        self.assertTrue((self.queue / 'quarantine' / (request['candidate_sha256'] + '.json')).is_file())
        runtime.atomic(self.queue / 'request.json', request)
        self.wait(lambda d: d['result'] == 'rejected' and d['error'] == 'candidate_quarantined')
        self.assertEqual(self.events.read_text().splitlines(), ['old', 'fail', 'old'])

    def test_switch_commit_failure_restores_runtime_and_durable_pointer(self):
        from types import SimpleNamespace
        options = SimpleNamespace(directory=self.queue, candidates=self.candidates,
            command=self.command, ready_timeout=3, stop_timeout=3, health_seconds=.05)
        supervisor = runtime.Supervisor(options)
        supervisor.child = runtime.Child(self.command)
        try:
            supervisor.wait_ready(supervisor.child)
            initial = supervisor.active_descriptor
            for failure in ('snapshot', 'publish'):
                request = self.candidate('new-' + failure)
                runtime.atomic(self.queue / 'request.json', request)
                command, manifest = supervisor.candidate_command(request)
                if failure == 'snapshot':
                    original = runtime.snapshot_command
                    def broken(command, candidate_request=None):
                        if candidate_request == request: raise ValueError('synthetic_descriptor_failure')
                        return original(command, candidate_request)
                    injected = patch.object(runtime, 'snapshot_command', side_effect=broken)
                else:
                    original = supervisor.publish
                    def broken(**changes):
                        if changes.get('result') == 'applied': raise OSError('synthetic status write failure')
                        return original(**changes)
                    injected = patch.object(supervisor, 'publish', side_effect=broken)
                with injected:
                    supervisor.switch(request, command, manifest)
                self.assertEqual(supervisor.status['result'], 'rolled_back_binary_state_preserved')
                self.assertEqual(supervisor.command, self.command)
                self.assertEqual(supervisor.active_descriptor, initial)
                self.assertEqual(supervisor.status['active_sha256'], initial['binary_sha256'])
                self.assertFalse((self.queue / 'active-command.json').exists())
                self.assertIsNone(supervisor.child.process.poll())
            supervisor.child.stop(3)
        finally:
            supervisor.child.stop(3)
            supervisor.lock.close()
        (self.queue / 'supervisor.json').unlink()
        self.launch()
        self.assertEqual(self.events.read_text().splitlines(), ['old', 'new-snapshot', 'old', 'new-publish', 'old', 'old'])

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

    def test_reapplying_active_candidate_after_supervisor_restart_preserves_rollback_anchor(self):
        self.launch(); request = self.candidate('new')
        runtime.atomic(self.queue / 'request.json', request)
        self.wait(lambda d: d['result'] == 'applied')
        previous = (self.queue / 'previous-command.json').read_bytes()
        self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name == 'nt' else signal.SIGINT)
        self.assertEqual(self.process.wait(timeout=10), 0)
        self.log.close(); self.process = None
        (self.queue / 'supervisor.json').unlink()
        self.launch()
        runtime.atomic(self.queue / 'request.json', request)
        self.wait(lambda d: d['result'] == 'applied')
        self.assertEqual(self.events.read_text().splitlines(), ['old', 'new', 'new'])
        self.assertEqual((self.queue / 'previous-command.json').read_bytes(), previous)
        (self.state / 'new.crash').touch()
        self.wait(lambda d: d['result'] == 'runtime_rolled_back_binary_state_preserved' and d['phase'] == 'running')
        self.assertEqual(self.events.read_text().splitlines(), ['old', 'new', 'new', 'old'])

    def test_failed_second_upgrade_preserves_first_upgrade_previous_version(self):
        self.launch(); first = self.candidate('first')
        runtime.atomic(self.queue / 'request.json', first)
        self.wait(lambda d: d['result'] == 'applied')
        previous = (self.queue / 'previous-command.json').read_bytes()
        second = self.candidate('fail')
        runtime.atomic(self.queue / 'request.json', second)
        self.wait(lambda d: d['result'] == 'rolled_back_binary_state_preserved')
        self.assertEqual((self.queue / 'previous-command.json').read_bytes(), previous)
        (self.state / 'first.crash').touch()
        self.wait(lambda d: d['result'] == 'runtime_rolled_back_binary_state_preserved' and d['phase'] == 'running')
        self.assertEqual(self.events.read_text().splitlines(), ['old', 'first', 'fail', 'first', 'old'])
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
