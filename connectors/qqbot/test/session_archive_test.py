"""真实 QQ 宿主跨进程冷会话验收；本地模型替身，不占用付费模型预算。"""
import hashlib
import json
import unittest
import context_reset_test as reset
from dialogue_tools_test import text_response


class SessionArchiveAcceptance(reset.ContextResetAcceptance):
    for _name in dir(reset.ContextResetAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def run_reset(self, script):
        self.run_eve(script, interest=False, memory=False, research=False, practice=False,
                     env_extra={'EVE_DEVELOPER_CONFIG': str(self.developer_config),
                                'EVE_SESSION_COLD_ARCHIVE': 'true',
                                # 本组核对原文与轮次，摘要协议由独立用例覆盖。
                                'EVE_HISTORY_DIGEST_ENABLED': 'false'})

    def documents(self):
        documents = super().documents()
        index = documents.get('eve.session', {}).get('sessions.v1')
        if index and index.get('format_version') == 2:
            for history in index['sessions'].values():
                turns = []
                for ref in history['turns']:
                    digest = ref['hash']
                    self.assertEqual(len(digest), 64)
                    self.assertTrue(all(c in '0123456789abcdef' for c in digest))
                    payload = (self.work / 'state/session-archive' / (digest + '.json')).read_bytes()
                    self.assertEqual(len(payload), ref['bytes'])
                    self.assertEqual(hashlib.sha256(payload).hexdigest(), digest)
                    turns.append(json.loads(payload))
                history['turns'] = turns
            index['format_version'] = 1
        return documents

    test_personal_and_global_boundaries_survive_restart = reset.ContextResetAcceptance.test_personal_and_global_boundaries_survive_restart_without_deleting_turns
    test_non_admin_cannot_reset_other_histories = reset.ContextResetAcceptance.test_non_admin_global_and_forged_personal_target_are_rejected_without_changing_boundaries

    def test_fifty_continuous_turns_keep_exact_history_over_five_processes(self):
        seen = []
        contexts = []
        def respond(body):
            users = self.users(body)
            index = len(seen)
            contexts.append((users, index))
            seen.append(users[-1])
            return text_response(f'确认-{index:02d}')
        self.callback = respond
        for start in range(0, 50, 10):
            self.run_reset([step for index in range(start, start + 10)
                            for step in self.send(self.message(f'cold-{index}', f'连续冷会话-{index:02d}', expected=f'确认-{index:02d}'))])
        self.assertEqual(len(seen), 50)
        for actual, index in contexts:
            # 分层装载允许省略旧轮次；核对最近六轮完整、选择仅来自原文且次序不变。
            recent = [f'连续冷会话-{i:02d}' for i in range(max(0, index - 6), index + 1)]
            self.assertEqual(actual[-len(recent):], recent)
            positions = [int(text.removeprefix('连续冷会话-')) for text in actual]
            self.assertEqual(positions, sorted(set(positions)))
            self.assertTrue(all(0 <= i <= index for i in positions))
        raw = json.loads(self.state_path.read_text(encoding='utf8'))
        index = json.loads(bytes(raw['entries']['eve.session']['sessions.v1']))
        self.assertEqual(index['format_version'], 2)
        self.assertNotIn('连续冷会话', json.dumps(index, ensure_ascii=False))
        history = next(iter(self.documents()['eve.session']['sessions.v1']['sessions'].values()))
        self.assertEqual(len(history['turns']), 50)
        self.assertEqual(history['revision'], 100)
        self.assertTrue(all(t['status']['state'] == 'Completed' for t in history['turns']))


if __name__ == '__main__':
    unittest.main()
