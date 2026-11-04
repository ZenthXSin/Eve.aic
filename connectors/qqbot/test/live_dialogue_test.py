"""真实 Eve 进程：只有明确表达会发送；模型和 QQ 均使用本地协议替身。"""
from contextlib import closing
import hashlib
import json
import os
import sqlite3
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
        self.run_live([{'send': self.message('base', '出来聊聊')}, {'wait_file': str(self.arrived('chat', 1))},
            {'send': self.message('steer', '等下，我先去修电脑')}, self.wait_receipt('steer', 'Merged'),
            {'touch': str(self.gate('release-old-draft'))}, self.wait_receipt('base')])
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
