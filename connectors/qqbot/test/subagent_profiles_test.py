"""实际宿主：自定义/编码/只读子代理、后台派发与共享总步数；只使用本地替身。"""
from contextlib import closing
import hashlib
import json
import sqlite3
import sys
import threading
import unittest
import developer_tools_test as developer
from background_task_test import calls, WORKER
from dialogue_tools_test import text_response

FORBIDDEN = {"run_subagent", "dispatch_background_task", "create_subagent", "list_subagents", "delete_subagent", "set_wakeup_trigger"}

class SubagentProfilesAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'): locals()[_name] = None
    del _name

    def enabled(self, script, **options):
        env = {"EVE_DEVELOPER_CONFIG": str(self.developer_config)}
        env.update(options.pop("env_extra", {}))
        return self.run_eve(script, interest=False, memory=True, research=False, practice=False,
                            more=["--background-tasks", "--subagents"],
                            env_extra=env, **options)

    def text(self, body):
        return next(m['content'] for m in reversed(body['messages']) if m['role'] == 'user')

    def leaf(self, body):
        return "你是 Eve 的子代理" in body['messages'][0]['content']

    def worker(self, body):
        return WORKER in body['messages'][0]['content']

    def persisted_run_rows(self):
        path = self.work / 'state/subagent-runs.sqlite3'
        self.assertTrue(path.is_file(), '宿主没有创建子代理持久执行库')
        with closing(sqlite3.connect(f'file:{path}?mode=ro', uri=True)) as db:
            self.assertEqual(db.execute('PRAGMA quick_check').fetchone()[0], 'ok')
            return db.execute('SELECT r.id,r.frozen,s.state FROM runs r JOIN run_states s ON r.id=s.id ORDER BY r.seq').fetchall()

    def test_persistent_run_receipts_and_saved_unsettled_result_are_readonly_after_restart(self):
        config = json.loads(self.developer_config.read_text())
        identity = json.dumps(['1904159860', 'c2c', 'user-2', 'user-2'], separators=(',', ':')).encode()
        config['operators'].append('qq:' + hashlib.sha256(identity).hexdigest())
        self.developer_config.write_text(json.dumps(config))
        content = 'xy'
        expected = hashlib.sha256(content.encode()).hexdigest()
        original = '授权在临时工作区执行一次写入，拒绝陈旧哈希后继续安全编辑并核验，之后只读检查子代理持久记录'
        command = {'program': sys.executable, 'args': ['-c',
            "from pathlib import Path; p=Path('persistent-once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('PERSISTED_ONCE_RECEIPT')"]}
        contract = {'kind': 'receipts', 'contract': {'checks': [{
            'tool': 'read_file', 'arguments': {'path': 'persistent-once.txt'},
            'output_pointer': '/sha256', 'expected': expected}], 'require_final_batch': True}}
        run_id = []
        attached_evidence = []
        query_results = []
        def assert_evidence(evidence):
            self.assertFalse(evidence['automatic_resume'])
            self.assertEqual(len(evidence['runs']), 1)
            row = evidence['runs'][0]
            self.assertEqual(row['run_id'], run_id[0])
            self.assertEqual(row['evidence_kind'], 'historical_tool_receipts')
            self.assertEqual(row['current_artifact_validity'], 'not_checked')
            self.assertFalse(row['automatic_resume'])
            self.assertFalse(row['effects_unknown'])
            self.assertEqual(row['report']['status'], 'historical_match')
            [check] = row['report']['checks']
            self.assertEqual(check['check_index'], 0)
            self.assertEqual(check['source']['call_id'], 'persistent-read')
            self.assertEqual(check['source']['batch_ordinal'], 5)
            self.assertEqual(check['source']['tool'], 'read_file')
            for key in ('arguments_sha256', 'receipt_sha256'):
                self.assertRegex(check['source'][key], r'^[0-9a-f]{64}$')
            serialized = json.dumps(evidence, ensure_ascii=False)
            for private in (original, 'persistent-once.txt', 'PERSISTED_ONCE_RECEIPT',
                            '持久结果已读取核验', expected):
                self.assertNotIn(private, serialized, '历史证据简表不能投影原文或实际观察内容')
        def respond(body):
            results = self.results(body)
            if self.leaf(body):
                self.assertFalse(self.definitions(body) & FORBIDDEN)
                if not results:
                    return calls(('persistent-once', 'run_command', command))
                self.assertIn('PERSISTED_ONCE_RECEIPT', json.dumps(results))
                if len(results) == 1:
                    return calls(('persistent-stale-edit', 'edit_file', {'path': 'persistent-once.txt',
                        'old_string': 'x', 'new_string': 'xy', 'expected_sha256': '0' * 64}))
                self.assertFalse(results[1].get('success'), '陈旧哈希不能执行写入')
                if len(results) == 2:
                    self.assertEqual((self.source / 'persistent-once.txt').read_text(), 'x')
                    return calls(('persistent-source-read', 'read_file', {'path': 'persistent-once.txt'}))
                if len(results) == 3:
                    self.assertEqual(results[-1]['content'], 'x')
                    return calls(('persistent-valid-edit', 'edit_file', {'path': 'persistent-once.txt',
                        'old_string': 'x', 'new_string': 'xy', 'expected_sha256': results[-1]['sha256']}))
                if len(results) == 4:
                    return calls(('persistent-read', 'read_file', {'path': 'persistent-once.txt'}))
                self.assertEqual(len(results), 5)
                self.assertEqual(results[-1]['sha256'], expected)
                return text_response('持久结果已读取核验')
            if self.text(body) == original:
                if not results:
                    return calls(('persistent-delegate', 'run_subagent', {
                        'agent_id': 'general', 'task': original, 'include_context': True,
                        'max_steps': 5, 'verification': contract}))
                self.assertTrue(results[0]['completed'], results[0])
                if len(results) == 1:
                    run_id.append(results[0]['usage']['run_id'])
                    attached_evidence.append(results[0]['receipt_evidence'])
                    assert_evidence({'runs': attached_evidence, 'automatic_resume': False})
                    self.assertEqual(attached_evidence[0]['verification'], 'verified')
                    return calls(('persistent-evidence', 'list_subagent_runs', {
                        'run_id': run_id[0], 'evidence': True}))
                self.assertEqual(len(results), 2)
                assert_evidence(results[1])
                self.assertEqual(results[1]['runs'][0], attached_evidence[0],
                                 '原委派返回与显式只读查询必须引用同一份持久证据')
                return text_response('首次子代理已完成')
            self.assertIn('list_subagent_runs', self.definitions(body))
            if not results:
                return calls(('persistent-list', 'list_subagent_runs', {}),
                             ('persistent-detail', 'list_subagent_runs', {'run_id': run_id[0]}),
                             ('persistent-evidence-query', 'list_subagent_runs', {
                                 'run_id': run_id[0], 'evidence': True}))
            query_results.append(results)
            return text_response('持久记录只读查询完成')
        self.callback = respond
        self.enabled(self.send(self.message('persistent-start', original, expected='首次子代理已完成')))
        [identifier, frozen_bytes, state_bytes] = self.persisted_run_rows()[0]
        self.assertEqual(identifier, run_id[0])
        frozen, state = json.loads(frozen_bytes), json.loads(state_bytes)
        self.assertEqual(frozen['review_messages'], [original])
        self.assertIsNotNone(frozen['context'])
        self.assertEqual(frozen['verification'], contract)
        self.assertEqual(state['end']['status'], 'completed')
        self.assertEqual(state['usage']['tool_calls'], 5)
        frame = state['checkpoint']
        self.assertEqual((frame['used_tool_calls'], frame['provider_requests']), (5, 6))
        self.assertEqual(frame['model_session'], frozen['model_session'])
        self.assertEqual(frame['pending_calls'], [])
        self.assertIn('持久结果已读取核验', frame['result'])
        receipt = next(receipt for message in frame['messages']
                       for receipt in message.get('tool_results', [])
                       if receipt['call_id'] == 'persistent-read')
        receipt_digest = hashlib.sha256(json.dumps(
            receipt, ensure_ascii=False, separators=(',', ':')).encode()).hexdigest()
        self.assertEqual(attached_evidence[0]['report']['checks'][0]['source']['receipt_sha256'],
                         receipt_digest, '证据摘要必须绑定 SQLite 中同一原始工具回执')
        self.assertEqual((self.source / 'persistent-once.txt').read_text(), content)
        leaf_before = sum(self.leaf(r['body']) for r in self.kind('chat'))
        self.enabled(self.send(self.message('persistent-restart-query', '只读查询记录', expected='持久记录只读查询完成')))
        [listed, detail, evidence] = query_results[-1]
        self.assertFalse(listed['automatic_resume'])
        self.assertEqual([r['run_id'] for r in listed['runs']], run_id)
        row = detail['runs'][0]
        self.assertEqual(row['status'], 'completed')
        self.assertEqual((row['tool_calls'], row['model_requests']), (5, 6))
        self.assertEqual([s['tool'] for s in row['steps']], ['run_command', 'edit_file', 'read_file', 'edit_file', 'read_file'])
        self.assertTrue(all(s['confirmed'] for s in row['steps']))
        assert_evidence(evidence)
        self.assertEqual(evidence['runs'][0], attached_evidence[0], '重启不能改变原回执引用或扩大已验证范围')
        self.assertEqual(self.persisted_run_rows(), [(identifier, frozen_bytes, state_bytes)], '只读查询不能改变子代理库')
        self.enabled(self.send(self.message('persistent-other-scope', '其他身份只读查询', user='user-2', expected='持久记录只读查询完成')))
        self.assertTrue(all(result.get('runs') == [] for result in query_results[-1]),
                        '另一会话身份不能读取原执行记录：' + json.dumps(query_results[-1], ensure_ascii=False))
        # 仅在进程退出后模拟“结果已保存但结算尚未写入”的持久边界；保留真实宿主检查点与原结果。
        unsettled = dict(state, end=None, usage=None, verification=None, finished_at_ms=None)
        with closing(sqlite3.connect(self.work / 'state/subagent-runs.sqlite3')) as db:
            db.execute('UPDATE run_states SET state=? WHERE id=?', (json.dumps(unsettled).encode(), identifier))
            db.commit()
        unsettled_rows = self.persisted_run_rows()
        self.enabled(self.send(self.message('persistent-unsettled', '读取未结算结果', expected='持久记录只读查询完成')))
        row = query_results[-1][1]['runs'][0]
        self.assertEqual(row['status'], 'result_saved_unsettled')
        self.assertIn('持久结果已读取核验', row['saved_result'])
        self.assertFalse(row['automatic_resume'])
        self.assertFalse(row['effects_unknown'])
        unsettled_evidence = query_results[-1][2]
        assert_evidence(unsettled_evidence)
        self.assertEqual(unsettled_evidence['runs'][0]['run_status'], 'result_saved_unsettled')
        self.assertIsNone(unsettled_evidence['runs'][0]['verification'])
        self.assertEqual(unsettled_evidence['runs'][0]['report'], attached_evidence[0]['report'])
        self.assertEqual(self.persisted_run_rows(), unsettled_rows)
        self.assertEqual(sum(self.leaf(r['body']) for r in self.kind('chat')), leaf_before)
        self.assertEqual((self.source / 'persistent-once.txt').read_text(), content, '重启和查询不能重跑命令')

    def test_persistent_inflight_child_effect_remains_unknown_and_restart_never_replays(self):
        command = {'program': sys.executable, 'args': ['-c',
            "from pathlib import Path; import time; p=Path('persistent-unknown.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); time.sleep(30)"]}
        run_id = []
        query_results = []
        def respond(body):
            if self.leaf(body):
                self.assertFalse(self.results(body))
                return calls(('persistent-inflight', 'run_command', command))
            if self.text(body) == '启动可中断子代理':
                return calls(('persistent-interrupt-delegate', 'run_subagent', {
                    'agent_id': 'general', 'task': '在临时工程执行一次命令并等待完成', 'max_steps': 2}))
            if not self.results(body):
                return calls(('persistent-unknown-list', 'list_subagent_runs', {}),
                             ('persistent-unknown-detail', 'list_subagent_runs', {'run_id': run_id[0]}))
            query_results.append(self.results(body))
            return text_response('未知回执保留，未继续执行')
        self.callback = respond
        stop = self.gate('persistent-interrupt-stop')
        def started():
            self.wait_for(lambda: (self.source / 'persistent-unknown.txt').exists(), '子代理真实命令尚未开始', 20)
            [identifier, _, state_bytes] = self.persisted_run_rows()[0]
            self.assertTrue(json.loads(state_bytes)['checkpoint']['pending_calls'])
            run_id.append(identifier)
        self.enabled([{'send': self.message('persistent-interrupt', '启动可中断子代理')},
                      *self.checkpoint('persistent-command-started'), {'touch': str(stop)},
                      {'wait_file': str(self.gate('persistent-never'))}],
                     checkpoints={'persistent-command-started': started}, stop_at=stop)
        before = self.persisted_run_rows()
        leaf_before = sum(self.leaf(r['body']) for r in self.kind('chat'))
        self.enabled(self.send(self.message('persistent-unknown-restart', '只读核对中断记录', expected='未知回执保留，未继续执行')))
        row = query_results[-1][1]['runs'][0]
        self.assertEqual(row['status'], 'interrupted_unknown')
        self.assertTrue(row['effects_unknown'])
        self.assertFalse(row['automatic_resume'])
        self.assertEqual(row['pending_calls'], 1)
        self.assertEqual(row['steps'][0]['tool'], 'run_command')
        self.assertFalse(row['steps'][0]['confirmed'])
        self.assertEqual(self.persisted_run_rows(), before)
        self.assertEqual(sum(self.leaf(r['body']) for r in self.kind('chat')), leaf_before)
        self.assertEqual((self.source / 'persistent-unknown.txt').read_text(), 'x')

    def test_custom_coding_role_writes_verifies_and_survives_restart(self):
        saved = []
        def respond(body):
            results = self.results(body)
            if self.leaf(body):
                self.assertFalse(self.definitions(body) & FORBIDDEN)
                self.assertEqual(self.definitions(body), {'read_file','write_file'})
                if not results:
                    return calls(('write', 'write_file', {'path':'from-agent.py','content':'value = 42\n','expected_sha256':''}))
                if len(results) == 1:
                    return calls(('verify','read_file',{'path':'from-agent.py'}))
                return text_response('文件已写入并读取核验')
            if self.text(body) == '列出角色':
                if not results: return calls(('list','list_subagents',{}))
                self.assertEqual(results[0]['profiles'][0]['id'], saved[0])
                return text_response('角色仍在')
            if not results:
                return calls(('create','create_subagent',{'name':'专用编码','instructions':'只处理本次授权代码并核验','tools':['read_file','write_file']}))
            if len(results) == 1:
                saved.append(results[0]['profile']['id'])
                return calls(('run','run_subagent',{'agent_id':saved[0],'task':'写入 from-agent.py 并读取验证',
                    'verification': {'kind':'receipts','contract': {'checks': [{
                        'tool':'read_file','arguments':{'path':'from-agent.py'},
                        'output_pointer':'/sha256','expected':hashlib.sha256(b'value = 42\n').hexdigest()}],
                        'require_final_batch':True}}}))
            self.assertTrue(results[1]['completed'])
            self.assertEqual(results[1]['usage']['shared_steps_used'],4)
            return text_response('已创建并完成编码')
        self.callback = respond
        self.enabled(self.send(self.message('create','创建编码角色并完成实现',expected='已创建并完成编码')))
        self.assertEqual((self.source/'from-agent.py').read_text(), 'value = 42\n')
        self.enabled(self.send(self.message('restart','列出角色',expected='角色仍在')))

    def test_readonly_role_cannot_execute_a_write_or_recursive_tool(self):
        def respond(body):
            results = self.results(body)
            if self.leaf(body):
                self.assertFalse(self.definitions(body) & (FORBIDDEN | {'write_file','edit_file','run_command'}))
                if not results:
                    return calls(('bad-write','write_file',{'path':'must-not-exist.txt','content':'bad','expected_sha256':''}),
                                 ('bad-recurse','run_subagent',{'task':'不得启动'}))
                self.assertTrue(all(r.get('failed') or r.get('code') or 'unknown' in json.dumps(r).lower() or 'UnknownTool' in json.dumps(r) for r in results), results)
                return text_response('未改动，也未递归派发')
            if not results: return calls(('review','run_subagent',{'agent_id':'review','task':'静态审阅'}))
            return text_response('只读核验结束')
        self.callback = respond
        self.enabled(self.send(self.message('read-only','用核验子代理审阅',expected='只读核验结束')))
        self.assertFalse((self.source/'must-not-exist.txt').exists())

    def test_ten_parallel_children_keep_parent_reserve_with_explicit_hundred_steps(self):
        entered = []
        condition = threading.Condition()
        used = []
        active = peak = 0
        def respond(body):
            nonlocal active, peak
            results = self.results(body)
            if self.leaf(body):
                if not results and body.get('tools'):
                    with condition:
                        active += 1
                        peak = max(peak, active)
                        entered.append(self.text(body));condition.notify_all()
                        # 十个子代理共用五个实际模型槽，按两批请求核验共享步骤。
                        target = ((len(entered) - 1) // 5 + 1) * 5
                        try:
                            self.assertTrue(condition.wait_for(lambda:len(entered)>=target,timeout=12))
                        finally:
                            active -= 1
                    return calls(*[(f'echo-{i}','echo',{'text':str(i)}) for i in range(10)])
                return text_response('依据已有回执收尾')
            if not results:
                return calls(*[(f'agent-{i}','run_subagent',{'task':f'并行核验{i}'}) for i in range(10)])
            if len(results) == 10:
                used.extend(r['usage']['steps_used'] for r in results)
                self.assertTrue(all(not r['completed'] and not r['blocked'] for r in results),
                            'echo 和用尽共享预算不能证明子任务完成')
                self.assertTrue(all(r['usage']['shared_steps_used']<=80 for r in results))
                self.assertTrue(all(r['usage']['parent_reserved_steps']==20 for r in results))
                self.assertTrue(body.get('tools'),'子代理耗尽可用额度后父线程仍可核验')
                return calls(('parent-verify','echo',{'text':'父线程核验'}))
            self.assertEqual(len(results),11)
            return text_response('父线程已使用保留额度核验')
        self.callback = respond
        self.enabled(self.send(self.message('shared','并行核验十项',expected='父线程已使用保留额度核验')),
                     env_extra={'EVE_FOREGROUND_STEPS':'100'})
        self.assertEqual(len(entered),10)
        self.assertEqual(peak,5,'模型实际请求合计上限为五，子代理仍有十个槽')
        self.assertEqual(sum(used),70,'显式总额一百步：十次委派、子代理七十步，父线程保留二十步')

    def test_background_worker_can_delegate_and_checkpoints_include_child_steps(self):
        def respond(body):
            results = self.results(body)
            if self.leaf(body):
                self.assertFalse(self.definitions(body) & FORBIDDEN)
                if not results and body.get('tools'):
                    return calls(*[(f'child-{i}','echo',{'text':str(i)}) for i in range(2)])
                return text_response('子代理核验结束')
            if self.worker(body):
                if not results:
                    self.assertIn('run_subagent',self.definitions(body))
                    return calls(('child','run_subagent',{'task':'核验三个项目'}))
                self.assertEqual(results[0]['usage']['steps_used'],2)
                self.assertEqual(results[0]['usage']['shared_steps_remaining'],1)
                self.assertEqual(results[0]['usage']['delegated_steps_remaining'],0)
                self.assertFalse(results[0]['completed'])
                self.assertFalse(results[0]['blocked'])
                if len(results)==1:
                    self.assertTrue(body.get('tools'))
                    return calls(('parent-verify','echo',{'text':'收尾核验'}))
                self.assertEqual(len(results),2)
                self.assertFalse(body.get('tools'))
                return text_response('共享预算已用尽，原目标尚未通过验收')
            if not results:
                return calls(('dispatch','dispatch_background_task',{'title':'共享预算核验','instructions':'委派核验三个项目','max_steps':4}))
            return text_response('已派发')
        self.callback = respond
        def finished():
            self.wait_for(lambda:any(t['status']=='Blocked' and (t.get('notice') or {}).get('status')=='Delivered'
                for t in self.documents().get('eve.task',{}).get('tasks.v1',{}).get('tasks',[])), 'background did not block and notify', 20)
        self.enabled([*self.send(self.message('background','后台委派核验',expected='已派发')),*self.checkpoint('finished')],checkpoints={'finished':finished})
        [task] = self.documents()['eve.task']['tasks.v1']['tasks']
        self.assertEqual(task['usage']['tool_calls'],4)
        self.assertEqual(task['status'],'Blocked')
        self.assertIn('任务尚未通过冻结验收', task['result'])
        frame = task['execution']['checkpoint']
        self.assertEqual((frame['used_tool_calls'],frame['delegated_tool_calls']),(2,2))
        self.assertEqual(frame['delegated_model_requests'],2)

if __name__ == '__main__': unittest.main(verbosity=2)
