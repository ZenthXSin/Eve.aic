"""真实 Eve 进程：长工具期间自主决定说话；只使用本地模型、QQ 与临时工作区。"""
import hashlib
import json
import sys
import time
import unittest

import dialogue_tools_test as dialogue
import live_dialogue_test as live


PROGRESS_KIND = 'foreground_tool_progress'
PROGRESS_TEXT = '嗯，我正在核对这一步，结果还没有回来。'
INITIAL = '请读取隔离检查脚本，交给编码分工执行并核验实际结果。'


class NaturalProgressAcceptance(live.LiveToolAcceptance):
    for name in dir(live.LiveToolAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        self.tool_started = self.source / 'tool-started'
        self.release_tool = self.source / 'release-tool'
        self.executions = self.source / 'completed-once.txt'
        self.command = {'program': sys.executable, 'args': ['gated-check.py']}
        (self.source / 'gated-check.py').write_text(
            "from pathlib import Path\nimport time\n"
            "Path('tool-started').touch()\n"
            "deadline=time.monotonic()+50\n"
            "while not Path('release-tool').exists():\n"
            "    assert time.monotonic()<deadline, 'fixture gate timed out'\n"
            "    time.sleep(.01)\n"
            "p=Path('completed-once.txt')\n"
            "p.write_text(p.read_text()+'x' if p.exists() else 'x')\n"
            "print('ACTUAL_ISOLATED_CHECK_PASSED')\n", encoding='utf8')
        self.main_requests = []
        self.child_requests = []
        self.completed_main = None

    def run_natural(self, script, **options):
        env = {
            'EVE_DEVELOPER_CONFIG': str(self.developer_config),
            'EVE_PROGRESS_STAGE_UPDATES': 'false',
            'EVE_PROGRESS_SLOW_AFTER_MS': '0',
            'EVE_PROGRESS_TOOL_MILESTONES': 'false',
            'EVE_MODEL_USE_CODING_ROLE': 'auxiliary_small',
            'EVE_MODELS_AUXILIARY_SMALL_ENABLED': 'true',
            'EVE_MODELS_AUXILIARY_SMALL_PROVIDER': 'openai',
            'EVE_MODELS_AUXILIARY_SMALL_MODEL': 'local-coding-fixture',
            'EVE_MODELS_AUXILIARY_SMALL_CREDENTIAL_REF': 'env:EVE_AUXILIARY_API_KEY',
            'EVE_AUXILIARY_API_KEY': 'local-coding-fixture-key',
            'EVE_OPENAI_AUXILIARY_BASE_URL': f'http://127.0.0.1:{self.server.server_port}',
            'EVE_OPENAI_AUXILIARY_PROTOCOL': 'chat',
            'EVE_OPENAI_AUXILIARY_REASONING_EFFORT': 'high',
        }
        env.update(options.pop('env_extra', {}))
        return self.run_live(script, more=['--subagents'], env_extra=env, **options)

    def progress_rows(self):
        return [(event, state, decision) for event, state, decision in self.speech_rows()
                if event['facts'].get('kind') == PROGRESS_KIND]

    def wait_progress(self, state):
        # 生产阈值为八秒，给真实进程调度和平台回执留出余量；不修改运行时阈值。
        deadline = time.monotonic() + 20
        while not any(status == state for _, status, _ in self.progress_rows()):
            self.assertLess(time.monotonic(), deadline, '自然进度没有进入 ' + state)
            self.assertFalse(self.run_release.wait(.01))

    def outbound_progress(self):
        return [event for event in self.events
                if event.get('direction') == 'out' and event.get('type') == 'passive_push'
                and event.get('foreground') is True]

    def handoffs(self, body):
        found = []
        for message in body['messages']:
            if message['role'] != 'system':
                continue
            for line in (message.get('content') or '').splitlines():
                try:
                    decoded = json.loads(line)
                except ValueError:
                    continue
                if isinstance(decoded, list):
                    found.extend(item for item in decoded if isinstance(item, dict)
                                 and item.get('event_id', '').startswith('foreground-progress:'))
        return found

    def decide(self, facts, after_ms=0):
        if facts['host_event'].get('kind') != PROGRESS_KIND:
            return {'action': 'silence'}
        self.assertEqual(facts['trigger'], 'background_event')
        self.assertTrue(facts['event_id'].startswith('foreground-progress:'))
        self.assertNotIn('current_user_message', facts)
        self.assertFalse(facts['host_event']['input_pending'])
        self.assertEqual(facts['host_event']['tool_state'], 'awaiting_results')
        if self.child_requests:
            self.assertEqual(facts['host_event']['delegated_tools'], [{
                'submitted_calls': 1, 'returned_calls': 0, 'failed_calls': 0,
                'unknown_calls': 0, 'pending_tool_names': ['run_command'],
                'last_returned_tool_name': None,
            }], '父轮必须提供真实子代理工具状态，而不是只显示 run_subagent')
            encoded = json.dumps(facts['host_event'], ensure_ascii=False)
            for private in ('gated-check.py', str(self.source), sys.executable,
                            'ACTUAL_ISOLATED_CHECK_PASSED', 'deadline=time.monotonic()'):
                self.assertNotIn(private, encoded, '子工具进度不得泄漏参数、源码或正文')
        self.assertTrue(self.tool_started.exists(), '实际工具必须已启动')
        self.assertFalse(self.release_tool.exists(), '决定发生在工具尚未结束时')
        self.assertFalse(self.executions.exists(), '不能把仍在运行的工具说成已完成')
        return {'action': 'speak', 'text': PROGRESS_TEXT, 'after_ms': after_ms}

    def install_task(self, delegated=False, expected_delivery=None, expected_followup=None):
        def respond(body):
            leaf = '你是 Eve 的子代理' in body['messages'][0]['content']
            (self.child_requests if leaf else self.main_requests).append(body)
            results = self.all_results(body)
            if not results:
                if delegated and not leaf:
                    return self.choose('run_subagent', {
                        'agent_id': 'coding', 'include_context': True, 'max_steps': 4,
                        'task': '在当前临时工作区运行 gated-check.py，等待真实完成，再读取 completed-once.txt 核验产物。',
                        'verification': {'kind': 'receipts', 'contract': {'checks': [{
                            'tool': 'read_file', 'arguments': {'path': 'completed-once.txt'},
                            'output_pointer': '/sha256',
                            'expected': hashlib.sha256(b'x').hexdigest(),
                        }], 'require_final_batch': True}},
                    })
                return self.choose('run_command', self.command)
            if leaf:
                self.assertEqual(body['model'], 'local-coding-fixture')
                if len(results) == 1:
                    self.assertTrue(results[-1]['success'], results[-1])
                    self.assertIn('ACTUAL_ISOLATED_CHECK_PASSED', results[-1]['stdout'])
                    return self.choose('read_file', {'path': 'completed-once.txt'})
                self.assertEqual(results[-1]['content'], 'x')
                self.assertEqual(results[-1]['sha256'], hashlib.sha256(b'x').hexdigest())
                return dialogue.text_response('隔离检查已实际执行通过。')
            if delegated:
                self.assertTrue(results[-1]['completed'], results[-1])
                self.assertEqual(results[-1]['usage']['agent_id'], 'coding')
                self.assertEqual(results[-1]['usage']['steps_used'], 2)
            else:
                self.assertTrue(results[-1]['success'], results[-1])
            self.assertEqual(self.executions.read_text(), 'x')
            if expected_delivery:
                handoff = self.handoffs(body)
                self.assertTrue(handoff, '主轮继续时必须收到自然发言的真实投递状态')
                latest = handoff[-1]
                self.assertEqual(latest['kind'], 'progress')
                self.assertEqual(latest['input_message_id'], 'working')
                self.assertEqual(latest['input_text'], INITIAL)
                self.assertEqual(latest['reply_text'], PROGRESS_TEXT)
                self.assertEqual(latest['delivery']['state'], expected_delivery)
                if expected_delivery == 'sent':
                    self.assertTrue(latest['delivery']['platform_message_id'])
            if expected_followup:
                self.assertIn(expected_followup, self.users(body))
            else:
                self.assertEqual(self.users(body), [INITIAL], '自然进度不能伪造新的用户输入')
            self.completed_main = body
            return dialogue.text_response('内部收尾：已核验，不重复发送途中那句话。')
        self.callback = respond

    def test_coding_continues_after_natural_delivery_without_another_user_message(self):
        self.install_task(delegated=True, expected_delivery='sent')
        self.speech_callback = self.decide

        def delivered_while_running():
            self.wait_progress('sent')
            self.assertFalse(self.executions.exists())
            self.assertEqual(len(self.main_requests), 1)
            self.assertEqual(len(self.child_requests), 1)
            self.assertEqual(self.receipt('working')['state'], 'Processing')

        self.run_natural([
            {'send': self.message('working', INITIAL)}, {'wait_file': str(self.tool_started)},
            *self.checkpoint('delivered-while-running'), {'touch': str(self.release_tool)},
            self.wait_receipt('working'), self.wait_command('working', 'finish'),
        ], checkpoints={'delivered-while-running': delivered_while_running})
        self.assertIsNotNone(self.completed_main)
        self.assertEqual(len(self.main_requests), 2)
        self.assertEqual(len(self.child_requests), 3)
        self.assertEqual([event['text'] for event in self.outbound_progress()], [PROGRESS_TEXT])
        self.assertEqual(len(self.progress_rows()), 1)
        self.assertEqual(len(self.speech_requests), 1, '同一批工具不得不断触发发言判断')

    def test_slow_preparation_past_passive_window_still_allows_actual_progress(self):
        self.install_task(expected_delivery='sent')
        task_response = self.callback
        first = True

        def slow_preparation(body):
            nonlocal first
            if first:
                first = False
                # 真实等待跨越平台被动窗口；生产没有测试专用时钟或放宽窗口开关。
                self.assertFalse(self.run_release.wait(241))
            return task_response(body)

        self.callback = slow_preparation
        self.speech_callback = self.decide

        def progress_after_window():
            deadline = time.monotonic() + 270
            while not self.tool_started.exists():
                self.assertLess(time.monotonic(), deadline, '长准备后工具未启动')
                self.assertFalse(self.run_release.wait(.05))
            self.wait_progress('sent')
            self.assertFalse(self.executions.exists())
            self.assertEqual(len(self.main_requests), 1)
            events = [json.loads(line) for line in
                      (self.work / f'events-{self.runs}.jsonl').read_text().splitlines()]
            outputs = [event for event in events if event.get('direction') == 'out'
                       and event.get('type') in ('push', 'passive_push')]
            self.assertEqual([event['type'] for event in outputs], ['push'],
                             '过期后应走独立主动通道，不能延长或伪造原被动窗口')
            self.assertEqual(outputs[0]['based_on_message_id'], 'working')
            self.assertEqual(outputs[0]['text'], PROGRESS_TEXT)
            self.assertEqual(len(self.speech_requests), 1)

        self.run_natural([
            {'send': self.message('working', INITIAL)},
            *self.checkpoint('prepared-after-window'), {'touch': str(self.release_tool)},
            self.wait_receipt('working'), self.wait_command('working', 'finish'),
        ], checkpoints={'prepared-after-window': progress_after_window}, timeout=290,
            env_extra={'EVE_OPENAI_TIMEOUT_SECONDS': '270', 'EVE_MODELS_PRIMARY_TIMEOUT_MS': '270000'})
        self.assertIsNotNone(self.completed_main)
        self.assertEqual(self.executions.read_text(), 'x')
        self.assertEqual(len(self.progress_rows()), 1)
        self.assertEqual(len(self.speech_requests), 1)

    def test_delayed_speak_uses_the_saved_decision_once_without_new_input(self):
        self.install_task(expected_delivery='sent')
        self.speech_callback = lambda facts: self.decide(facts, after_ms=2500)

        def saved_but_not_sent():
            self.wait_progress('ready')
            events = self.work / f'events-{self.runs}.jsonl'
            self.assertFalse(any(json.loads(line).get('type') == 'passive_push'
                                 for line in events.read_text().splitlines()))
            self.assertFalse(self.release_tool.exists())
            self.assertEqual(len(self.speech_requests), 1)

        def delivered_while_running():
            self.wait_progress('sent')
            self.assertFalse(self.executions.exists())
            self.assertEqual(len(self.main_requests), 1)
            self.assertEqual(len(self.speech_requests), 1,
                             '延迟到点只能使用已有决定，不能再调用发言模型')

        self.run_natural([
            {'send': self.message('working', INITIAL)}, {'wait_file': str(self.tool_started)},
            *self.checkpoint('saved-before-due'), *self.checkpoint('delivered-after-due'),
            {'touch': str(self.release_tool)}, self.wait_receipt('working'),
            self.wait_command('working', 'finish'),
        ], checkpoints={'saved-before-due': saved_but_not_sent,
                        'delivered-after-due': delivered_while_running})
        self.assertEqual([event['text'] for event in self.outbound_progress()], [PROGRESS_TEXT])
        self.assertEqual(len(self.progress_rows()), 1)
        self.assertEqual(len(self.speech_requests), 1)
        self.assertIsNotNone(self.completed_main)

    def test_wait_does_not_reconsider_unchanged_tools_and_business_still_completes(self):
        self.install_task()

        def wait_decision(facts):
            self.decide(facts)
            return {'action': 'wait', 'after_ms': 1000}

        self.speech_callback = wait_decision

        def wait_without_new_facts():
            self.wait_progress('waiting')
            # 超过返回的到期时间与两次原两秒扫描；仅断言不重复请求，不用等待猜测成功。
            self.assertFalse(self.run_release.wait(4.5))
            self.assertFalse(self.release_tool.exists())
            self.assertFalse(self.executions.exists())
            self.assertEqual(len(self.main_requests), 1)
            self.assertEqual(len(self.speech_requests), 1,
                             '相同活动快照的 wait 不应变成周期性模型轮询')

        self.run_natural([
            {'send': self.message('working', INITIAL)}, {'wait_file': str(self.tool_started)},
            *self.checkpoint('wait-without-new-facts'), {'touch': str(self.release_tool)},
            self.wait_receipt('working'), self.wait_command('working', 'finish'),
        ], checkpoints={'wait-without-new-facts': wait_without_new_facts})
        self.assertIsNotNone(self.completed_main)
        self.assertEqual(self.executions.read_text(), 'x')
        self.assertEqual(len(self.main_requests), 2)
        self.assertEqual(len(self.speech_requests), 1)
        self.assertFalse(self.outbound_progress())

    def test_explicit_quiet_request_keeps_work_running_without_progress_judgment(self):
        def respond(body):
            self.main_requests.append(body)
            results = self.all_results(body)
            if not results:
                return self.choose('run_command', self.command)
            self.assertTrue(results[-1]['success'], results[-1])
            self.assertEqual(self.executions.read_text(), 'x')
            return dialogue.text_response('已完成内部核对，保持安静。')

        self.callback = respond
        self.speech_callback = self.decide

        def quiet_while_working():
            self.assertFalse(self.run_release.wait(11))
            self.assertFalse(self.executions.exists())
            self.assertEqual(len(self.main_requests), 1)
            self.assertEqual(self.speech_requests, [])
            self.assertEqual(self.progress_rows(), [])

        self.run_natural([
            {'send': self.message('working', '执行隔离检查，保持安静，不用汇报。')},
            {'wait_file': str(self.tool_started)}, *self.checkpoint('quiet-while-working'),
            {'touch': str(self.release_tool)}, self.wait_receipt('working'),
            self.wait_command('working', 'finish'),
        ], checkpoints={'quiet-while-working': quiet_while_working})
        self.assertEqual(len(self.main_requests), 2)
        self.assertEqual(self.speech_requests, [])
        self.assertFalse(self.outbound_progress())

    def test_new_input_retires_a_delayed_progress_decision_and_the_tool_runs_once(self):
        followup = '先别说进度，等这次实际检查结束。'
        self.install_task(expected_followup=followup)
        self.speech_callback = lambda facts: self.decide(facts, after_ms=3000)
        self.run_natural([
            {'send': self.message('working', INITIAL)}, {'wait_file': str(self.tool_started)},
            *self.checkpoint('decision-ready'),
            {'send': self.message('new-input', followup)}, self.wait_receipt('new-input', 'Merged'),
            {'touch': str(self.release_tool)}, self.wait_receipt('working'),
            self.wait_command('working', 'finish'), {'delay_ms': 3200},
        ], checkpoints={'decision-ready': lambda: self.wait_progress('ready')})
        self.assertEqual(self.executions.read_text(), 'x')
        self.assertIsNotNone(self.completed_main)
        self.assertFalse(self.outbound_progress(), '新原话必须使尚未发送的旧进度失效')
        self.assertTrue(all(state == 'stale' for _, state, _ in self.progress_rows()))

    def test_cancel_retires_a_delayed_progress_decision_without_sending_it(self):
        self.install_task()
        self.speech_callback = lambda facts: self.decide(facts, after_ms=3000)
        self.run_natural([
            {'send': self.message('working', INITIAL)}, {'wait_file': str(self.tool_started)},
            *self.checkpoint('decision-ready'), *self.send(self.message('cancel', '/cancel')),
            self.wait_receipt('working', 'Failed'), self.wait_command('working', 'finish'),
            {'touch': str(self.release_tool)}, {'delay_ms': 3200},
        ], checkpoints={'decision-ready': lambda: self.wait_progress('ready')})
        self.assertFalse(self.outbound_progress())
        self.assertEqual(len(self.main_requests), 1, '取消后不得再次运行主模型')
        self.assertTrue(all(state == 'stale' for _, state, _ in self.progress_rows()))

    def test_unknown_progress_is_visible_to_main_and_never_replayed_after_restart(self):
        self.install_task(expected_delivery='unknown')
        self.speech_callback = self.decide
        message = self.message('working', INITIAL)
        self.run_natural([
            {'push_mode': {'ok': False, 'failure_kind': 'timeout'}}, {'send': message},
            {'wait_file': str(self.tool_started)}, *self.checkpoint('unknown-delivery'),
            {'touch': str(self.release_tool)}, self.wait_receipt('working'),
            self.wait_command('working', 'finish'),
        ], checkpoints={'unknown-delivery': lambda: self.wait_progress('unknown')})
        self.assertEqual(self.executions.read_text(), 'x')
        self.assertEqual(len(self.outbound_progress()), 1)
        original_rows = self.progress_rows()
        self.assertEqual([state for _, state, _ in original_rows], ['unknown'])
        original_calls = len(self.speech_requests)
        journal = self.work / 'state/developer-tools/operations.jsonl'
        original_journal = journal.read_bytes()
        self.callback = lambda _: dialogue.text_response('这次只记录输入，不重发旧话。')
        self.speech_callback = lambda _: self.fail('重启不能重新判断已投递结果未知的进度')
        self.run_natural([
            {'send': message}, self.wait_command('working', 'finish'),
            *self.send(self.message('fresh', '继续聊别的，原检查不用重做。')),
        ])
        self.assertEqual(self.progress_rows(), original_rows)
        self.assertEqual(len(self.speech_requests), original_calls)
        self.assertEqual(journal.read_bytes(), original_journal)
        self.assertEqual(self.executions.read_text(), 'x')
        self.assertFalse(self.outbound_progress())


if __name__ == '__main__':
    unittest.main(verbosity=2)
