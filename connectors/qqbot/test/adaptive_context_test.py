"""实际 QQ 进程：200k 上限下先分层装载，短上下文不强制压缩，原历史保持完整。"""
import json
import unittest
import context_compaction_test as compact
import history_digest_test as digest
from dialogue_tools_test import text_response
from request_messages import conversation_messages


class AdaptiveContextAcceptance(compact.ContextCompactionAcceptance):
    for name in dir(compact.ContextCompactionAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def test_layered_history_stays_bounded_and_short_followups_do_not_force_compaction(self):
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
        # 这里只测请求副本的主动压缩；后台持久摘要由 history_digest_test 独立验收。
        env = {'EVE_CONTEXT_ADAPTIVE_LIMIT_TOKENS': '200000', 'EVE_HISTORY_MAX_BYTES': '65536', 'EVE_HISTORY_DIGEST_ENABLED': 'false'}
        self.run_window(script, panel=True, env_extra=env, timeout=90)
        count = len(self.digest_requests())
        self.assertEqual(count, 0, '按需装载已能容纳时不强制发摘要模型请求')
        history_bytes = [len(json.dumps([m for m in conversation_messages(r['body']['messages'])
                                        if m['role'] != 'system'],
                                       ensure_ascii=False).encode('utf8')) for r in self.requests]
        self.assertTrue(history_bytes)
        self.assertLess(max(history_bytes), 65536, '请求历史受原64KiB窗口限制，不能全量重灌')
        events = [json.loads(line.split(' ', 1)[1]) for line in self.panel_stderr.read_text().splitlines()
                  if line.startswith('EVE_CONTEXT_COMPACTION ')]
        self.assertEqual(events, [])
        for i in range(13):
            self.assertEqual(self.turn('第'+str(i)+'轮')['status']['messages'][-1]['text'], large_reply)
        before = json.dumps(self.turn('第0轮'), sort_keys=True)
        self.large = False
        self.run_window([*self.send(self.message('short-1', '在吗', expected='接着聊')),
                         *self.send(self.message('short-2', '嗯', expected='接着聊')),
                         *self.send(self.message('short-3', '然后呢', expected='接着聊'))], env_extra=env)
        self.assertEqual(len(self.digest_requests()), count, '重启与短消息不应强制压缩')
        self.assertEqual(json.dumps(self.turn('第0轮'), sort_keys=True), before)
        for id in ['short-1', 'short-2', 'short-3']:
            self.assertFalse(self.receipt(id).get('progress'))


if __name__ == '__main__': unittest.main()
