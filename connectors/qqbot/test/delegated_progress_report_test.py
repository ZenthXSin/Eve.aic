"""真实子工具阶段自述到 Speech 的进程验收；模型与平台均为本地替身。

仅验证实际工具回执、等待中的上下文和最终验收，不评价模型表达风格。
并行、取消与未知回执沿用观察器边界及 natural_progress 的既有回归。
"""
from contextlib import closing
import hashlib
import json
import sqlite3
import unittest

import dialogue_tools_test as dialogue
import natural_progress_test as natural
from background_task_test import calls


class DelegatedProgressReportAcceptance(natural.NaturalProgressAcceptance):
    for name in dir(natural.NaturalProgressAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def test_report_from_actual_child_receipt_reaches_speech_before_later_tool_finishes(self):
        source_text = json.dumps({
            'duplicate_entry': 'river-map',
            'unindexed_entry': 'ridge-map',
            'private_detail': 'RAW_CHILD_RECORD_NOT_FOR_SPEECH_6821',
        }, ensure_ascii=False)
        source_path = self.source / 'stage-a.json'
        source_path.write_text(source_text, encoding='utf8')
        source_sha = hashlib.sha256(source_text.encode()).hexdigest()
        config = json.loads(self.developer_config.read_text())
        config['max_tool_rounds'] = 16
        self.developer_config.write_text(json.dumps(config))
        initial = '请交给编码分工核对合成索引，再运行隔离检查并核验实际结果。'
        report_id = 'child-report-a'
        parent_call_id = 'delegate-progress-report'
        reports = []
        decisions = []
        final_child_results = []
        report_text = []
        spoken = '子代理报来第一阶段的两项差异；后续检查仍未结束。'
        done_text = '后续隔离检查已返回成功，继续读取产物核验。'

        def respond(body):
            leaf = '你是 Eve 的子代理' in body['messages'][0]['content']
            (self.child_requests if leaf else self.main_requests).append(body)
            results = self.all_results(body)
            if leaf:
                self.assertEqual(body['model'], 'local-coding-fixture')
                if not results:
                    return calls(('child-read-a', 'read_file', {'path': 'stage-a.json'}))
                if len(results) == 1:
                    self.assertEqual(results[0]['sha256'], source_sha)
                    data = json.loads(results[0]['content'])
                    # 报告在收到实际文件回执后形成，只摘要当前阶段所需事实。
                    remaining = (f"第一阶段发现 {data['duplicate_entry']} 重复、"
                                 f"{data['unindexed_entry']} 未入索引；后续隔离检查尚待完成和核验。")
                    report_text.append(remaining)
                    return calls((report_id, 'task_progress', {
                        'status': 'in_progress', 'remaining': remaining}))
                if len(results) == 2:
                    self.assertEqual(results[-1], {
                        'status': 'in_progress', 'remaining': report_text[0],
                        'verified_by_host': False,
                    }, '必须经过真实 task_progress 工具，不以模型文本替代回执')
                    self.assertFalse(self.tool_started.exists())
                    return calls(('child-slow-b', 'run_command', self.command))
                if len(results) == 3:
                    self.assertTrue(results[-1]['success'], results[-1])
                    self.assertEqual(results[-1]['exit_code'], 0)
                    self.assertIn('ACTUAL_ISOLATED_CHECK_PASSED', results[-1]['stdout'])
                    self.assertEqual(self.executions.read_text(), 'x')
                    return calls(('child-report-done', 'task_progress', {
                        'status': 'completed', 'remaining': done_text}))
                if len(results) == 4:
                    self.assertEqual(results[-1], {
                        'status': 'completed', 'remaining': done_text,
                        'verified_by_host': False,
                    })
                    return calls(('child-verify-b', 'read_file', {'path': 'completed-once.txt'}))
                self.assertEqual(len(results), 5)
                self.assertEqual(results[-1]['content'], 'x')
                self.assertEqual(results[-1]['sha256'], hashlib.sha256(b'x').hexdigest())
                final_child_results.extend(results)
                return dialogue.text_response('阶段差异已保留，后续检查与产物已实际核验。')
            if not results:
                return calls((parent_call_id, 'run_subagent', {
                    'agent_id': 'coding', 'include_context': True, 'max_steps': 6,
                    'task': '读取 stage-a.json 核对索引差异，再运行 gated-check.py，最后读取 completed-once.txt 核验。',
                    'verification': {'kind': 'receipts', 'contract': {'checks': [{
                        'tool': 'read_file', 'arguments': {'path': 'completed-once.txt'},
                        'output_pointer': '/sha256',
                        'expected': hashlib.sha256(b'x').hexdigest(),
                    }], 'require_final_batch': True}},
                }))
            self.assertEqual(len(results), 1)
            self.assertTrue(results[0]['completed'], results[0])
            self.assertEqual(results[0]['usage']['agent_id'], 'coding')
            self.assertEqual(results[0]['usage']['steps_used'], 5)
            self.assertEqual(results[0]['receipt_evidence']['verification'], 'verified')
            self.assertEqual(results[0]['receipt_evidence']['report']['status'], 'historical_match')
            self.assertEqual(self.executions.read_text(), 'x')
            self.assertEqual(len(final_child_results), 5)
            self.assertEqual(self.users(body), [initial], '进度自述不能变成新的用户输入')
            self.completed_main = body
            return dialogue.text_response('真实结果已核验，内部结束。')

        def decide(facts):
            self.assertEqual(facts['host_event']['kind'], natural.PROGRESS_KIND)
            self.assertEqual(facts['trigger'], 'background_event')
            self.assertEqual(facts['host_event']['tool_state'], 'awaiting_results')
            self.assertTrue(self.tool_started.exists())
            self.assertFalse(self.release_tool.exists())
            self.assertFalse(self.executions.exists())
            self.assertIsNone(self.completed_main)
            self.assertEqual(len(self.main_requests), 1)
            self.assertEqual(len(self.child_requests), 3)
            [delegated] = facts['host_event']['delegated_tools']
            self.assertEqual(delegated['parent_call_id'], parent_call_id)
            self.assertEqual(delegated['submitted_calls'], 3)
            self.assertEqual(delegated['returned_calls'], 2)
            self.assertEqual(delegated['failed_calls'], 0)
            self.assertEqual(delegated['unknown_calls'], 0)
            self.assertEqual(delegated['pending_tool_names'], ['run_command'])
            self.assertEqual(delegated['last_returned_tool_name'], 'task_progress')
            self.assertEqual(delegated['task_progress_report'], {
                'call_id': report_id, 'status': 'in_progress', 'remaining': report_text[0],
            })
            meaning = facts['host_event']['delegated_results_meaning']
            self.assertIn('task_progress_report是匹配成功回执的子代理自述', meaning)
            self.assertIn('completed不证明测试、部署、业务成功或消息送达', meaning)
            encoded = json.dumps(facts['host_event'], ensure_ascii=False)
            self.assertEqual(encoded.count(report_text[0]), 1, '阶段正文只投影一份')
            for private in ('RAW_CHILD_RECORD_NOT_FOR_SPEECH_6821', source_text,
                            'stage-a.json', 'gated-check.py', str(self.source),
                            'ACTUAL_ISOLATED_CHECK_PASSED'):
                self.assertNotIn(private, encoded, '不能带入一般工具输出或源码')
            reports.append(delegated['task_progress_report'])
            decisions.append(facts)
            return {'action': 'speak', 'text': spoken, 'after_ms': 0}

        def delivered_during_b():
            self.wait_progress('sent')
            self.assertFalse(self.executions.exists())
            self.assertEqual(self.receipt('working')['state'], 'Processing')
            self.assertEqual(len(decisions), 1)
            self.assertEqual(len(self.main_requests), 1)

        self.callback = respond
        self.speech_callback = decide
        self.run_natural([
            {'send': self.message('working', initial)}, {'wait_file': str(self.tool_started)},
            *self.checkpoint('report-delivered-during-b'), {'touch': str(self.release_tool)},
            self.wait_receipt('working'), self.wait_command('working', 'finish'),
        ], checkpoints={'report-delivered-during-b': delivered_during_b})
        self.assertIsNotNone(self.completed_main)
        self.assertEqual(len(self.main_requests), 2)
        self.assertEqual(len(self.child_requests), 6)
        self.assertEqual(len(self.speech_requests), 1)
        self.assertEqual([event['text'] for event in self.outbound_progress()], [spoken])
        self.assertEqual(self.executions.read_text(), 'x', '原检查只执行一次')
        self.assertEqual(source_path.read_text(), source_text)
        self.assertEqual(reports, [{'call_id': report_id, 'status': 'in_progress',
                                   'remaining': report_text[0]}])
        with closing(sqlite3.connect(self.work / 'state/subagent-runs.sqlite3')) as db:
            [state_bytes] = [row[0] for row in db.execute('SELECT state FROM run_states')]
        state = json.loads(state_bytes)
        self.assertEqual(state['end']['status'], 'completed')
        receipts = [receipt for message in state['checkpoint']['messages']
                    for receipt in message.get('tool_results', [])]
        by_id = {receipt['call_id']: receipt for receipt in receipts}
        self.assertEqual(len(receipts), 5)
        self.assertIn(report_id, by_id)
        self.assertIn('child-slow-b', by_id)
        self.assertIn('child-verify-b', by_id)


if __name__ == '__main__':
    unittest.main()
