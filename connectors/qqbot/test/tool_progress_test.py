"""真实宿主的 100 次调用上限、途中回执、配额、取消与独立进程恢复。"""
import json
import unittest

import dialogue_tools_test as dialogue
import memory_test


class ToolProgressAcceptance(dialogue.DialogueToolsAcceptance):
    for name in dir(dialogue.DialogueToolsAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name
    receipt = memory_test.MemoryAcceptance.receipt
    turn = memory_test.MemoryAcceptance.turn

    def run_eve(self, script, **options):
        milestones = options.pop('milestones', True)
        options.setdefault('interest', False)
        options.setdefault('research', False)
        options.setdefault('practice', False)
        options.setdefault('memory', False)
        options.setdefault('more', ['--max-tool-rounds', '64'])
        options.setdefault('timeout', 60)
        env = dict(options.pop('env_extra', None) or {})
        # 三个工具里程碑单独验收；启用压缩时预留一次压缩提示，由 context_compaction_test 覆盖。
        env.setdefault('EVE_CONTEXT_COMPACT_AT_TOKENS', '0')
        if milestones is not None:
            env.setdefault('EVE_PROGRESS_TOOL_MILESTONES', str(milestones).lower())
        options['env_extra'] = env
        result = super().run_eve(script, **options)
        path = self.work / f'events-{self.runs}.jsonl'
        self.events = [json.loads(line) for line in path.read_text().splitlines()]
        return result

    def calls(self, count):
        response = dialogue.tool_response([('echo', {'text':'合成工具回执'}) for _ in range(count)])
        for call in response['choices'][0]['message']['tool_calls']:
            call['id'] += f'-request-{self.chat_rounds}'
        return response

    def progress(self, id):
        return [e for e in self.events if e.get('type') == 'progress' and e.get('id') == id]

    def test_default_fast_tool_turn_sends_only_the_answer(self):
        self.callback = lambda body: dialogue.text_response('看清了') if self.results(body) else self.calls(1)
        self.run_eve(self.send(self.message('quiet', '看一下', expected='看清了')), milestones=None)
        self.assertEqual(self.progress('quiet'), [])
        self.assertNotIn('progress', self.receipt('quiet'))
        self.assertEqual(self.receipt('quiet')['state'], 'Sent')

    def test_hundred_calls_three_reports_final_segments_and_fresh_turn_budget(self):
        def respond(body):
            if not body.get('tools'):
                self.assertEqual(len(self.results(body)), 100)
                return dialogue.text_response('第一段结果。\n\n第二段结果。\n\n尚未核实的部分仍未完成。')
            if self.chat_rounds > 101:
                return dialogue.text_response('下一轮正常') if self.results(body) else self.calls(1)
            return self.calls(1)
        self.callback = respond
        message = self.message('fifty', '连续合成调用') | {'accept_any': True}
        self.run_eve([{'send':message}, self.wait_receipt('fifty'),
                      *self.send(self.message('fresh', '新的测试', expected='下一轮正常'))],
                     more=['--segmented'])
        self.assertEqual(self.chat_rounds, 103)
        self.assertEqual(len(self.progress('fifty')), 3)
        self.assertEqual([p['state'] for p in self.receipt('fifty')['progress']], ['Sent'] * 3)
        finals = [e for e in self.events if e.get('id') == 'fifty' and e.get('type') in ['reply','segment']]
        self.assertTrue(finals)
        self.assertLessEqual(len(finals) + len(self.progress('fifty')), 4)
        self.assertEqual(len(self.progress('fresh')), 1)
        before = self.receipt('fifty')
        self.callback = lambda _: dialogue.text_response('重启正常')
        self.run_eve(self.send(self.message('restart', '重启后聊天', expected='重启正常')))
        self.assertEqual(self.receipt('fifty'), before)
        self.assertFalse(self.progress('fifty'))

    def test_oversized_parallel_batch_is_not_executed(self):
        def respond(body):
            if body.get('tools'): return self.calls(101)
            self.assertEqual(self.results(body), [])
            return dialogue.text_response('超额批次未执行')
        self.callback = respond
        self.run_eve(self.send(self.message('over', '超额批次', expected='超额批次未执行')))
        self.assertEqual(self.chat_rounds, 2)
        self.assertFalse(self.progress('over'))
        self.assertFalse(any(m['role'] == 'tool' for m in self.requests[-1]['body']['messages']))

    def test_failed_progress_is_not_retried_and_does_not_replace_final_receipt(self):
        self.callback = lambda body: dialogue.text_response('最终完成') if self.results(body) else self.calls(1)
        message = self.message('failure', '进度发送失败', expected='最终完成') | {'fail_progress':True}
        self.run_eve(self.send(message))
        self.assertEqual(len(self.progress('failure')), 1)
        self.assertEqual(self.receipt('failure')['state'], 'Sent')
        self.assertEqual(self.receipt('failure')['progress'][0]['state'], 'Failed')
        before = self.receipt('failure')
        self.callback = lambda _: dialogue.text_response('恢复正常')
        self.run_eve(self.send(self.message('after', '重启', expected='恢复正常')))
        self.assertEqual(self.receipt('failure'), before)
        self.assertFalse(self.progress('failure'))

    def test_cancel_after_progress_suppresses_old_final_and_new_progress(self):
        self.response_gates[('chat', 2)] = 'never-release'
        self.callback = lambda body: self.calls(1)
        old = self.message('old', '执行一个等待任务') | {'expected_type':'finish'}
        self.run_eve([{'send':old}, {'wait_command':{'id':'old','type':'progress'}},
                      {'wait_file':str(self.arrived('chat', 2))},
                      *self.send(self.message('cancel', '/cancel', contains='已取消')),
                      {'wait_command':{'id':'old','type':'finish'}}])
        self.assertEqual(len(self.progress('old')), 1)
        self.assertEqual(self.receipt('old')['state'], 'Failed')
        self.assertFalse([e for e in self.events if e.get('id') == 'old' and e.get('type') in ['reply','segment']])

    def test_unknown_progress_delivery_is_preserved_without_replay_after_process_restart(self):
        self.response_gates[('chat', 2)] = 'never-release'
        self.callback = lambda body: self.calls(1)
        message = self.message('unknown', '等待投递回执') | {'hold_progress':True}
        stop = self.gate('kill-owned-child')
        self.run_eve([{'send':message}, {'wait_command':{'id':'unknown','type':'progress'}},
                      {'wait_file':str(self.arrived('chat', 2))}, {'touch':str(stop)},
                      {'wait_file':str(self.gate('never'))}], stop_at=stop)
        self.assertEqual(self.receipt('unknown')['progress'][0]['state'], 'Sending')
        self.response_gates.clear()
        self.callback = lambda _: dialogue.text_response('恢复后正常')
        self.run_eve(self.send(self.message('restart', '独立进程恢复', expected='恢复后正常')))
        self.assertEqual(self.receipt('unknown')['progress'][0]['state'], 'Sending')
        self.assertFalse(self.progress('unknown'))

    def test_replacement_has_its_own_progress_and_suppresses_retired_generation(self):
        self.response_gates[('chat', 2)] = 'never-release'
        self.callback = lambda body: dialogue.text_response('新任务完成') if self.results(body) else self.calls(1)
        old = self.message('retired', '旧任务等待') | {'expected_type':'finish'}
        self.run_eve([*self.send(self.message('bind-progress', '/bind progress-acceptance-key', contains='管理员')),
                      {'send':old}, {'wait_command':{'id':'retired','type':'progress'}},
                      {'wait_file':str(self.arrived('chat', 2))},
                      *self.send(self.message('replace', '/new 新任务', expected='新任务完成')),
                      {'wait_command':{'id':'retired','type':'finish'}}],
                     env_extra={'EVE_ADMIN_BIND_KEY': 'progress-acceptance-key'})
        self.assertEqual(len(self.progress('retired')), 1)
        self.assertEqual(len(self.progress('replace')), 1)
        self.assertEqual(self.receipt('retired')['state'], 'Failed')
        self.assertEqual(self.receipt('replace')['state'], 'Sent')


if __name__ == '__main__': unittest.main()
