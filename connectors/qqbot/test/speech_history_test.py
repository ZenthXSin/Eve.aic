"""真实宿主恢复长工具历史后仍可自主回应命令；模型与QQ仅使用本地替身。"""
import hashlib
import json
import unittest

import live_dialogue_test as live


class SpeechHistoryAcceptance(live.LiveToolAcceptance):
    for name in dir(live.LiveToolAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def seed_large_history(self):
        # 先由真实宿主建立会话/原话/回执，退出后只在临时夹具补入合法长历史。
        self.callback = lambda _: live.dialogue.text_response('内部记录')
        self.run_live(self.send(self.message('initial', '历史起点')))
        path = self.work / 'state/state.json'
        state = json.loads(path.read_text())
        ledger = json.loads(bytes(state['entries']['eve.session']['sessions.v1']))
        [session] = ledger['sessions'].values()
        completed = []
        for number in range(2, 8):
            user = f'第{number}轮旧工作'
            call_id = f'long-call-{number}'
            msg = lambda role, text=None, calls=None, results=None: {
                'role': role, 'text': text, 'tool_calls': calls or [], 'tool_results': results or []}
            output = '不可重放的旧工具输出' * 10_000
            messages = [msg('User', user),
                msg('Assistant', calls=[{'id': call_id, 'name': 'read_file', 'arguments': {'path': 'old.txt'}}]),
                msg('Tool', results=[{'call_id': call_id, 'output': {'Success': {'content': output}}}]),
                msg('Assistant', '旧内部收尾，尚未发送')]
            completed.append({'id': number, 'input': user,
                              'status': {'state': 'Completed', 'messages': messages}})
        self.assertGreater(sum(len(json.dumps(t['status']['messages']).encode()) for t in completed), 256 * 1024)
        self.history_files = {}
        for turn in completed:
            if ledger['format_version'] == 2:
                raw = json.dumps(turn, ensure_ascii=False, separators=(',', ':')).encode()
                digest = hashlib.sha256(raw).hexdigest()
                target = self.work / 'state/session-archive' / (digest + '.json')
                target.write_bytes(raw)
                self.history_files[target] = digest
                session['turns'].append({'hash': digest, 'bytes': len(raw)})
            else:
                session['turns'].append(turn)
        session['revision'] += 2 * len(completed)
        state['entries']['eve.session']['sessions.v1'] = list(json.dumps(ledger, ensure_ascii=False, separators=(',', ':')).encode())
        path.write_text(json.dumps(state))

    def test_command_is_answered_after_long_history_and_restart_without_replaying_tools(self):
        self.seed_large_history()
        before = self.chat_rounds
        def decide(facts):
            self.assertEqual(facts['current_user_message'], {'id': 'help-long', 'text': '/help'})
            self.assertEqual(facts['host_event']['kind'], 'host_operation_result')
            return {'action': 'speak', 'text': '可以，先说你想查看哪一项。', 'after_ms': 0}
        self.speech_callback = decide
        push_id = 'speech-' + hashlib.sha256(b'host-reply:help-long').hexdigest()
        self.run_live([*self.send(self.message('help-long', '/help')), self.wait_command(push_id, 'push'), {'delay_ms': 150}])
        self.assertEqual(self.chat_rounds, before, '命令发言不应再次启动主线程执行旧工具')
        self.assertEqual(len(self.speech_requests), 1)
        body = self.speech_requests[0]
        self.assertLess(len(json.dumps(body, ensure_ascii=False).encode()), 200 * 1024)
        self.assertNotIn('不可重放的旧工具输出', json.dumps(body, ensure_ascii=False))
        view_text = next(m['content'] for m in body['messages'] if m.get('content', '').startswith('以下 JSON 是历史事实视图'))
        view = json.loads(view_text.split('：', 1)[1])
        self.assertEqual([t['turn_id'] for t in view['turns']], list(range(2, 8)))
        self.assertTrue(all(t['internal_final']['text'] == '旧内部收尾，尚未发送' for t in view['turns']))
        self.assertEqual([e['text'] for e in self.events if e.get('type') == 'push'], ['可以，先说你想查看哪一项。'])
        self.assertTrue(any(event['id'] == 'host-reply:help-long' and state == 'sent'
                            for event, state, _ in self.speech_rows()))
        for path, digest in self.history_files.items():
            self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), digest)
        count = len(self.speech_requests)
        self.run_live([])
        self.assertEqual(len(self.speech_requests), count)
        self.assertFalse([e for e in self.events if e.get('type') in ('push', 'progress')])


if __name__ == '__main__':
    unittest.main()
