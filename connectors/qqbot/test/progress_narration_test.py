"""真实宿主的模型进度、群私聊限额与轮内提问；只连接本地替身。"""
import json
import hashlib
import sys
import unittest

import developer_tools_test as developer
import dialogue_tools_test as dialogue


class ProgressNarrationAcceptance(developer.DeveloperAcceptance):
    for name in dir(developer.DeveloperAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        self.narrations = []
        config = json.loads(self.developer_config.read_text())
        identity = json.dumps(["1904159860", "group", "group-1", "user-1"], separators=(",", ":")).encode()
        config["operators"].append("qq:" + hashlib.sha256(identity).hexdigest())
        self.developer_config.write_text(json.dumps(config))

    def provider_response(self, body, kind, fallback):
        if kind == 'progress':
            facts = json.loads(body['messages'][-1]['content'])
            self.assertFalse(body.get('tools'))
            self.narrations.append(facts)
            if facts['phase'] == 'start':
                return dialogue.text_response('先核对这两步的实际结果。')
            if facts['phase'] == 'waiting':
                return dialogue.text_response('' if facts['group'] else '这一步还在等待结果，我会继续核对。')
            return dialogue.text_response('第一步的回执已拿到，继续检查后面的结果。')
        return super().provider_response(body, kind, fallback)

    def run_progress(self, script, **options):
        env = {'EVE_DEVELOPER_CONFIG': str(self.developer_config),
               'EVE_PROGRESS_STAGE_UPDATES': 'true', 'EVE_PROGRESS_MIN_INTERVAL_MS': '1000',
               'EVE_PROGRESS_SLOW_AFTER_MS': '1000', 'EVE_CONTEXT_COMPACT_AT_TOKENS': '0'}
        env.update(options.pop('env_extra', {}))
        self.run_eve(script, interest=False, memory=False, research=False, practice=False,
                     env_extra=env, timeout=60, **options)
        self.events = [json.loads(line) for line in (self.work / f'events-{self.runs}.jsonl').read_text().splitlines()]

    def progress(self, message_id):
        return [e for e in self.events if e.get('type') == 'progress' and e.get('id') == message_id]

    def test_private_and_group_progress_are_model_prose_and_final_is_still_delivered(self):
        # 同一真实操作者在群里绑定不同 principal； fixture 管理员角色负责同一宿主授权。
        def respond(body):
            count = len(self.results(body))
            if count < 2:
                return dialogue.tool_response([('run_command', {'program': sys.executable,
                    'args': ['-c', 'import time; time.sleep(2.2)']})])
            return dialogue.text_response('两步验证完成，未部署。')
        # 每批工具 ID 必须不同，避免测试自身触发跨批次重复保护。
        def unique(body):
            response = respond(body)
            for call in response['choices'][0]['message'].get('tool_calls', []):
                call['id'] += str(len(self.results(body)))
            return response
        self.callback = unique
        self.run_progress([*self.send(self.message('private', '核对两步，不要部署', expected='两步验证完成，未部署。')),
                           *self.send(self.message('group', 'eve 核对两步，不要部署', scope='group', expected='两步验证完成，未部署。'))])
        private = self.progress('private')
        group = self.progress('group')
        self.assertTrue(private)
        self.assertTrue(group)
        self.assertLessEqual(len(group), 2)
        allowed = {'先核对这两步的实际结果。', '这一步还在等待结果，我会继续核对。',
                   '第一步的回执已拿到，继续检查后面的结果。'}
        self.assertTrue(all(e['text'] in allowed for e in private + group))
        self.assertTrue(any(f['group'] for f in self.narrations))
        self.assertTrue(any(not f['group'] for f in self.narrations))
        self.assertEqual(self.receipt('private')['state'], 'Sent')
        self.assertEqual(self.receipt('group')['state'], 'Sent')

    def test_live_question_returns_platform_and_session_ids_and_does_not_end_task(self):
        def respond(body):
            if not self.results(body):
                return dialogue.tool_response([('send_message', {'text': '这个目录需要保留吗？', 'purpose': 'question'})])
            result = self.results(body)[-1]
            self.assertTrue(result['sent'])
            self.assertTrue(result['message_id'])
            self.assertTrue(result['session_id'])
            return dialogue.text_response('问题已经送达，独立检查也完成了。')
        self.callback = respond
        self.run_progress(self.send(self.message('question', '先提问并继续独立检查', expected='问题已经送达，独立检查也完成了。')),
                          env_extra={'EVE_QQ_LIVE_INTERACTION': 'true', 'EVE_PROGRESS_STAGE_UPDATES': 'false',
                                     'EVE_PROGRESS_SLOW_AFTER_MS': '0'})
        self.assertEqual([e['text'] for e in self.progress('question')], ['这个目录需要保留吗？'])
        self.assertEqual(self.receipt('question')['state'], 'Sent')

    def test_file_queue_receipt_can_be_queried_with_actual_platform_and_session_ids(self):
        (self.source / 'artifact.txt').write_text('已验证的测试产物')
        def respond(body):
            results = self.results(body)
            if not results:
                return dialogue.tool_response([('send_file', {'path': 'artifact.txt'})])
            if results[-1].get('queued'):
                self.assertFalse(results[-1]['sent'])
                self.assertIsNone(results[-1]['message_id'])
                ledger = self.work / 'state/qq-outbox/ledger.json'
                self.wait_until(lambda: ledger.exists() and any(r['state'] == 'Sent' for r in json.loads(ledger.read_text())['records']), '真实文件回执未落盘')
                return dialogue.tool_response([('query_file_delivery', {'id': results[-1]['id']})])
            receipt = results[-1]
            self.assertTrue(receipt['sent'])
            self.assertTrue(receipt['message_id'].startswith('file-'))
            self.assertTrue(receipt['session_id'])
            self.assertFalse(receipt['retry_allowed'])
            return dialogue.text_response('产物已实际送达，凭据已核对。')
        self.callback = respond
        self.run_progress(self.send(self.message('artifact', '交付测试产物并核对回执', expected='产物已实际送达，凭据已核对。')),
                          env_extra={'EVE_PROGRESS_STAGE_UPDATES': 'false', 'EVE_PROGRESS_SLOW_AFTER_MS': '0'})
        self.assertEqual(self.receipt('artifact')['state'], 'Sent')


if __name__ == '__main__':
    unittest.main()
