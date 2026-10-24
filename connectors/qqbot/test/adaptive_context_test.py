"""实际 QQ 进程：只设 200k 上限，历史自然增长、按固定开销动态压缩，后续短聊不重复压缩。"""
import json
import unittest
import context_compaction_test as compact
import history_digest_test as digest
from dialogue_tools_test import text_response


class AdaptiveContextAcceptance(compact.ContextCompactionAcceptance):
    for name in dir(compact.ContextCompactionAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def test_only_cap_is_needed_and_short_followups_reuse_saved_compaction(self):
        large_reply = '旧交流资料' * 1600
        def respond(body):
            if body['messages'][0]['content'].startswith(digest.DIGEST_PROMPT):
                request = json.loads(body['messages'][1]['content'])
                covered = request['previous_turns'] + len(request['turns'])
                return text_response(json.dumps({'points': [{'text': '保留交流目标与已知资料，等待用户后续要求', 'turns': [covered]}]}, ensure_ascii=False))
            return text_response(large_reply if self.large else '接着聊')
        self.callback = respond
        self.large = True
        script = []
        for i in range(13):
            script.extend(self.send(self.message('long-'+str(i), '第'+str(i)+'轮', expected=large_reply)))
        env = {'EVE_CONTEXT_ADAPTIVE_LIMIT_TOKENS': '200000'}
        self.run_window(script, panel=True, env_extra=env, timeout=90)
        count = len(self.digest_requests())
        self.assertGreater(count, 0)
        events = [json.loads(line.split(' ', 1)[1]) for line in self.panel_stderr.read_text().splitlines()
                  if line.startswith('EVE_CONTEXT_COMPACTION ')]
        self.assertTrue(events)
        self.assertLess(len(events), 5, '一次压缩可能分批生成摘要，但不能每句话都触发')
        self.assertTrue(all(e['trigger_tokens'] <= 200000 and e['estimated_after'] < 200000 for e in events))
        before = json.dumps(self.turn('第0轮'), sort_keys=True)
        self.large = False
        self.run_window([*self.send(self.message('short-1', '在吗', expected='接着聊')),
                         *self.send(self.message('short-2', '嗯', expected='接着聊')),
                         *self.send(self.message('short-3', '然后呢', expected='接着聊'))], env_extra=env)
        self.assertEqual(len(self.digest_requests()), count, '重启与短消息必须复用摘要')
        self.assertEqual(json.dumps(self.turn('第0轮'), sort_keys=True), before)
        for id in ['short-1', 'short-2', 'short-3']:
            self.assertFalse(self.receipt(id).get('progress'))


if __name__ == '__main__': unittest.main()
