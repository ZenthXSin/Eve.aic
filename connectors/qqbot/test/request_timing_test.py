"""真实 QQ 宿主的请求前阻塞定位；仅连接本地模型和 QQ 替身。"""
import json
import threading
import time
import unittest

import semantic_test as semantic


class RequestTimingAcceptance(semantic.SemanticAcceptance):
    panel = semantic.memory_harness.MemoryAcceptance.panel
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

    def test_slow_semantic_query_falls_back_before_the_embedding_server_finishes(self):
        self.seed_interaction('source', semantic.HISTORY)
        query = '回答简短：合成的慢查询验收'
        entered, release = threading.Event(), threading.Event()
        before = len(self.requests)
        waits = []

        def block_query(body):
            if body['input'] == [query]:
                waits.append(time.monotonic())
                entered.set()
                if not release.wait(5): raise AssertionError('query gate was not released')

        def verify_primary_precedes_release():
            try:
                self.assertTrue(entered.wait(3))
                self.wait_until(lambda: len(self.requests) > before, 'primary stayed blocked by embeddings')
                self.assertFalse(release.is_set())
                self.assertLess(time.monotonic() - waits[0], 2.5)
                [data] = self.recall_data(self.requests[-1])
                self.assertEqual(data['retrieval'], 'lexical_fallback')
                self.assertTrue(data['hits'])
            finally:
                release.set()

        self.before_embedding = block_query

        def set_budget_without_restart():
            self.indexed(2)()
            code, page = self.panel('/api/plugin-pages/read', {'plugin_id': 'eve.config', 'page_id': 'provider.openai'})
            self.assertEqual(code, 200)
            field = next(field for field in page['fields'] if field['id'] == 'semantic_query_timeout_ms')
            self.assertEqual(field['value'], 5000)
            self.assertFalse(field['restart_required'])
            code, saved = self.panel('/api/plugin-pages/save', {'plugin_id': 'eve.config', 'page_id': 'provider.openai',
                'instance': page['instance'], 'expected_revision': page['revision'], 'values': {'semantic_query_timeout_ms': 250}})
            self.assertEqual(code, 200)
            self.assertEqual(saved['restart_required'], [])

        self.semantic_run([*self.checkpoint('indexed'), {'send': self.message('bounded', query)},
                           *self.checkpoint('primary'), self.wait_reply('bounded'), self.wait_receipt('bounded')],
                          panel=True,
                          checkpoints={'indexed': set_budget_without_restart, 'primary': verify_primary_precedes_release},
                          env={'EVE_TRACE_DIALOGUE_TIMING': '1', 'EVE_SEMANTIC_QUERY_TIMEOUT_MS': '5000',
                               'EVE_MODELS_SEMANTIC_TIMEOUT_MS': '10000'})
        self.assertEqual(len(self.requests), before + 1)
        traces = [json.loads(line.split(' ', 1)[1]) for line in self.last_stderr.splitlines()
                  if line.startswith('EVE_RECALL_TIMING ')]
        self.assertEqual([item['phase'] for item in traces], ['embedding_begin', 'embedding_timeout'])
        self.assertEqual(traces[-1]['budget_ms'], 250)
        self.assertGreaterEqual(traces[-1]['elapsed_ms'], 200)
        self.assertLess(traces[-1]['elapsed_ms'], 2000)
        self.assertNotIn(query, self.last_stderr)
        self.assertTrue(self.index()['entries'], 'existing index must be preserved')

    def test_background_query_does_not_delay_primary_and_exact_cached_vectors_keep_scope_isolation(self):
        self.seed_interaction('source', semantic.HISTORY)
        query = semantic.PARAPHRASE
        entered, release = threading.Event(), threading.Event()
        before = len(self.requests)
        started = []

        def block_query(body):
            if body['input'] == [query]:
                started.append(time.monotonic())
                entered.set()
                if not release.wait(5): raise AssertionError('background query gate was not released')

        def enable_background():
            self.indexed(2)()
            code, page = self.panel('/api/plugin-pages/read', {'plugin_id': 'eve.config', 'page_id': 'provider.openai'})
            self.assertEqual(code, 200)
            field = next(field for field in page['fields'] if field['id'] == 'semantic_query_background')
            self.assertIs(field['value'], False)
            self.assertFalse(field['restart_required'])
            code, saved = self.panel('/api/plugin-pages/save', {'plugin_id': 'eve.config', 'page_id': 'provider.openai',
                'instance': page['instance'], 'expected_revision': page['revision'], 'values': {'semantic_query_background': True}})
            self.assertEqual(code, 200)
            self.assertEqual(saved['restart_required'], [])

        def primary_while_embedding_blocked():
            try:
                self.assertTrue(entered.wait(3))
                self.wait_until(lambda: len(self.requests) == before + 1, 'background query blocked the primary')
                self.assertLess(time.monotonic() - started[0], 2)
                self.assertFalse(release.is_set())
                self.assertEqual(self.recall_data(self.requests[-1]), [], 'cold paraphrase must not invent semantic hits')
                self.assertFalse(release.wait(.2))
                self.assertNotIn('"phase":"embedding_timeout"', self.panel_stderr.read_text(),
                                 'background request must outlive the foreground wait budget')
            finally:
                release.set()
            self.wait_until(lambda: '"phase":"embedding_ready"' in self.panel_stderr.read_text(), 'background query did not finish')

        self.before_embedding = block_query
        self.semantic_run([*self.checkpoint('enable'), *self.send(self.message('cold', query)),
                           *self.checkpoint('warm'), *self.send(self.message('cached', query)),
                           *self.send(self.message('foreign', query, user='user-2'))],
                          panel=True, env={'EVE_TRACE_DIALOGUE_TIMING': '1', 'EVE_SEMANTIC_QUERY_TIMEOUT_MS': '50'},
                          checkpoints={'enable': enable_background, 'warm': primary_while_embedding_blocked})
        self.assertEqual(len(self.requests), before + 3)
        [data] = self.recall_data(self.requests[-2])
        self.assertEqual(data['retrieval'], 'hybrid')
        self.assertTrue(any(semantic.HISTORY in hit['excerpt'] for hit in data['hits']))
        self.assertEqual(self.recall_data(self.requests[-1]), [], 'cached vectors cannot leak another user memory')
        self.assertEqual(sum(body['input'] == [query] for body in self.embeddings), 1)
        self.assertNotIn(query, self.last_stderr)


if __name__ == '__main__': unittest.main()
