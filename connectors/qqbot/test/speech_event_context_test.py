"""真实进程：后台事件有自己的触发原因，旧入站只作语境；不使用正文去重。"""
import hashlib
import json
import time
import unittest

import dialogue_tools_test as dialogue
import live_dialogue_test as live


class SpeechEventContextAcceptance(live.LiveToolAcceptance):
    for name in dir(live.LiveToolAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        principal = 'qq:' + hashlib.sha256(json.dumps(
            ['1904159860', 'c2c', 'user-1', 'user-1'], separators=(',', ':')).encode()).hexdigest()
        path = self.work / 'state/configuration/config.json'
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({'format_version': 1, 'current': {'revision': 1, 'namespaces': {
            'runtime.people': {'schema_version': 1, 'values': {'owner_principals': principal}}
        }}, 'backups': []}))

    def seed_user(self):
        def respond(body):
            if not self.all_results(body):
                return self.choose('send_message', {'text': '晚安，明早聊。'})
            return dialogue.text_response('{"action":"silence"}')
        self.callback = respond
        self.run_live(self.send(self.message('goodnight', '晚安喵，你慢慢来')))
        self.assertEqual([e['text'] for e in self.progress('goodnight')], ['晚安，明早聊。'])
        self.callback = lambda _: dialogue.text_response('{"action":"silence"}')

    def seed_incidents(self):
        # 仅在已停止的临时状态中创建合法历史事件，时间不同而入站始终只有一次。
        at = int(time.time() * 1000)
        rows = [{'id': 'health-' + str(n), 'at_ms': at - n * 60_000,
                 'findings': [{'code': 'synthetic.event' + str(n), 'summary': '本地事件 ' + str(n)}],
                 'owner': None, 'delivery': 'Pending'} for n in range(1, 4)]
        (self.work / 'state/health-watch.json').write_text(json.dumps({
            'version': 1, 'next_id': 4, 'checked_at_ms': at,
            'active': [r['findings'][0] for r in rows], 'notices': rows}))

    def run_incidents(self, inspect):
        return self.run_live(self.checkpoint('observed'), more=['--health-watch'],
            env_extra={'EVE_HEALTH_CHECK_MS': '3600000'}, checkpoints={'observed': inspect})

    def test_background_events_never_present_old_goodnight_as_new_input_and_do_not_replay(self):
        self.seed_user()
        self.seed_incidents()
        main_calls = self.chat_rounds

        def decide(facts):
            self.assertEqual(facts['trigger'], 'background_event')
            self.assertEqual(facts['host_event']['kind'], 'health_findings')
            self.assertNotIn('current_user_message', facts)
            self.assertNotIn('current_message_id', facts)
            self.assertNotIn('latest_user_message', facts)
            request = self.speech_requests[-1]
            self.assertNotIn('晚安喵，你慢慢来', request['messages'][-1]['content'])
            context = json.loads(request['messages'][4]['content'].split('：', 1)[1])
            self.assertEqual(context['latest_user_message'], {'id': 'goodnight', 'text': '晚安喵，你慢慢来'})
            return {'action': 'silence'}

        self.speech_callback = decide

        def settled():
            self.wait_until(lambda: len([s for e, s, _ in self.speech_rows()
                if e['facts'].get('kind') == 'health_findings' and s == 'silence']) == 3,
                '三个独立后台事件未结束')

        self.run_incidents(settled)
        self.assertEqual(self.chat_rounds, main_calls, '后台事实不能重启旧主对话')
        self.assertEqual(len(self.speech_requests), 3)
        self.assertFalse([e for e in self.events if e.get('type') in ('push', 'progress')])
        before = len(self.speech_requests)
        self.run_live([{'delay_ms': 2300}], more=['--health-watch'],
                      env_extra={'EVE_HEALTH_CHECK_MS': '3600000'})
        self.assertEqual(len(self.speech_requests), before)

    def test_distinct_event_can_choose_its_own_words_without_a_body_deduplicator(self):
        self.seed_user()
        self.seed_incidents()
        # 对真实不同的事件，模型可以自主选择相同文字；宿主不按文字相等吞消息。
        self.speech_callback = lambda _: {'action': 'speak', 'text': '这件事我想告诉你。', 'after_ms': 0}

        def sent():
            self.wait_until(lambda: len([s for e, s, _ in self.speech_rows()
                if e['facts'].get('kind') == 'health_findings' and s == 'sent']) == 3,
                '独立事件没有按各自决定发送')

        self.run_incidents(sent)
        self.assertEqual([e['text'] for e in self.events if e.get('type') == 'push'],
                         ['这件事我想告诉你。'] * 3)
        self.assertEqual(len({json.loads(r['messages'][-1]['content'])['event_id']
                              for r in self.speech_requests}), 3)


if __name__ == '__main__':
    unittest.main()
