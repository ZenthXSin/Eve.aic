"""真实 QQ 宿主的请求前阻塞定位；仅连接本地模型和 QQ 替身。"""
import json
import threading
import time
import unittest

import semantic_test as semantic


class RequestTimingAcceptance(semantic.SemanticAcceptance):
    for name in dir(semantic.SemanticAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name

    def test_context_can_wait_without_a_primary_request_and_timing_separates_the_stages(self):
        self.seed_interaction('source', semantic.HISTORY)
        self.assertNotIn('EVE_HTTP_TIMING', self.last_stderr)
        query = '简短的合成准入观察，不是用户私密内容'
        entered, release = threading.Event(), threading.Event()
        before = len(self.requests)

        def wait_query(body):
            if body['input'] == [query]:
                entered.set()
                if not release.wait(5): raise AssertionError('query gate was not released')

        def inspect_pending():
            try:
                self.assertTrue(entered.wait(3), 'semantic query did not arrive')
                self.assertEqual(len(self.requests), before, 'primary request must not precede context completion')
                time.sleep(.15)
                self.assertEqual(len(self.requests), before)
            finally:
                release.set()

        self.before_embedding = wait_query
        self.semantic_run([*self.checkpoint('indexed'), {'send': self.message('timing', query)},
                           *self.checkpoint('blocked'), self.wait_reply('timing'), self.wait_receipt('timing')],
                          checkpoints={'indexed': self.indexed(2), 'blocked': inspect_pending},
                          env={'EVE_TRACE_DIALOGUE_TIMING': '1'})
        events = []
        for line in self.last_stderr.splitlines():
            if line.startswith(('EVE_QQ_TIMING ', 'EVE_CONTEXT_TIMING ', 'EVE_HTTP_TIMING ')):
                prefix, data = line.split(' ', 1)
                events.append((prefix, json.loads(data)))
        self.assertEqual(len(self.requests), before + 1)
        stages = [(prefix, value['phase']) for prefix, value in events]
        required = [('EVE_QQ_TIMING', 'message_received'), ('EVE_CONTEXT_TIMING', 'begin'),
                    ('EVE_CONTEXT_TIMING', 'ready'), ('EVE_QQ_TIMING', 'provider_begin'),
                    ('EVE_HTTP_TIMING', 'dispatch'), ('EVE_HTTP_TIMING', 'headers'),
                    ('EVE_HTTP_TIMING', 'decoded'), ('EVE_QQ_TIMING', 'session_saved')]
        positions = [stages.index(item) for item in required]
        self.assertEqual(positions, sorted(positions))
        context_ready = next(value for prefix, value in events if prefix == 'EVE_CONTEXT_TIMING' and value['phase'] == 'ready')
        self.assertGreaterEqual(context_ready['elapsed_ms'], 100)
        for _, data in events:
            self.assertLessEqual(set(data), {'request', 'phase', 'at_ms', 'elapsed_ms', 'http_status', 'turn'})
        self.assertNotIn(query, self.last_stderr)


if __name__ == '__main__': unittest.main()
