"""真实 QQ 宿主的请求前阻塞定位；仅连接本地模型和 QQ 替身。"""
import hashlib
import io
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

    def capture_wire_requests(self):
        """保留替身实际收到的字节；仍由已有处理器回复，不增加请求或模型行为。"""
        captured = []
        handler = self.server.RequestHandlerClass

        class CaptureHandler(handler):
            def respond(self):
                if self.path != '/v1/chat/completions':
                    return super().respond()
                raw = self.rfile.read(int(self.headers['Content-Length']))
                captured.append({'raw': raw, 'headers': {
                    name: self.headers.get(name)
                    for name in ('session-id', 'thread-id', 'x-client-request-id')}})
                original = self.rfile
                self.rfile = io.BytesIO(raw)
                try:
                    return super().respond()
                finally:
                    self.rfile = original

        self.server.RequestHandlerClass = CaptureHandler
        return captured

    def test_wire_prefix_diagnostics_use_actual_host_body_with_timing_disabled(self):
        captured = self.capture_wire_requests()
        query = 'WIRE_HOST_PRIVATE_MARKER：这是本地合成交流，不应进入诊断日志。'
        self.run_eve(self.send(self.message('wire-on', query)), memory=False,
                     env_extra={'EVE_TRACE_MODEL_PREFIX': 'true', 'EVE_TRACE_DIALOGUE_TIMING': '0'})
        self.assertEqual(len(captured), 1)
        self.assertEqual(len(self.requests), 1, '诊断不得额外请求主模型')
        self.assertNotIn('EVE_HTTP_TIMING ', self.last_stderr)
        lines = [line for line in self.last_stderr.splitlines() if line.startswith('EVE_MODEL_WIRE_')]
        self.assertEqual([line.split(' ', 1)[0] for line in lines],
                         ['EVE_MODEL_WIRE_FINAL', 'EVE_MODEL_WIRE_HEADERS', 'EVE_MODEL_WIRE_USAGE'])
        final, headers, usage = [json.loads(line.split(' ', 1)[1]) for line in lines]
        for event in (final, headers, usage):
            self.assertEqual(event['schema_version'], 1)
            self.assertEqual(event['protocol'], 'chat')
            self.assertEqual(event['role'], 'dialogue')
            self.assertGreater(event['call'], 0)
            self.assertGreater(event['request'], 0)
            self.assertEqual(event['attempt'], 1)
            for key in ('domain', 'process_nonce', 'call', 'request', 'attempt'):
                self.assertEqual(event[key], final[key])
        raw = captured[0]['raw']
        body = json.loads(raw)
        self.assertEqual(body, self.requests[0]['body'])
        self.assertEqual(final['body']['bytes'], len(raw))
        self.assertEqual(len(final['body']['hmac_sha256']), 64)
        self.assertEqual(final['message_count'], len(body['messages']))
        self.assertFalse(final['messages_truncated'])
        self.assertTrue(any(message['role'] == 'system' for message in body['messages']))
        self.assertEqual(body['messages'][-1]['content'], query)

        def encoded_size(value):
            return len(json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode('utf8'))

        endpoint = f'http://127.0.0.1:{self.server.server_port}/v1/chat/completions'
        self.assertEqual(final['destination']['bytes'], encoded_size(endpoint))
        self.assertRegex(final['destination']['hmac_sha256'], r'^[0-9a-f]{64}$')
        self.assertNotIn(endpoint, self.last_stderr)
        # 对照最终 HTTP 正文中的全部消息（含宿主补充的 system），而非提前的 ModelRequest。
        for index, (message, item) in enumerate(zip(body['messages'], final['messages'], strict=True)):
            self.assertEqual(item['index'], index)
            self.assertEqual(item['role'], message['role'])
            self.assertEqual(item['bytes'], encoded_size(message))
        self.assertEqual(final['input']['bytes'], encoded_size(body['messages']))
        self.assertTrue(body['tools'], '此用例应覆盖宿主真实工具清单')
        self.assertEqual(final['tools']['count'], len(body['tools']))
        self.assertEqual(final['tools']['bytes'], encoded_size(body['tools']))
        self.assertEqual(final['tools']['truncated'], len(body['tools']) > 128)
        self.assertEqual(len(final['tools']['items']), min(len(body['tools']), 128))
        for tool, item in zip(body['tools'], final['tools']['items']):
            self.assertEqual(item['bytes'], encoded_size(tool))
        for field, name in (('session_id', 'session-id'), ('thread_id', 'thread-id'),
                            ('client_request_id', 'x-client-request-id')):
            value = captured[0]['headers'][name]
            self.assertTrue(value, name)
            self.assertEqual(final['identifiers'][field]['sha256'], hashlib.sha256(value.encode()).hexdigest())
            self.assertNotIn(value, self.last_stderr)
        self.assertEqual(final['identifiers']['prompt_cache_key']['sha256'],
                         hashlib.sha256(body['prompt_cache_key'].encode()).hexdigest())
        self.assertEqual(headers['http_status'], 200)
        # 既有替身没有 usage；诊断必须保留未知，不能为了验收补成零命中。
        self.assertIs(usage['usage_reported'], False)
        for field in ('input_tokens', 'cached_input_tokens', 'output_tokens'):
            self.assertIsNone(usage[field])
        self.assertNotIn(query, self.last_stderr)
        self.assertNotIn(body['prompt_cache_key'], self.last_stderr)

    def test_wire_prefix_diagnostics_stay_off_when_only_timing_is_enabled(self):
        captured = self.capture_wire_requests()
        query = 'WIRE_DISABLED_PRIVATE_MARKER：只开启阶段计时。'
        self.run_eve(self.send(self.message('wire-off', query)), memory=False,
                     env_extra={'EVE_TRACE_DIALOGUE_TIMING': '1'})
        self.assertEqual(len(captured), 1)
        self.assertEqual(len(self.requests), 1)
        self.assertIn('EVE_HTTP_TIMING ', self.last_stderr)
        self.assertNotIn('EVE_MODEL_WIRE_', self.last_stderr)
        self.assertNotIn(query, self.last_stderr)

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
        for prefix, data in events:
            fields = {'request', 'phase', 'at_ms', 'elapsed_ms', 'http_status', 'turn'}
            if prefix == 'EVE_HTTP_TIMING':
                fields |= {'call', 'attempt', 'role'}
                self.assertGreater(data['call'], 0)
                self.assertGreater(data['request'], 0)
                self.assertEqual(data['attempt'], 1)
                self.assertIn(data['role'], (None, 'dialogue'))
            self.assertLessEqual(set(data), fields)
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

        def query_requests():
            with self.lock:
                return [request for request in self.embedding_requests if request['body']['input'] == [query]]

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

        def cached_query_reuses_vector():
            self.assertEqual(len(query_requests()), 1, 'same-user warm query must reuse the completed vector')
            [data] = self.recall_data(self.requests[-1])
            self.assertEqual(data['retrieval'], 'hybrid')
            self.assertTrue(any(semantic.HISTORY in hit['excerpt'] for hit in data['hits']))

        def foreign_query_uses_own_session():
            self.wait_until(lambda: len(query_requests()) >= 2, 'another user must start a separate embedding request')
            requests = query_requests()
            self.assertEqual(len(requests), 2)
            first, foreign = requests
            for name in ('session-id', 'thread-id', 'x-client-request-id'):
                self.assertTrue(first['headers'][name], f'first query is missing {name}')
                self.assertTrue(foreign['headers'][name], f'foreign query is missing {name}')
                self.assertNotEqual(first['headers'][name], foreign['headers'][name], name)
            for request in requests:
                self.assertEqual(set(request['body']), {'model', 'input', 'dimensions', 'encoding_format'},
                                 'embedding session metadata belongs in headers')

        self.before_embedding = block_query
        self.semantic_run([*self.checkpoint('enable'), *self.send(self.message('cold', query)),
                           *self.checkpoint('warm'), *self.send(self.message('cached', query)),
                           *self.checkpoint('cache_hit'), *self.send(self.message('foreign', query, user='user-2')),
                           *self.checkpoint('foreign_query')],
                          panel=True, env={'EVE_TRACE_DIALOGUE_TIMING': '1', 'EVE_SEMANTIC_QUERY_TIMEOUT_MS': '50'},
                          checkpoints={'enable': enable_background, 'warm': primary_while_embedding_blocked,
                                       'cache_hit': cached_query_reuses_vector, 'foreign_query': foreign_query_uses_own_session})
        self.assertEqual(len(self.requests), before + 3)
        [data] = self.recall_data(self.requests[-2])
        self.assertEqual(data['retrieval'], 'hybrid')
        self.assertTrue(any(semantic.HISTORY in hit['excerpt'] for hit in data['hits']))
        self.assertEqual(self.recall_data(self.requests[-1]), [], 'cached vectors cannot leak another user memory')
        self.assertEqual(len(query_requests()), 2)
        self.assertNotIn(query, self.last_stderr)


if __name__ == '__main__': unittest.main()
