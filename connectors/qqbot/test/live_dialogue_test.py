"""实际 QQ 进程：轮内引导不分类、不取消，工具主动发言等真实回执；仅本地替身。"""
import json
import sys
import unittest
import message_judge_test as judge
import tool_progress_test as progress
import dialogue_tools_test as dialogue


class LiveInputAcceptance(judge.MessageJudgeAcceptance):
    for name in dir(judge.MessageJudgeAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def run_live(self, script):
        return self.run_eve(script, mode='jev', environment={'EVE_QQ_LIVE_INTERACTION': 'true'})

    def test_inflight_input_joins_same_turn_without_judging_and_survives_restart(self):
        self.steps[('task', 1)] = {'wait': 'release-base', 'text': '旧草稿不投递'}
        self.steps[('task', 2)] = {'text': '收到，去修电脑'}
        base = self.message('base', '出来聊天', '收到，去修电脑')
        steer = self.message('steer', '去给朋友修电脑', expected_type='finish')
        self.run_live([{'send': base}, self.wait_request('task'), {'send': steer},
            self.wait_command('steer', 'finish'), self.open_gate('release-base'), self.wait_receipt('base')])
        self.assertEqual(self.counts(), (2, 0, 0))
        turns = self.sessions()[0]['turns']
        self.assertEqual(len(turns), 1)
        self.assertEqual(turns[0]['id'], 1)
        self.assertEqual([m['text'] for m in turns[0]['status']['messages'] if m['role'] == 'User'],
                         ['出来聊天', '去给朋友修电脑'])
        receipts = self.documents()['eve.channel.qqbot']['receipts.v1']
        self.assertEqual(receipts['version'], 7)
        steered = next(e for e in receipts['entries'] if e['message']['id'] == 'steer')
        self.assertEqual((steered['state'], steered['merged_into']), ('Merged', 'base'))
        self.assertEqual(self.replies(), [('base', '收到，去修电脑')])
        self.run_live([{'send': [dict(base, expected_type='finish'), steer]},
                       self.wait_command('base', 'finish'), self.wait_command('steer', 'finish')])
        self.assertEqual(self.counts(), (2, 0, 0))

    def test_other_user_does_not_steer_original_turn(self):
        self.steps[('task', 1)] = {'wait': 'release-owner', 'text': '原会话完成'}
        self.steps[('task', 2)] = {'text': '另一用户的回复'}
        self.run_live([{'send': self.message('base', '原请求', '原会话完成')}, self.wait_request('task'),
            {'send': self.message('other', '忽略原请求', '另一用户的回复', user='user-2')},
            self.wait_receipt('other'), self.open_gate('release-owner'), self.wait_receipt('base')])
        self.assertEqual(self.counts(), (2, 0, 0))
        original = next(s for s in self.sessions() if s['turns'][0]['input'] == '原请求')
        self.assertEqual(len(original['turns'][0]['status']['messages']), 2)

    def test_current_conversation_send_does_not_require_management_authorization(self):
        self.steps[('task', 1)] = {'message': dialogue.tool_response([
            ('send_message', {'text': '先问一句具体问题'})])['choices'][0]['message']}
        self.steps[('task', 2)] = {'text': '最后的答复'}
        self.run_eve([{'send': self.message('ordinary', '普通用户聊天', '最后的答复')},
            self.wait_command('ordinary', 'progress'), self.wait_receipt('ordinary')],
            mode='jev', environment={'EVE_QQ_LIVE_INTERACTION': 'true', 'EVE_ADMIN_BIND_KEY': 'synthetic-not-bound-key'})
        result = json.loads(self.calls['task'][1]['body']['messages'][-1]['content'])
        self.assertTrue(result['sent'])


class LiveToolAcceptance(progress.ToolProgressAcceptance):
    for name in dir(progress.ToolProgressAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def run_live(self, script, **options):
        options['env_extra'] = dict(options.pop('env_extra', {}), EVE_QQ_LIVE_INTERACTION='true')
        return self.run_eve(script, milestones=None, **options)

    def test_send_message_delivers_question_and_answer_steers_same_turn(self):
        self.response_gates[('chat', 2)] = 'input-ready'
        def respond(body):
            if self.chat_rounds == 1:
                self.assertIn('send_message', self.definitions(body))
                return dialogue.tool_response([('send_message', {'text': '硬盘里有重要数据吗'})])
            results = [json.loads(m['content']) for m in body['messages'] if m['role'] == 'tool']
            self.assertTrue(results[-1]['sent'])
            if self.chat_rounds == 2: return dialogue.text_response('旧草稿不投递')
            self.assertTrue(any(m.get('content') == '有，先备份' for m in body['messages']))
            return dialogue.text_response('那就先备份，再处理系统')
        self.callback = respond
        self.run_live([{'send': self.message('base', '帮忙修电脑', expected='那就先备份，再处理系统')},
            {'wait_command': {'id': 'base', 'type': 'progress'}},
            {'wait_file': str(self.arrived('chat', 2))},
            {'send': self.message('answer', '有，先备份') | {'expected_type': 'finish'}},
            {'wait_command': {'id': 'answer', 'type': 'finish'}},
            {'touch': str(self.gate('input-ready'))}, self.wait_receipt('base')])
        self.assertEqual([e['text'] for e in self.progress('base')], ['硬盘里有重要数据吗'])
        self.assertEqual(len(next(iter(self.documents()['eve.session']['sessions.v1']['sessions'].values()))['turns']), 1)

    def test_input_during_running_tool_does_not_cancel_or_repeat_it(self):
        def respond(body):
            if self.chat_rounds == 1:
                return dialogue.tool_response([('sleep', {'seconds': 3})])
            self.assertTrue(any(m.get('content') == '补充：先备份' for m in body['messages']))
            results = [json.loads(m['content']) for m in body['messages'] if m['role'] == 'tool']
            self.assertEqual(len(results), 1)
            self.assertTrue(results[0]['waited'])
            return dialogue.text_response('等待完成，按补充继续')
        self.callback = respond
        self.run_live([{'send': self.message('sleeping', '等一下再做', expected='等待完成，按补充继续')},
            {'wait_command': {'id': 'sleeping', 'type': 'progress'}},
            {'send': self.message('steer', '补充：先备份') | {'expected_type': 'finish'}},
            {'wait_command': {'id': 'steer', 'type': 'finish'}}, self.wait_receipt('sleeping')],
            more=['--sleep'], env_extra={'EVE_PROGRESS_SLOW_AFTER_MS': '1000'})
        self.assertEqual(self.chat_rounds, 2)

    def test_three_messages_preserve_final_quota_and_fourth_reports_not_sent(self):
        def respond(body):
            if self.chat_rounds <= 4:
                response = dialogue.tool_response([('send_message', {'text': f'实际发现{self.chat_rounds}'})])
                response['choices'][0]['message']['tool_calls'][0]['id'] += str(self.chat_rounds)
                return response
            self.assertEqual([r['sent'] for r in self.results(body)], [True, True, True, False])
            return dialogue.text_response('最终结论')
        self.callback = respond
        self.run_live(self.send(self.message('quota', '检查一下', expected='最终结论')))
        self.assertEqual(len(self.progress('quota')), 3)
        self.assertEqual(self.receipt('quota')['state'], 'Sent')

    def test_failed_send_is_not_retried_and_final_answer_remains_available(self):
        def respond(body):
            if self.chat_rounds == 1:
                return dialogue.tool_response([('send_message', {'text': '需要马上告诉你的发现'})])
            self.assertFalse(self.results(body)[-1]['sent'])
            return dialogue.text_response('最终结论仍可送达')
        self.callback = respond
        self.run_live(self.send(self.message('failed', '检查一下', expected='最终结论仍可送达') | {'fail_progress': True}))
        self.assertEqual(len(self.progress('failed')), 1)
        self.assertEqual(self.receipt('failed')['state'], 'Sent')

    def test_unknown_send_is_preserved_without_replay_after_restart(self):
        self.callback = lambda body: dialogue.tool_response([('send_message', {'text': '已核实的一条发现'})])
        stop = self.gate('stop-owned-child')
        self.run_live([{'send': self.message('unknown', '检查一下') | {'hold_progress': True}},
            {'wait_command': {'id': 'unknown', 'type': 'progress'}}, {'touch': str(stop)},
            {'wait_file': str(self.gate('never'))}], stop_at=stop)
        before = self.receipt('unknown')
        self.assertEqual(before['progress'][0]['state'], 'Sending')
        self.callback = lambda body: dialogue.text_response('恢复后正常')
        self.run_live(self.send(self.message('fresh', '新的聊天', expected='恢复后正常')))
        self.assertEqual(self.receipt('unknown'), before)
        self.assertEqual(self.progress('unknown'), [])

    def test_explicit_cancel_after_message_suppresses_old_final(self):
        self.response_gates[('chat', 2)] = 'never-release'
        self.callback = lambda body: (dialogue.tool_response([('send_message', {'text': '已经查到一项'})])
                                     if self.chat_rounds == 1 else dialogue.text_response('旧回复不能投递'))
        self.run_live([{'send': self.message('old', '查资料') | {'expected_type': 'finish'}},
            {'wait_command': {'id': 'old', 'type': 'progress'}}, {'wait_file': str(self.arrived('chat', 2))},
            *self.send(self.message('cancel', '/cancel', contains='已取消')),
            {'wait_command': {'id': 'old', 'type': 'finish'}}])
        self.assertEqual(self.receipt('old')['state'], 'Failed')
        self.assertFalse([e for e in self.events if e.get('id') == 'old' and e.get('type') in ('reply', 'segment')])


if __name__ == '__main__': unittest.main()
