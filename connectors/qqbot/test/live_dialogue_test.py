"""真实 Eve 进程：只有明确表达会发送；模型和 QQ 均使用本地协议替身。"""
from contextlib import closing
import hashlib
import json
import os
import sqlite3
import sys
import time
import unittest

import dialogue_tools_test as dialogue
import tool_progress_test as progress
import vision_test as vision


class LiveToolAcceptance(progress.ToolProgressAcceptance):
    for name in dir(progress.ToolProgressAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        self.speech_requests = []
        self.speech_callback = lambda _: {"action": "silence"}

    def provider_response(self, body, kind, fallback):
        try:
            facts = json.loads(body['messages'][-1].get('content') or 'null')
        except (TypeError, ValueError):
            facts = None
        if isinstance(facts, dict) and 'host_event' in facts:
            self.assertFalse(body.get('tools'))
            self.assertEqual(body.get('response_format'), {'type': 'json_object'})
            self.assertEqual(body['messages'][-2]['role'], 'system')
            self.assertTrue(body['messages'][-2]['content'].startswith('本轮自主发言协议：'))
            self.assertIn('当前只返回 speak / wait / silence 的单个 JSON 决定',
                          body['messages'][-2]['content'])
            # 完整协议在稳定前缀中；末尾仍保留模式提醒，不能延续旧 final。
            self.assertEqual(body['messages'][1]['role'], 'system')
            self.assertIn('只能输出一个 JSON 决定', body['messages'][1]['content'])
            self.assertIn('以下 JSON 是固定宿主资料', body['messages'][2]['content'])
            self.assertIn('以下 JSON 是历史事实视图', body['messages'][3]['content'])
            self.assertIn('以下 JSON 是当前资料', body['messages'][4]['content'])
            context = json.loads(body['messages'][4]['content'].split('：', 1)[1])
            latest = context['latest_user_message']
            self.assertIsInstance(latest['text'], str)
            if facts['trigger'] == 'user_input':
                self.assertEqual(facts['current_user_message'], latest)
                self.assertEqual(latest['id'], facts['current_message_id'])
            else:
                self.assertEqual(facts['trigger'], 'background_event')
                self.assertNotIn('current_user_message', facts)
                self.assertNotIn('latest_user_message', facts)
                self.assertNotIn('current_message_id', facts)
            self.speech_requests.append(body)
            return dialogue.text_response(json.dumps(self.speech_callback(facts), ensure_ascii=False))
        return super().provider_response(body, kind, fallback)

    def run_live(self, script, **options):
        options['env_extra'] = dict(options.pop('env_extra', {}), EVE_QQ_LIVE_INTERACTION='true')
        result = self.run_eve(script, milestones=None, **options)
        self.assertFalse(self.narrations, '交流模式不能另起进度文案生成器')
        self.assertFalse([e for e in self.events if e.get('direction') == 'out' and
                          e.get('type') in ('reply', 'segment', 'sticker')],
                         '宿主不能发送内部 final、自动分段或自动附带表情')
        return result

    def run_live_failure(self, script, notifications, **options):
        """显式开启固定故障通知；普通 live 默认保持安静。"""
        self.assertTrue(notifications)
        options['env_extra'] = dict(options.pop('env_extra', {}), EVE_QQ_LIVE_INTERACTION='true',
                                    EVE_PROGRESS_FAILURE_NOTICES='true')
        result = self.run_eve(script, milestones=None, **options)
        self.assertFalse(self.narrations, '故障通知不得调用措辞模型')
        outputs = [e for e in self.events if e.get('direction') == 'out' and
                   e.get('type') in ('reply', 'segment', 'sticker')]
        self.assertCountEqual([e['id'] for e in outputs], notifications)
        for output in outputs:
            self.assertEqual(output['type'], 'reply', '系统通知只能单条投递')
            self.assertTrue(output['text'].startswith('系统通知：'))
            receipt = self.receipt(output['id'])
            self.assertEqual(receipt['state'], notifications[output['id']])
            self.assertEqual(receipt['reply'], output['text'])
            self.assertIsNone(receipt.get('control_turn_id'), '系统通知不能冒充成功对话')
            self.assertFalse(receipt.get('segments') or receipt.get('sticker'))
            first = next(e for e in self.events if e.get('direction') == 'out' and
                         e.get('id') == output['id'] and e.get('type') in ('finish', 'reply'))
            self.assertEqual(first['type'], 'reply', '不得先关闭原消息再转交发言模型')
        self.assertFalse(any(event['id'] in {f'host-reply:{id}' for id in notifications}
                             for event, _, _ in self.speech_rows()))
        return result

    def failure_message(self, id, text, **options):
        return super().message(id, text, contains=['系统通知：'], **options) | {'allow_finish': True}

    def failed_send(self, message, state='Sent'):
        return [{'send': message}, self.wait_command(message['id'], 'reply'),
                self.wait_receipt(message['id'], state)]

    def malformed_tool_response(self):
        response = self.choose('echo', {'text': '这一批不得执行'})
        response['choices'][0]['message']['tool_calls'][0]['function']['arguments'] = '{"text":'
        return response

    def message(self, *args, **kwargs):
        return super().message(*args, **kwargs) | {'expected_type': 'finish'}

    def wait_receipt(self, id, state='Handled'):
        return super().wait_receipt(id, state)

    def send(self, message):
        return [{'send': message}, self.wait_receipt(message['id']), self.wait_command(message['id'], 'finish')]

    @staticmethod
    def wait_command(id, kind, count=1):
        return {'wait_command': {'id': id, 'type': kind, 'count': count}}

    def choose(self, name, arguments):
        response = dialogue.tool_response([(name, arguments)])
        response['choices'][0]['message']['tool_calls'][0]['id'] += f'-{self.chat_rounds}'
        return response

    @staticmethod
    def all_results(body):
        return [json.loads(message['content']) for message in body['messages'] if message['role'] == 'tool']

    @staticmethod
    def users(body):
        return [message.get('content') for message in body['messages'] if message['role'] == 'user']

    def speech_rows(self):
        path = self.work / 'state/speech.sqlite3'
        if not path.exists():
            return []
        with closing(sqlite3.connect(path)) as db:
            return [(json.loads(event), state, json.loads(decision) if decision else None)
                    for event, state, decision in db.execute('SELECT event,state,decision FROM speech')]

    def test_internal_final_can_end_in_silence_even_with_automatic_progress_settings(self):
        def respond(body):
            self.assertIn('对外正文只经 send_message 明确发送', body['messages'][0]['content'])
            self.assertNotIn('output format: plain text', body['messages'][0]['content'])
            current = next(m['content'] for m in reversed(body['messages']) if m['role'] == 'system')
            self.assertEqual(current.count('本次即时通讯输出选择：'), 1)
            self.assertIn('{"action":"silence"}', current)
            if not self.all_results(body):
                return self.choose('echo', {'text': '内部核对完成'})
            return dialogue.text_response('只是内部结束，不是要说给对方的话')
        self.callback = respond
        summary = self.run_live(self.send(self.message('quiet', '这个先不用回答')), env_extra={
            'EVE_PROGRESS_TOOL_MILESTONES': 'true', 'EVE_PROGRESS_STAGE_UPDATES': 'true',
            'EVE_PROGRESS_SLOW_AFTER_MS': '1000'})
        self.assertEqual(self.progress('quiet'), [])
        self.assertEqual(self.receipt('quiet')['state'], 'Handled')
        self.assertIsNone(self.receipt('quiet')['reply'])
        self.assertEqual(self.speech_rows(), [])
        self.assertEqual(summary['sent'], 0)
        turns = next(iter(self.documents()['eve.session']['sessions.v1']['sessions'].values()))['turns']
        original = turns[-1]['status']['messages'][-1]
        self.assertEqual(original['text'], '只是内部结束，不是要说给对方的话')
        self.assertTrue(original['internal_only'])
        def inspect_history(body):
            records = [m.get('content') for m in body['messages'] if m['role'] == 'assistant']
            self.assertTrue(any(t and t.startswith('【宿主内部记录，未发送】') and original['text'] in t for t in records))
            self.assertNotIn(original['text'], records)
            return dialogue.text_response('继续保持安静')
        self.callback = inspect_history
        self.run_live(self.send(self.message('after-restart', '先不回答，接着刚才看一下')))
        self.assertFalse(self.progress('after-restart'))
        turns = next(iter(self.documents()['eve.session']['sessions.v1']['sessions'].values()))['turns']
        self.assertEqual(turns[0]['status']['messages'][-1], original)

    def test_memory_imports_delivered_text_and_preserves_internal_final_after_restart(self):
        def respond(body):
            if not self.all_results(body):
                return self.choose('send_message', {'text': '实际说的话'})
            return dialogue.text_response('绝不能记成已说的草稿')
        self.callback = respond
        self.run_live(self.send(self.message('spoken-memory', '看看')), memory=True)
        [source] = [item['source'] for item in self.interactions()]
        self.assertEqual(source['assistant_text'], '实际说的话')
        original = self.turn('看看')['status']['messages'][-1]
        self.assertEqual(original['text'], '绝不能记成已说的草稿')
        self.assertTrue(original['internal_only'])
        self.callback = lambda _: dialogue.text_response('{"action":"silence"}')
        self.run_live(self.send(self.message('memory-restart', '先不用答')), memory=True)
        self.assertEqual([item['source'] for item in self.interactions()], [source])
        self.assertEqual(self.turn('看看')['status']['messages'][-1], original)
        self.assertFalse(self.progress('memory-restart'))

    def test_question_and_wait_take_new_input_in_the_same_turn_without_ack(self):
        def respond(body):
            if self.chat_rounds == 1:
                return self.choose('send_message', {'text': '硬盘里有重要数据吗', 'purpose': 'question'})
            if self.chat_rounds == 2:
                self.assertTrue(self.all_results(body)[-1]['sent'])
                self.gate('waiting-started').touch()
                return self.choose('wait_for_input', {'timeout_ms': 10_000})
            if self.chat_rounds == 3:
                self.assertIn('有，先备份', self.users(body))
                self.assertTrue(self.all_results(body)[-1]['new_input'])
                return self.choose('send_message', {'text': '那先保住数据，再看系统的问题'})
            return dialogue.text_response('本轮结束')
        self.callback = respond
        summary = self.run_live([{'send': self.message('base', '帮忙修电脑')}, self.wait_command('base', 'progress'),
            {'wait_file': str(self.gate('waiting-started'))}, {'delay_ms': 150},
            {'send': self.message('answer', '有，先备份')}, self.wait_receipt('answer', 'Merged'), self.wait_receipt('base')])
        self.assertEqual([e['text'] for e in self.progress('base')], ['硬盘里有重要数据吗', '那先保住数据，再看系统的问题'])
        self.assertEqual(self.progress('answer'), [])
        self.assertEqual(summary['sent'], 2)
        turns = next(iter(self.documents()['eve.session']['sessions.v1']['sessions'].values()))['turns']
        self.assertEqual(len(turns), 1)
        self.assertEqual([m['text'] for m in turns[0]['status']['messages'] if m['role'] == 'User'], ['帮忙修电脑', '有，先备份'])

    def test_inflight_model_draft_is_discarded_after_a_correction(self):
        self.response_gates[('chat', 1)] = 'release-old-draft'
        def respond(body):
            if self.chat_rounds == 1:
                return dialogue.text_response('旧草稿不能投递')
            if self.chat_rounds == 2:
                self.assertEqual(self.users(body)[0], '出来聊聊')
                self.assertEqual(self.users(body)[-1], '等下，我先去修电脑')
                return self.choose('send_message', {'text': '行，忙完再聊'})
            return dialogue.text_response('内部结束')
        self.callback = respond
        # Handled 先落盘，桥接须收到随后写出的 finish 才能结束进程。
        self.run_live([{'send': self.message('base', '出来聊聊')}, {'wait_file': str(self.arrived('chat', 1))},
            {'send': self.message('steer', '等下，我先去修电脑')}, self.wait_receipt('steer', 'Merged'),
            {'touch': str(self.gate('release-old-draft'))}, self.wait_receipt('base'),
            self.wait_command('base', 'finish')])
        self.assertEqual([e['text'] for e in self.progress('base')], ['行，忙完再聊'])
        self.assertEqual(self.progress('steer'), [])
        before = self.chat_rounds
        self.run_live([{'send': self.message('base', '出来聊聊')}, self.wait_command('base', 'finish'),
                       {'send': self.message('steer', '等下，我先去修电脑')}, self.wait_command('steer', 'finish')])
        self.assertEqual(self.chat_rounds, before)
        self.assertFalse(self.progress('base'))

    def test_input_during_a_running_tool_does_not_cancel_repeat_or_announce_it(self):
        def respond(body):
            if self.chat_rounds == 1:
                return self.choose('sleep', {'seconds': 1})
            if self.chat_rounds == 2:
                self.assertIn('补充：先备份', self.users(body))
                self.assertTrue(self.all_results(body)[0]['waited'])
                return self.choose('send_message', {'text': '先备份，这条记住了'})
            return dialogue.text_response('内部结束')
        self.callback = respond
        self.run_live([{'send': self.message('sleeping', '等一下再做')}, {'wait_file': str(self.arrived('chat', 1))},
            {'delay_ms': 200}, {'send': self.message('steer', '补充：先备份')},
            self.wait_receipt('steer', 'Merged'), self.wait_receipt('sleeping')], more=['--sleep'],
            env_extra={'EVE_PROGRESS_STAGE_UPDATES': 'true', 'EVE_PROGRESS_SLOW_AFTER_MS': '1000'})
        self.assertEqual([e['text'] for e in self.progress('sleeping')], ['先备份，这条记住了'])
        messages = self.turn('等一下再做')['status']['messages']
        self.assertEqual(sum(c['name'] == 'sleep' for m in messages for c in m.get('tool_calls', [])), 1)

    def test_chosen_delay_is_cancelled_by_new_input_before_any_send(self):
        def respond(body):
            if self.chat_rounds == 1:
                return self.choose('send_message', {'text': '延后的旧话', 'after_ms': 3000})
            if self.chat_rounds == 2:
                self.assertEqual(self.all_results(body)[-1]['delivery_status'], 'stale')
                self.assertIn('不用了，事情变了', self.users(body))
                return self.choose('send_message', {'text': '好，那先停在这里'})
            return dialogue.text_response('内部结束')
        self.callback = respond
        self.run_live([{'send': self.message('delayed', '过会儿再说')}, {'wait_file': str(self.arrived('chat', 1))},
            {'delay_ms': 200}, {'send': self.message('changed', '不用了，事情变了')},
            self.wait_receipt('changed', 'Merged'), self.wait_receipt('delayed')])
        self.assertEqual([e['text'] for e in self.progress('delayed')], ['好，那先停在这里'])

    def test_multiple_messages_are_chosen_individually_without_mandatory_final(self):
        def respond(body):
            if self.chat_rounds <= 4:
                return self.choose('send_message', {'text': f'独立表达{self.chat_rounds}', 'after_ms': 1100 if self.chat_rounds > 2 else 0})
            self.assertEqual([r['sent'] for r in self.all_results(body)], [True] * 4)
            return dialogue.text_response('不可见的收尾')
        self.callback = respond
        self.run_live(self.send(self.message('several', '说说你查到的')))
        self.assertEqual([e['text'] for e in self.progress('several')], [f'独立表达{n}' for n in range(1, 5)])
        parts = self.receipt('several')['progress']
        self.assertEqual([p.get('proactive', False) for p in parts], [False, False, True, True])
        self.assertTrue(all(p['state'] == 'Sent' and p['platform_message_id'] for p in parts))

    def assert_parts_result(self, result, states, attempted, *, mode='explicit_parts',
                            requested=None):
        """核对模型实际收到的聚合回执，不把入队当作平台确认。"""
        self.assertIsNone(result['message_id'])
        self.assertTrue(result['session_id'])
        self.assertEqual(result['applied_mode'], mode)
        self.assertEqual(result['requested_part_count'], requested or len(states))
        self.assertEqual(result['actual_part_count'], len(states))
        self.assertEqual(result['sent_count'], states.count('sent'))
        self.assertEqual(result['attempted_count'], sum(attempted))
        self.assertIs(result['sent'], all(state == 'sent' for state in states))
        expected_status = ('sent' if result['sent'] else 'partial' if 'sent' in states
                           else states[0])
        self.assertEqual(result['delivery_status'], expected_status)
        self.assertEqual([part['index'] for part in result['parts']], list(range(len(states))))
        self.assertEqual([part['delivery_status'] for part in result['parts']], states)
        self.assertEqual([part['attempted'] for part in result['parts']], attempted)
        for part, state in zip(result['parts'], states):
            self.assertIs(part['sent'], state == 'sent')
            if state == 'sent':
                self.assertTrue(part['message_id'])
            else:
                self.assertIsNone(part['message_id'])

    def test_explicit_parts_preserve_repeated_short_messages_with_distinct_real_receipts(self):
        observed = []

        def respond(body):
            if not self.all_results(body):
                return self.choose('send_message', {'parts': [{'text': '嗯'}, {'text': '嗯'}]})
            result = self.all_results(body)[-1]
            self.assert_parts_result(result, ['sent', 'sent'], [True, True])
            self.assertEqual([part['message_id'] for part in result['parts']],
                             ['progress-parts-short-0', 'progress-parts-short-1'])
            observed.append(result)
            return dialogue.text_response('内部结束')

        self.callback = respond
        summary = self.run_live(self.send(self.message('parts-short', '按你的节奏说两句')),
                                more=['--segmented'])
        self.assertEqual(len(observed), 1, '一次工具调用产生一次聚合结果')
        self.assertEqual([frame['text'] for frame in self.progress('parts-short')], ['嗯', '嗯'])
        parts = self.receipt('parts-short')['progress']
        self.assertEqual([part['state'] for part in parts], ['Sent', 'Sent'])
        self.assertEqual([part['platform_message_id'] for part in parts],
                         [part['message_id'] for part in observed[0]['parts']])
        digest = hashlib.sha256(b'call-send_message-0-1').hexdigest()
        self.assertEqual([part['decision_id'] for part in parts],
                         [f'speech-parts-{digest}-{index}' for index in range(2)])
        self.assertEqual(summary['sent'], 2)

    def parts_stop_after_failed_receipt(self, diagnostic, state):
        observed = []

        def respond(body):
            if not self.all_results(body):
                return self.choose('send_message', {'parts': [
                    {'text': '第一条已经核实'}, {'text': '第二条的投递待确认'}, {'text': '第三条不能发送'}]})
            result = self.all_results(body)[-1]
            self.assert_parts_result(result, ['sent', state, 'not_sent'], [True, True, False])
            self.assertEqual(result['stopped_by'], state)
            self.assertEqual(result['parts'][2]['failure_kind'], f'previous_part_{state}')
            self.assertEqual(result['parts'][0]['message_id'], 'progress-parts-failure-0')
            observed.append(result)
            return dialogue.text_response('内部结束，保留已知的发送结果')

        self.callback = respond
        message = self.message('parts-failure', '逐条报告你核实的内容') | {
            'progress_diagnostics': [{}, {'ok': False, **diagnostic}]}
        self.run_live(self.send(message), more=['--segmented'])
        self.assertEqual(len(observed), 1)
        self.assertEqual([frame['text'] for frame in self.progress('parts-failure')],
                         ['第一条已经核实', '第二条的投递待确认'])
        parts = self.receipt('parts-failure')['progress']
        self.assertEqual([part['state'] for part in parts],
                         ['Sent', 'Unknown' if state == 'unknown' else 'Failed'])
        self.assertTrue(parts[0]['platform_message_id'])
        self.assertFalse(parts[1].get('platform_message_id'))

    def test_explicit_parts_stop_after_http_rejection_and_preserve_sent_prefix(self):
        self.parts_stop_after_failed_receipt({'failure_kind': 'http', 'http_status': 403},
                                            'not_sent')

    def test_explicit_parts_stop_after_unknown_delivery_without_retry_or_suffix_send(self):
        self.parts_stop_after_failed_receipt({'failure_kind': 'timeout'}, 'unknown')

    def test_new_input_during_part_pause_stops_old_suffix_and_is_consumed_in_same_turn(self):
        observed = []

        def respond(body):
            if self.chat_rounds == 1:
                return self.choose('send_message', {'parts': [
                    {'text': '先确认这一条'}, {'text': '延后的旧结论', 'after_ms': 3000},
                    {'text': '后面的旧结论'}]})
            if self.chat_rounds == 2:
                result = self.all_results(body)[-1]
                self.assert_parts_result(result, ['sent', 'stale', 'not_sent'],
                                         [True, False, False])
                self.assertEqual(result['stopped_by'], 'stale')
                self.assertEqual(result['parts'][2]['failure_kind'], 'previous_part_stale')
                self.assertIn('事情变了，先不要发旧结论', self.users(body))
                observed.append(result)
                return self.choose('send_message', {'text': '好，按你刚补充的情况继续'})
            self.assertEqual(self.chat_rounds, 3)
            self.assertTrue(self.all_results(body)[-1]['sent'])
            return dialogue.text_response('同一轮内部结束')

        self.callback = respond
        self.run_live([{'send': self.message('parts-paused', '按顺序说你的结论')},
            self.wait_command('parts-paused', 'progress'), {'delay_ms': 150},
            {'send': self.message('parts-changed', '事情变了，先不要发旧结论')},
            self.wait_receipt('parts-changed', 'Merged'), self.wait_receipt('parts-paused'),
            self.wait_command('parts-paused', 'finish')], more=['--segmented'])
        self.assertEqual(len(observed), 1)
        self.assertEqual([frame['text'] for frame in self.progress('parts-paused')],
                         ['先确认这一条', '好，按你刚补充的情况继续'])
        self.assertFalse(self.progress('parts-changed'))
        turns = [turn for session in self.documents()['eve.session']['sessions.v1']['sessions'].values()
                 for turn in session['turns']]
        self.assertEqual(len(turns), 1)
        self.assertEqual([message['text'] for message in turns[0]['status']['messages']
                          if message['role'] == 'User'],
                         ['按顺序说你的结论', '事情变了，先不要发旧结论'])

    def test_explicit_parts_merge_full_text_when_channel_segmentation_is_disabled(self):
        observed = []

        def respond(body):
            if not self.all_results(body):
                return self.choose('send_message', {'parts': [
                    {'text': '第一条完整内容'}, {'text': '第二条完整内容'}]})
            result = self.all_results(body)[-1]
            self.assert_parts_result(result, ['sent'], [True], mode='merged_disabled',
                                     requested=2)
            observed.append(result)
            return dialogue.text_response('内部结束')

        self.callback = respond
        self.run_live(self.send(self.message('parts-disabled', '说出所有内容')))
        self.assertEqual(len(observed), 1)
        self.assertEqual([frame['text'] for frame in self.progress('parts-disabled')],
                         ['第一条完整内容\n\n第二条完整内容'])
        self.assertEqual([part['state'] for part in self.receipt('parts-disabled')['progress']], ['Sent'])

    def test_segment_commands_apply_to_live_parts_and_preserve_merged_tail_text(self):
        observed = {}
        command_results = {}
        commands = {
            'segment-off': ('/segment off', '已关闭本会话分段', False, None),
            'segment-on': ('/segment on', '已开启本会话分段', True, None),
            'segment-limit': ('/segment parts 2', '已将本会话分段上限设为 2 段', True, 2),
        }
        requests = {'关闭后试一次': (['关闭后的第一条', '关闭后的第二条'],
                                    'merged_disabled', 1),
                    '重新开启后试一次': (['开启后的第一条', '开启后的第二条'],
                                          'explicit_parts', 2),
                    '限制两条后试一次': (['保留第一条', '尾部第二条', '尾部第三条'],
                                          'merged_limit', 2)}

        def respond(body):
            current = self.users(body)[-1]
            texts, mode, actual = requests[current]
            results = self.results(body)
            if not results:
                return self.choose('send_message', {'parts': [{'text': text} for text in texts]})
            result = results[-1]
            self.assert_parts_result(result, ['sent'] * actual, [True] * actual,
                                     mode=mode, requested=len(texts))
            observed[current] = result
            return dialogue.text_response('内部结束')

        def decide(facts):
            event = facts['host_event']
            self.assertEqual(event['kind'], 'host_operation_result')
            id = next(id for id, (text, _, _, _) in commands.items()
                      if text == event['user_text'])
            self.assertIn(commands[id][1], event['result'])
            self.assertNotIn(id, command_results)
            command_results[id] = event['result']
            return {'action': 'silence'}

        def inspect_command(id):
            def inspect():
                self.wait_until(lambda: any(event['id'] == f'host-reply:{id}' and state == 'silence'
                    for event, state, _ in self.speech_rows()), '分段命令的沉默决定没有落盘')
                self.assertIn(id, command_results)
                preferences = self.documents()['eve.segment.preferences']['preferences.v1']
                self.assertEqual(preferences['version'], 1)
                [setting] = preferences['scopes']
                routing = json.dumps(['1904159860', 'c2c', 'user-1', 'user-1'],
                                     separators=(',', ':')).encode()
                session = 'qq:' + hashlib.sha256(routing).hexdigest()
                self.assertEqual(setting['scope'], {
                    'channel': 'qq', 'session_id': session, 'user_id': session})
                self.assertIs(setting['enabled'], commands[id][2])
                self.assertEqual(setting['max_segments'], commands[id][3])
                self.assertIsNone(setting['pause_percent'])
                self.assertEqual(self.receipt(id)['state'], 'Handled')
                self.assertIsNone(self.receipt(id)['reply'])
            return inspect

        def command(id):
            return [*self.send(self.message(id, commands[id][0])), *self.checkpoint(id)]

        self.callback = respond
        self.speech_callback = decide
        # live 宿主命令先保存结果事实，Eve 可沉默；设置在下一次显式发送前已经落盘。
        self.run_live([*command('segment-off'),
            *self.send(self.message('parts-off', '关闭后试一次')),
            *command('segment-on'),
            *self.send(self.message('parts-on', '重新开启后试一次')),
            *command('segment-limit'),
            *self.send(self.message('parts-limit', '限制两条后试一次'))],
            more=['--segmented'], checkpoints={id: inspect_command(id) for id in commands})
        self.assertEqual(set(observed), set(requests))
        self.assertEqual(set(command_results), set(commands))
        self.assertEqual(self.chat_rounds, 6, '三个设置命令不能调用聊天模型')
        self.assertEqual(len(self.speech_requests), 3, '命令结果各由独立发言模型决定沉默')
        self.assertFalse([event for event in self.events if event.get('type') == 'push'])
        self.assertEqual([frame['text'] for frame in self.progress('parts-off')],
                         ['关闭后的第一条\n\n关闭后的第二条'])
        self.assertEqual([frame['text'] for frame in self.progress('parts-on')],
                         ['开启后的第一条', '开启后的第二条'])
        self.assertEqual([frame['text'] for frame in self.progress('parts-limit')],
                         ['保留第一条', '尾部第二条\n\n尾部第三条'])

    def test_failed_send_does_not_generate_an_automatic_replacement(self):
        def respond(body):
            if self.chat_rounds == 1:
                return self.choose('send_message', {'text': '一条已核实的发现'})
            self.assertFalse(self.all_results(body)[-1]['sent'])
            return dialogue.text_response('结束，不补备用消息')
        self.callback = respond
        self.run_live(self.send(self.message('failed', '检查一下') | {'fail_progress': True,
            'progress_diagnostic': {'failure_kind': 'http', 'http_status': 403, 'biz_code': 304023}}))
        self.assertEqual(len(self.progress('failed')), 1)
        self.assertEqual(self.receipt('failed')['progress'][0]['state'], 'Failed')

    def test_repeated_bad_arguments_send_one_system_notice_without_a_speech_decision(self):
        self.callback = lambda _: self.malformed_tool_response()
        message = self.failure_message('bad-arguments', '检查工具参数错误')
        summary = self.run_live_failure(self.failed_send(message), {'bad-arguments': 'Sent'},
            more=['--segmented'], memory=True, env_extra={'EVE_OPENAI_TRANSIENT_RETRIES': 'true'})
        self.assertEqual(self.chat_rounds, 2, '同一坏批次至多重取一次')
        self.assertFalse(self.speech_requests)
        failure = self.turn(message['text'])['status']['failure']
        self.assertEqual(failure['code'], 'Protocol')
        self.assertEqual(failure['started_tools'], 0)
        self.assertIn('本批工具未执行', self.receipt(message['id'])['reply'])
        self.assertIn('尚未启动工具调用', self.receipt(message['id'])['reply'])
        self.assertEqual(summary['sent'], 1)
        self.assertFalse(self.interactions(), '失败通知不进入成功交互学习')

    def test_failure_after_a_delivered_message_preserves_executed_command_and_sends_system_notice(self):
        marker = self.source / 'executions.txt'
        program = "from pathlib import Path; p=Path('executions.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('command-complete')"
        def respond(body):
            results = self.all_results(body)
            if not results:
                return self.choose('send_message', {'text': '已收到，正在核对'})
            if len(results) == 1:
                self.assertTrue(results[0]['sent'])
                return self.choose('run_command', {'program': sys.executable, 'args': ['-c', program]})
            return self.malformed_tool_response()
        self.callback = respond
        message = self.failure_message('partial-failure', '核对一次命令的执行结果')
        self.run_live_failure(self.failed_send(message), {'partial-failure': 'Sent'}, env_extra={
            'EVE_OPENAI_TRANSIENT_RETRIES': 'true', 'EVE_DEVELOPER_CONFIG': str(self.developer_config)})
        self.assertEqual(self.chat_rounds, 4)
        self.assertEqual(marker.read_text(), 'x', '重取坏参数不得重放前面的成功命令')
        self.assertEqual([e['text'] for e in self.progress(message['id'])], ['已收到，正在核对'])
        receipt = self.receipt(message['id'])
        self.assertEqual(receipt['progress'][0]['state'], 'Sent')
        self.assertTrue(receipt['progress'][0]['platform_message_id'])
        self.assertIn('已启动 2 次工具调用，收到 2 条工具回执', receipt['reply'])
        self.assertEqual(self.turn(message['text'])['status']['failure']['started_tools'], 2)
        self.assertFalse(self.speech_requests)

    def silent_failure_preserves_delivery(self, delivery_state):
        marker = self.source / 'executions.txt'
        program = "from pathlib import Path; p=Path('executions.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('command-complete')"
        def respond(body):
            results = self.all_results(body)
            if not results:
                return self.choose('send_message', {'text': '核对到的一条事实'})
            if len(results) == 1:
                self.assertEqual(results[0]['delivery_status'], delivery_state.lower())
                return self.choose('run_command', {'program': sys.executable, 'args': ['-c', program]})
            return self.malformed_tool_response()
        self.callback = respond
        message = self.message('quiet-failure', '检查实际执行与消息回执')
        if delivery_state == 'Unknown':
            message |= {'fail_progress': True, 'progress_diagnostic': {'failure_kind': 'timeout'}}
        summary = self.run_live([{'send': message}, self.wait_receipt(message['id'], 'Failed'),
            self.wait_command(message['id'], 'finish'), {'send': message},
            self.wait_command(message['id'], 'finish', count=2)], memory=True, env_extra={
                'EVE_OPENAI_TRANSIENT_RETRIES': 'true', 'EVE_DEVELOPER_CONFIG': str(self.developer_config)})
        self.assertEqual(self.chat_rounds, 4)
        self.assertEqual(marker.read_text(), 'x')
        self.assertEqual([e['text'] for e in self.progress(message['id'])], ['核对到的一条事实'])
        before = self.receipt(message['id'])
        self.assertIsNone(before['reply'])
        self.assertIsNone(before.get('control_turn_id'))
        self.assertEqual(before['progress'][0]['state'], delivery_state)
        self.assertEqual(bool(before['progress'][0].get('platform_message_id')), delivery_state == 'Sent')
        failure = self.turn(message['text'])['status']['failure']
        self.assertEqual(failure['code'], 'Protocol')
        self.assertEqual(failure['started_tools'], 2)
        self.assertEqual(summary['failed'], 1 + int(delivery_state == 'Unknown'))
        self.assertEqual(summary['sent'], int(delivery_state == 'Sent'))
        self.assertFalse(self.speech_requests)
        self.assertFalse(self.interactions(), '失败轮不进入成功交互学习')
        self.callback = lambda _: dialogue.text_response('内部结束')
        self.run_live([{'send': message}, self.wait_command(message['id'], 'finish'),
            *self.send(self.message('after-quiet-failure', '重启后这次先不回复'))], memory=True)
        self.assertEqual(self.chat_rounds, 5, '重启只处理新输入')
        self.assertEqual(self.receipt(message['id']), before)
        self.assertEqual(marker.read_text(), 'x', '重复输入和重启均不得重放命令')
        self.assertFalse(self.progress(message['id']))
        self.assertFalse(self.speech_requests)

    def test_default_failure_releases_input_and_preserves_sent_expression_after_restart(self):
        self.silent_failure_preserves_delivery('Sent')

    def test_default_failure_preserves_unknown_expression_without_replay_after_restart(self):
        self.silent_failure_preserves_delivery('Unknown')

    def failure_notice_not_replayed(self, state, diagnostic):
        self.callback = lambda _: self.malformed_tool_response()
        message = self.failure_message('notice-outcome', '核对故障通知的发送结果') | {
            'fail_delivery': True, 'delivery_diagnostic': diagnostic}
        self.run_live_failure([*self.failed_send(message, state), {'delay_ms': 100},
            {'send': message}, self.wait_command(message['id'], 'finish')], {message['id']: state},
            env_extra={'EVE_OPENAI_TRANSIENT_RETRIES': 'true'})
        self.assertEqual(self.chat_rounds, 2)
        self.assertFalse(self.speech_requests)
        before = self.receipt(message['id'])
        self.callback = lambda _: dialogue.text_response('内部结束，不重发旧消息')
        self.run_live(self.send(self.message('new-turn', '这次先不用回复')))
        self.assertEqual(self.receipt(message['id']), before)
        self.assertFalse(self.speech_requests)

    def test_rejected_system_notice_is_not_retried_after_duplicate_input_or_restart(self):
        self.failure_notice_not_replayed('Failed', {'failure_kind': 'http', 'http_status': 403})

    def test_unknown_system_notice_is_not_retried_after_duplicate_input_or_restart(self):
        self.failure_notice_not_replayed('Unknown', {'failure_kind': 'timeout'})

    def test_unknown_send_survives_restart_without_replay_and_enters_actual_receipt_context(self):
        self.callback = lambda _: self.choose('send_message', {'text': '可能已经送达的发现'})
        stop = self.gate('stop-owned-child')
        self.run_live([{'send': self.message('unknown', '开始检查') | {'hold_progress': True}},
            self.wait_command('unknown', 'progress'), {'touch': str(stop)},
            {'wait_file': str(self.gate('never'))}], stop_at=stop)
        before = self.receipt('unknown')
        self.assertEqual(before['progress'][0]['state'], 'Sending')
        def resumed(body):
            context = '\n'.join(m.get('content') or '' for m in body['messages'] if m['role'] == 'system')
            self.assertIn('可能已经送达的发现', context)
            self.assertIn('unknown', context)
            return dialogue.text_response('看到未知回执，安静结束')
        self.callback = resumed
        self.run_live(self.send(self.message('fresh', '接着聊点别的')))
        self.assertEqual(self.receipt('unknown'), before)
        self.assertFalse(self.progress('unknown'))
        self.assertTrue(any(state == 'unknown' for _, state, _ in self.speech_rows()))

    def test_duplicate_unknown_text_is_not_retried_but_new_expression_is_allowed(self):
        def respond(body):
            if self.chat_rounds < 4:
                return self.choose('send_message', {'text': '第一条发现' if self.chat_rounds < 3 else '不同的新发现'})
            self.assertEqual([r['delivery_status'] for r in self.all_results(body)], ['unknown', 'not_sent', 'unknown'])
            return dialogue.text_response('内部结束')
        self.callback = respond
        self.run_live(self.send(self.message('dedup', '看看这个') | {'fail_progress': True,
            'progress_diagnostic': {'failure_kind': 'timeout'}}))
        self.assertEqual([e['text'] for e in self.progress('dedup')], ['第一条发现', '不同的新发现'])

    def test_current_conversation_expression_does_not_require_management_authorization(self):
        def respond(body):
            if self.chat_rounds == 1:
                self.assertIn('send_message', self.definitions(body))
                self.assertIn('wait_for_input', self.definitions(body))
                return self.choose('send_message', {'text': '你说，我在听'})
            if self.chat_rounds == 2:
                self.assertTrue(self.all_results(body)[-1]['sent'])
                return self.choose('wait_for_input', {'timeout_ms': 50})
            self.assertFalse(self.all_results(body)[-1]['new_input'])
            return dialogue.text_response('内部结束')
        self.callback = respond
        self.run_live(self.send(self.message('ordinary', '有人吗', user='ordinary-user')),
                      env_extra={'EVE_ADMIN_BIND_KEY': 'synthetic-not-bound-key'})
        self.assertEqual([e['text'] for e in self.progress('ordinary')], ['你说，我在听'])

    def test_wait_timeout_may_end_without_saying_anything(self):
        def respond(body):
            if self.chat_rounds == 1:
                return self.choose('wait_for_input', {'timeout_ms': 100})
            self.assertFalse(self.all_results(body)[-1]['new_input'])
            return dialogue.text_response('不需要再补一句')
        self.callback = respond
        self.run_live(self.send(self.message('listen', '我想想再说')))
        self.assertFalse(self.progress('listen'))

    def test_host_command_becomes_a_fact_and_eve_can_choose_silence(self):
        def inspect():
            self.wait_until(lambda: any(state == 'silence' for _, state, _ in self.speech_rows()), 'Eve 的沉默决定没有落盘')
        self.run_live([*self.send(self.message('help', '/help')), *self.checkpoint('quiet-command')],
                      checkpoints={'quiet-command': inspect})
        self.assertEqual(len(self.speech_requests), 1)
        facts = json.loads(self.speech_requests[0]['messages'][-1]['content'])
        self.assertEqual(facts['host_event']['kind'], 'host_operation_result')
        self.assertFalse([e for e in self.events if e.get('type') == 'push'])

    def test_background_reconsideration_reads_new_original_even_when_that_turn_failed(self):
        self.callback = lambda _: dialogue.text_response('{"action":"silence"}')
        self.speech_callback = lambda _: {'action': 'wait', 'after_ms': 1000}
        def waiting():
            self.wait_until(lambda: any(event['id'] == 'host-reply:older' and state == 'waiting'
                for event, state, _ in self.speech_rows()), '旧事件没有保存等待决定')
        self.run_live([*self.send(self.message('older', '/help')), *self.checkpoint('waiting')],
                      checkpoints={'waiting': waiting})
        current = '我先忙，这条不用回复，也不要继续旧话题。'
        self.status_for_body = lambda body: 503 if body['messages'][-1].get('content') == current else 200
        def decide(facts):
            context = json.loads(self.speech_requests[-1]['messages'][4]['content'].split('：', 1)[1])
            self.assertEqual(context['latest_user_message'], {'id': 'new-failed', 'text': current})
            self.assertNotIn('current_user_message', facts)
            if facts['host_event']['user_text'] == '/help':
                self.gate('new-original-seen').touch()
            return {'action': 'silence'}
        self.speech_callback = decide
        self.run_live_failure([*self.failed_send(self.failure_message('new-failed', current)),
                       {'wait_file': str(self.gate('new-original-seen'))}, {'delay_ms': 300}],
                      {'new-failed': 'Sent'},
                      env_extra={'EVE_OPENAI_TRANSIENT_RETRIES': 'false'})
        self.assertEqual(self.turn(current)['status']['state'], 'Failed')
        self.assertFalse([e for e in self.events if e.get('type') in ('push', 'progress')])
        self.assertTrue(any(event['id'] == 'host-reply:older' and event['anchor'] == 'new-failed'
                            and state == 'silence' for event, state, _ in self.speech_rows()))

    def test_restart_cannot_send_cached_text_when_original_commit_is_ahead_of_receipt(self):
        self.speech_callback = lambda _: {'action': 'speak', 'text': '旧决定不可越过新原话', 'after_ms': 60_000}
        def ready():
            self.wait_until(lambda: any(event['id'] == 'host-reply:cached' and state == 'ready'
                for event, state, _ in self.speech_rows()), '延迟发言没有落盘')
        self.run_live([*self.send(self.message('cached', '/help')), *self.checkpoint('ready')],
                      checkpoints={'ready': ready})
        before = self.receipt('cached')
        # 模拟原文事务已提交、主回执尚未提交即停止的窗口，保留真实库结构与旧回执。
        with closing(sqlite3.connect(self.work / 'state/qq-receipts.sqlite3')) as db, db:
            db.execute('INSERT INTO qq_originals SELECT seq+1,app,?,session_id,session_user_id,scope,'
                       'target_id,user_id,? FROM qq_originals ORDER BY seq DESC LIMIT 1',
                       ('original-only', '先别继续，等我回来。'))
            db.execute('UPDATE qq_original_metadata SET committed_count=committed_count+1')
        with closing(sqlite3.connect(self.work / 'state/speech.sqlite3')) as db, db:
            db.execute("UPDATE speech SET due=0 WHERE state='ready'")
        calls = len(self.speech_requests)
        def postponed():
            def future_due():
                with closing(sqlite3.connect(self.work / 'state/speech.sqlite3')) as db:
                    return db.execute("SELECT due FROM speech WHERE state='ready'").fetchone()[0] > time.time() * 1000
            self.wait_until(future_due, '原话领先于回执时没有延后旧决定')
        self.run_live(self.checkpoint('postponed'), checkpoints={'postponed': postponed})
        self.assertEqual(self.receipt('cached'), before)
        self.assertEqual(len(self.speech_requests), calls)
        self.assertFalse([e for e in self.events if e.get('type') in ('push', 'progress')])

    def test_cancel_confirmation_is_authored_by_eve_and_old_output_stays_cancelled(self):
        self.response_gates[('chat', 2)] = 'never-release'
        self.callback = lambda _: (self.choose('send_message', {'text': '先查这条线索'})
                                  if self.chat_rounds == 1 else dialogue.text_response('旧回复不能投递'))
        def decide(facts):
            self.assertEqual(facts['host_event']['user_text'], '/cancel')
            return {'action': 'speak', 'text': '停下了，刚才那条线索先留着', 'after_ms': 0}
        self.speech_callback = decide
        push_id = 'speech-' + hashlib.sha256(b'host-reply:cancel').hexdigest()
        self.run_live([{'send': self.message('old', '查一下')}, self.wait_command('old', 'progress'),
            {'wait_file': str(self.arrived('chat', 2))}, *self.send(self.message('cancel', '/cancel')),
            self.wait_command('old', 'finish'), self.wait_command(push_id, 'push'), {'delay_ms': 150}])
        self.assertEqual(self.receipt('old')['state'], 'Failed')
        self.assertEqual([e['text'] for e in self.events if e.get('type') == 'push'], ['停下了，刚才那条线索先留着'])
        self.assertTrue(any(state == 'sent' for event, state, _ in self.speech_rows() if event['id'] == 'host-reply:cancel'))

    def install_sticker(self):
        data = vision.png('自主表情测试')
        sha = hashlib.sha256(data).hexdigest()
        directory = self.work / 'state/stickers'
        (directory / 'blobs').mkdir(parents=True)
        path = directory / 'blobs' / (sha + '.png')
        path.write_bytes(data)
        (directory / 'index.json').write_text(json.dumps({'format_version': 1, 'next': 1, 'stickers': [
            {'id': 's1', 'sha256': sha, 'media_type': 'image/png', 'bytes': len(data), 'state': 'approved',
             'description': '开心的猫', 'moods': ['开心'], 'uses': 0, 'added_at_ms': int(time.time() * 1000)}]}))
        return path

    def test_sticker_can_be_the_only_expression_and_returns_an_actual_receipt(self):
        path = self.install_sticker()
        def respond(body):
            if not self.all_results(body):
                return self.choose('send_sticker', {'mood': '开心'})
            result = self.all_results(body)[-1]
            self.assertTrue(result['sent'])
            self.assertTrue(result['message_id'])
            return dialogue.text_response('内部结束，不附文字')
        self.callback = respond
        self.run_live(self.send(self.message('image-only', '哈哈这也太好了')), more=['--stickers'])
        [frame] = self.progress('image-only')
        self.assertTrue(os.path.samefile(frame['image_path'], path))
        part = self.receipt('image-only')['progress'][0]
        self.assertTrue(os.path.samefile(part['image_path'], path))
        self.assertEqual(part['state'], 'Sent')
        self.assertTrue(part['platform_message_id'])
        self.assertTrue(any(event['facts'].get('medium') == 'image' and state == 'sent'
                            for event, state, _ in self.speech_rows()))
        self.callback = lambda _: dialogue.text_response('继续安静')
        self.run_live(self.send(self.message('image-restart', '先不用说')), more=['--stickers'])
        self.assertFalse(self.progress('image-only'))
        self.assertFalse(self.progress('image-restart'))

    def test_text_and_sticker_share_a_serial_send_queue_and_each_wait_for_its_receipt(self):
        self.install_sticker()
        def respond(body):
            if not self.all_results(body):
                return dialogue.tool_response([('send_message', {'text': '确实不错'}), ('send_sticker', {'mood': '开心'})])
            self.assertEqual([r['sent'] for r in self.all_results(body)], [True, True])
            self.assertEqual(len({r['message_id'] for r in self.all_results(body)}), 2)
            return dialogue.text_response('内部结束')
        self.callback = respond
        self.run_live(self.send(self.message('mixed', '你看这个好不好')), more=['--stickers'])
        frames = self.progress('mixed')
        self.assertEqual(len(frames), 2)
        self.assertEqual(frames[0]['text'], '确实不错')
        self.assertTrue(frames[1]['image_path'])
        self.assertTrue(all(p['state'] == 'Sent' for p in self.receipt('mixed')['progress']))


if __name__ == '__main__':
    unittest.main()
