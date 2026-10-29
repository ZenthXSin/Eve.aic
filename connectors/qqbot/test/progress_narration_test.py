"""真实宿主的模型进度、群私聊限额与轮内提问；只连接本地替身。"""
import json
import hashlib
import sys
from pathlib import Path
import unittest
from unittest.mock import patch

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
                return dialogue.text_response('NO_PROGRESS' if facts['group'] else '这一步还在等待结果，我会继续核对。')
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

    def test_private_ack_does_not_wait_for_a_blocked_progress_model(self):
        self.response_gates[('progress', 1)] = 'release-progress'
        def respond(body):
            if not any(m["role"] == "tool" for m in body["messages"]):
                return dialogue.tool_response([('run_command', {'program': sys.executable,
                    'args': ['-c', 'import time; time.sleep(3)']})])
            return dialogue.text_response('原任务已完成')
        self.callback = respond
        self.run_progress([{'send': self.message('running', '执行检查', expected='原任务已完成')},
                           {'wait_file': str(self.arrived('progress', 1))},
                           {'send': self.message('followup', '人呢', contains='已把这条消息加入原任务')},
                           self.wait_receipt('followup'),
                           {'touch': str(self.gate('release-progress'))}, self.wait_receipt('running')],
                          env_extra={'EVE_QQ_LIVE_INTERACTION': 'true'})
        self.assertEqual(self.receipt('followup')['state'], 'Sent')
        sessions = self.documents()['eve.session']['sessions.v1']['sessions']
        self.assertEqual(sum(len(s['turns']) for s in sessions.values()), 1)

    def test_invalid_narration_uses_bounded_host_status_and_final_still_arrives(self):
        original = self.provider_response
        def broken(body, kind, fallback):
            if kind == 'progress':
                return dialogue.text_response('')
            return original(body, kind, fallback)
        self.provider_response = broken
        def respond(body):
            if not self.results(body):
                return dialogue.tool_response([('run_command', {'program': sys.executable,
                    'args': ['-c', 'import time; time.sleep(2.2)']})])
            return dialogue.text_response('工具执行完成')
        self.callback = respond
        self.run_progress(self.send(self.message('fallback', '执行检查', expected='工具执行完成')))
        texts = [e['text'] for e in self.progress('fallback')]
        self.assertTrue(texts)
        self.assertEqual(len(texts), len(set(texts)))
        self.assertTrue(all(t in ['任务已进入工具执行阶段，完成后会返回结果。',
                                '任务仍在执行，当前步骤还没有返回结果。'] for t in texts))
        self.assertEqual(self.receipt('fallback')['state'], 'Sent')

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
        (self.source / 'artifact.txt').write_text('已验证的测试产物', encoding='utf-8')
        expected = '产物已实际送达，凭据已核对。'
        platform_frame = self.gate('artifact-platform-frame')
        observed = []
        failures = []
        queries = 0

        def respond(body):
            nonlocal queries
            results = self.results(body)
            if not results:
                return dialogue.tool_response([('send_file', {'path': 'artifact.txt'})])
            receipt = results[-1]
            observed.append(receipt)
            if receipt.get('state') == 'Sent':
                return dialogue.text_response(expected)
            if receipt.get('state') not in ('Pending', 'Sending'):
                failures.append({'unexpected_file_result': receipt})
                return dialogue.text_response(expected)
            try:
                # Windows 下读句柄可能阻止宿主原子替换 ledger；等真实桥接事件，
                # 不在模型 HTTP 回调中并发读取投递账本。
                self.wait_until(platform_frame.exists, '替身平台未收到文件发送命令')
            except AssertionError as error:
                failures.append({'platform_frame': str(error), 'last_result': receipt})
                return dialogue.text_response(expected)
            queries += 1
            if queries > 6 or 'query_file_delivery' not in self.definitions(body):
                failures.append({'file_query_budget_exhausted': queries, 'last_result': receipt})
                return dialogue.text_response(expected)
            response = dialogue.tool_response([('query_file_delivery', {'id': receipt['id']})])
            # 桥接写出回执不等于宿主已处理回执；Pending/Sending 时有界再次查询。
            response['choices'][0]['message']['tool_calls'][0]['id'] += str(queries)
            return response

        self.callback = respond
        message = self.message('artifact', '交付测试产物并核对回执', expected=expected)
        script = [{'send': message},
                  {'wait_command': {'id': 'file-1', 'type': 'file'}},
                  {'touch': str(platform_frame)}, self.wait_reply('artifact'),
                  self.wait_receipt('artifact')]
        read_text = Path.read_text
        def no_concurrent_ledger_read(path, *args, **kwargs):
            if path.name == 'ledger.json' and path.parent.name == 'qq-outbox':
                raise PermissionError('验收通过公开回执查询，不与 Windows 投递账本原子替换竞争')
            return read_text(path, *args, **kwargs)

        try:
            # 在所有平台保留 Windows 读取拒绝的回归条件；宿主进程的实际写盘不受影响。
            with patch.object(Path, 'read_text', no_concurrent_ledger_read):
                self.run_progress(script, more=['--max-tool-rounds', '8'],
                                  env_extra={'EVE_PROGRESS_STAGE_UPDATES': 'false', 'EVE_PROGRESS_SLOW_AFTER_MS': '0'})
        except Exception as error:
            # HTTP 替身中的断言会变成 503 重试；保留真实工具状态，在主线程报告。
            self.fail(f'文件投递验收未完成：{error}; failures={failures!r}; observed={observed!r}; http_errors={self.errors!r}')
        self.assertFalse(failures, repr(failures))
        self.assertGreaterEqual(len(observed), 2, '至少取得排队结果和一次实际回执查询')
        queued = observed[0]
        self.assertTrue(queued['queued'])
        self.assertFalse(queued['sent'])
        self.assertIsNone(queued['message_id'])
        receipt = observed[-1]
        self.assertEqual(receipt['state'], 'Sent')
        self.assertTrue(receipt['sent'])
        self.assertEqual(receipt['message_id'], 'file-' + queued['id'])
        self.assertEqual(receipt['session_id'], queued['session_id'])
        self.assertTrue(receipt['session_id'])
        self.assertFalse(receipt['retry_allowed'])
        files = [event for event in self.events if event.get('type') == 'file']
        self.assertEqual(len(files), 1, '查询未再次上传文件')
        self.assertEqual(files[0]['id'], queued['id'])
        self.assertEqual(self.receipt('artifact')['state'], 'Sent')


if __name__ == '__main__':
    unittest.main()
