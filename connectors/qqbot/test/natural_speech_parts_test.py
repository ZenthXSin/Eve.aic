"""自然发言的真实宿主分条验收；模型、QQ 和业务文件都在本地临时目录。

只验证明确选择 parts 后的投递、设置、持久化和交接，不据此宣称模型会
自然分条或已采用蒸馏经验。复用原进程夹具，不重跑其已有单条测试。
"""
from contextlib import closing
import json
import sqlite3
import time
import unittest

import dialogue_tools_test as dialogue
import live_reminder_test as reminder
import natural_progress_test as natural


INPUT_KIND = 'foreground_input_pending'
PARTS = ['第一项回执还没有回来。', '我在核对它对应的记录。', '有确切结果后再接着说。']


class NaturalSpeechPartsAcceptance(natural.NaturalProgressAcceptance):
    for name in dir(natural.NaturalProgressAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def run_parts(self, script, *, segmented=True, **options):
        env = {
            'EVE_DEVELOPER_CONFIG': str(self.developer_config),
            'EVE_PROGRESS_STAGE_UPDATES': 'false',
            'EVE_PROGRESS_SLOW_AFTER_MS': '0',
            'EVE_PROGRESS_TOOL_MILESTONES': 'false',
        }
        env.update(options.pop('env_extra', {}))
        return self.run_live(script, more=['--segmented'] if segmented else [],
                             env_extra=env, **options)

    def wait_until(self, predicate, message):
        # 自然进度保留真实八秒门槛；杀进程门控还要等首条回执和第二条派发。
        deadline = time.monotonic() + 25
        while not predicate():
            self.assertLess(time.monotonic(), deadline, message)
            self.assertFalse(self.run_release.wait(.01), message)

    def speech_records(self, kind=natural.PROGRESS_KIND):
        path = self.work / 'state/speech.sqlite3'
        if not path.exists():
            return []
        with closing(sqlite3.connect(path)) as db:
            try:
                rows = db.execute('SELECT event,state,decision,receipt FROM speech').fetchall()
            except sqlite3.OperationalError as error:
                if 'no such table' in str(error):
                    return []
                raise
        return [
            {'event': event, 'state': state,
             'decision': json.loads(decision) if decision else None,
             'receipt': json.loads(receipt) if receipt else None}
            for encoded, state, decision, receipt in rows
            if (event := json.loads(encoded))['facts'].get('kind') == kind
        ]

    def speech_record(self, kind=natural.PROGRESS_KIND):
        records = self.speech_records(kind)
        self.assertEqual(len(records), 1, '同一发言应保留一项事件，不把后段伪造为新事件')
        return records[0]

    def live_outputs(self):
        path = self.work / f'events-{self.runs}.jsonl'
        if not path.exists():
            return []
        return [event for line in path.read_text().splitlines()
                if (event := json.loads(line)).get('direction') == 'out'
                and event.get('type') in ('push', 'passive_push')]

    def wait_parts(self, states, *, kind=natural.PROGRESS_KIND, parent=None, texts=PARTS):
        def reached():
            records = self.speech_records(kind)
            if len(records) != 1 or not records[0]['receipt']:
                return False
            receipt = records[0]['receipt']
            delivery = receipt.get('delivery', {})
            return (delivery.get('state') == 'parts'
                    and [part['delivery']['state'] for part in delivery['parts']] == states
                    and (parent is None or records[0]['state'] == parent))
        self.wait_until(reached, '分条回执未达到 ' + repr(states))
        return self.assert_receipt(states, kind=kind, parent=parent, texts=texts)

    def assert_receipt(self, states, *, kind=natural.PROGRESS_KIND,
                       parent=None, texts=PARTS):
        row = self.speech_record(kind)
        if parent is not None:
            self.assertEqual(row['state'], parent)
        receipt = row['receipt']
        self.assertEqual(receipt['version'], 2)
        self.assertEqual([part['text'] for part in receipt['plan']], texts)
        self.assert_delivery(receipt['delivery'], states, texts)
        with closing(sqlite3.connect(self.work / 'state/speech.sqlite3')) as db:
            self.assertEqual(db.execute('PRAGMA user_version').fetchone()[0], 2)
        return row

    def assert_delivery(self, delivery, states, texts):
        self.assertEqual(set(delivery), {'state', 'parts'},
                         '整体不能套用首条平台 ID 或把终止冒充成功')
        self.assertEqual(delivery['state'], 'parts')
        self.assertEqual([part['index'] for part in delivery['parts']], list(range(len(texts))))
        self.assertEqual([part['text'] for part in delivery['parts']], texts)
        self.assertEqual([part['delivery']['state'] for part in delivery['parts']], states)
        ids = []
        for part in delivery['parts']:
            detail = part['delivery']
            if detail['state'] == 'sent':
                self.assertTrue(detail.get('platform_message_id'))
                ids.append(detail['platform_message_id'])
            else:
                self.assertIsNone(detail.get('platform_message_id'))
        self.assertEqual(len(ids), len(set(ids)), '每条成功都要有独立的真实平台回执')

    def assert_handoff(self, body, *, kind=natural.PROGRESS_KIND,
                       states=None, texts=PARTS):
        row = self.speech_record(kind)
        matches = []
        for message in body['messages']:
            if message['role'] != 'system':
                continue
            for line in (message.get('content') or '').splitlines():
                try:
                    value = json.loads(line)
                except ValueError:
                    continue
                if isinstance(value, list):
                    matches.extend(item for item in value if isinstance(item, dict)
                                   and item.get('event_id') == row['event']['id'])
        self.assertTrue(matches, '主轮必须收到同一事件的完整有序交接')
        previous = None
        for handoff in matches:
            # runtime 保留此前已见的 System 快照；同事件新回执追加历史不是新事件。
            self.assertEqual(handoff['input_message_id'], row['event']['anchor'])
            self.assertEqual(handoff['reply_text'], '\n\n'.join(texts))
            if kind == natural.PROGRESS_KIND:
                self.assertEqual(handoff['kind'], 'progress')
            else:
                self.assertNotEqual(handoff.get('kind'), 'progress',
                                    '直接回应追加输入不能伪装成新进度事件')
            parts = handoff['delivery']['parts']
            self.assert_delivery(handoff['delivery'],
                                 [part['delivery']['state'] for part in parts], texts)
            if previous is not None:
                for old, new in zip(previous, parts):
                    if old['delivery']['state'] in ('sent', 'unknown', 'failed', 'not_attempted'):
                        self.assertEqual(old, new, '已见的终态凭据不能被后续快照改写')
                    if old['delivery']['state'] == 'sending':
                        self.assertNotEqual(new['delivery']['state'], 'planned')
            previous = parts
        handoff = matches[-1]
        if states is None:
            states = [part['delivery']['state'] for part in handoff['delivery']['parts']]
        self.assert_delivery(handoff['delivery'], states, texts)
        return handoff

    def decide_parts(self, facts, *, texts=PARTS, delay=1800, kind=natural.PROGRESS_KIND):
        if facts['host_event'].get('kind') != kind:
            return {'action': 'silence'}
        if kind == natural.PROGRESS_KIND:
            self.decide(facts)  # 核对真实工具运行中、后台触发、没有假造新输入。
        else:
            self.assertEqual(facts['trigger'], 'user_input')
            self.assertTrue(facts['host_event']['input_pending'])
            self.assertTrue(self.tool_started.exists())
            self.assertFalse(self.executions.exists())
        return {'action': 'speak', 'after_ms': 0, 'parts': [
            {'text': text, 'after_ms': 0 if index == 0 else delay}
            for index, text in enumerate(texts)
        ]}

    def install_parts_task(self, *, states=None, texts=PARTS, followup=None):
        self.install_task(expected_followup=followup)
        original = self.callback

        def respond(body):
            result = original(body)
            if self.all_results(body):
                self.assert_handoff(body, states=states, texts=texts)
            return result
        self.callback = respond

    def first_sent(self, count=3, *, kind=natural.PROGRESS_KIND):
        self.wait_parts(['sent'] + ['planned'] * (count - 1), kind=kind, parent='sending')
        self.assertFalse(self.executions.exists())
        self.assertEqual([event['text'] for event in self.live_outputs()], [PARTS[0]])

    def finish_work(self):
        return [{'touch': str(self.release_tool)}, self.wait_receipt('working'),
                self.wait_command('working', 'finish')]

    def wait_command_silence(self, message_id):
        # 命令回执 Handled/finish 之后还会有独立的宿主事实发言判断。
        # 先等该判断落盘再继续测试，避免桥在业务结束时抢先 EOF。
        self.wait_until(lambda: any(event['id'] == f'host-reply:{message_id}' and state == 'silence'
                                   for event, state, _ in self.speech_rows()),
                        '命令的独立沉默决定尚未落盘：' + message_id)

    def assert_outputs(self, texts, *, anchor='working'):
        outputs = self.live_outputs()
        self.assertEqual([event['text'] for event in outputs], texts)
        self.assertEqual(len({event['id'] for event in outputs}), len(outputs))
        self.assertTrue(all(event.get('based_on_message_id') == anchor for event in outputs))

    def test_real_tool_progress_persists_each_part_and_returns_one_complete_handoff(self):
        self.install_parts_task(states=['sent'] * 3)
        self.speech_callback = self.decide_parts
        self.run_parts([
            {'send': self.message('working', natural.INITIAL)},
            {'wait_file': str(self.tool_started)}, *self.checkpoint('first-sent'),
            *self.checkpoint('all-sent'), *self.finish_work(),
        ], checkpoints={'first-sent': self.first_sent,
                        'all-sent': lambda: self.wait_parts(['sent'] * 3, parent='sent')})
        self.assert_outputs(PARTS)
        self.assertEqual(len(self.speech_requests), 1)
        self.assertEqual(self.executions.read_text(), 'x')
        self.assertEqual(len(self.main_requests), 2)

    def test_completed_main_turn_keeps_its_original_window_until_parts_finish(self):
        self.install_parts_task(states=['sent', 'planned', 'planned'])
        self.speech_callback = lambda facts: self.decide_parts(facts, delay=3000)

        def completed_before_suffix():
            self.assertEqual(self.turn(natural.INITIAL)['status']['state'], 'Completed')
            self.assertIsNotNone(self.completed_main)
            self.assert_receipt(['sent', 'planned', 'planned'], parent='sending')
            path = self.work / f'events-{self.runs}.jsonl'
            frames = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertFalse(any(frame.get('direction') == 'out' and frame.get('id') == 'working'
                                 and frame.get('type') in ('finish', 'close_foreground')
                                 for frame in frames), '不能在分条结束前关闭原 Node 窗口')
            self.wait_parts(['sent'] * 3, parent='sent')

        self.run_parts([
            {'send': self.message('working', natural.INITIAL)},
            {'wait_file': str(self.tool_started)}, *self.checkpoint('first-sent'),
            {'touch': str(self.release_tool)}, self.wait_receipt('working'),
            *self.checkpoint('completed-before-suffix'), self.wait_command('working', 'finish'),
        ], checkpoints={'first-sent': self.first_sent,
                        'completed-before-suffix': completed_before_suffix})
        self.assert_outputs(PARTS)
        self.assertEqual(len(self.speech_requests), 1)
        self.assertEqual(len(self.main_requests), 2)
        self.assertEqual(self.executions.read_text(), 'x')

    def test_pending_input_parts_survive_normal_consumption_by_the_original_turn(self):
        followup = '现在看到哪一步了？'
        kind = INPUT_KIND

        def respond(body):
            self.main_requests.append(body)
            results = self.all_results(body)
            if not results:
                return self.choose('run_command', self.command)
            self.assertEqual(self.users(body), [natural.INITIAL, followup])
            if len(results) == 1:
                self.assertTrue(results[0]['success'])
                # 请求已包含追加原话，证明 runtime 真正消费了输入；此时余段仍未发。
                self.assert_handoff(body, kind=kind, states=['sent', 'planned', 'planned'])
                self.wait_parts(['sent'] * 3, kind=kind, parent='sent')
                return self.choose('echo', {'text': '已核对完整交接'})
            self.assert_handoff(body, kind=kind, states=['sent'] * 3)
            self.completed_main = body
            return dialogue.text_response('内部结束，不重发已经说过的三条。')

        self.callback = respond
        self.speech_callback = lambda facts: self.decide_parts(facts, kind=kind, delay=3000)
        self.run_parts([
            {'send': self.message('working', natural.INITIAL)},
            {'wait_file': str(self.tool_started)},
            {'send': self.message('followup', followup)}, self.wait_receipt('followup', 'Merged'),
            *self.checkpoint('pending-first-sent'), *self.finish_work(),
        ], checkpoints={'pending-first-sent': lambda: self.first_sent(kind=kind)})
        self.assert_outputs(PARTS, anchor='followup')
        self.assertIsNotNone(self.completed_main)
        self.assertEqual(self.executions.read_text(), 'x')
        self.assertEqual(len(self.main_requests), 3)
        self.assertEqual(len(self.speech_records(kind)), 1)

    def stopped_by_delivery(self, delivery):
        states = ['sent', delivery, 'not_attempted']
        self.install_parts_task(states=states)
        self.speech_callback = self.decide_parts
        mode = {'ok': False, 'failure_kind': 'timeout' if delivery == 'unknown' else 'api_error'}
        self.run_parts([
            {'send': self.message('working', natural.INITIAL)},
            {'wait_file': str(self.tool_started)}, *self.checkpoint('first-sent'),
            {'push_mode': mode}, *self.checkpoint('stopped'), *self.finish_work(),
        ], checkpoints={'first-sent': self.first_sent,
                        'stopped': lambda: self.wait_parts(states, parent=delivery)})
        self.assert_outputs(PARTS[:2])
        self.assertEqual(len(self.speech_requests), 1)
        self.assertEqual(self.executions.read_text(), 'x')
        before = self.speech_record()
        journal = self.work / 'state/developer-tools/operations.jsonl'
        before_journal = journal.read_bytes()
        self.callback = lambda _: dialogue.text_response('新话题内部结束。')
        self.speech_callback = lambda _: self.fail('已停止的分条不能重审或重放')
        self.run_parts([
            {'send': self.message('working', natural.INITIAL)},
            self.wait_command('working', 'finish'),
            *self.send(self.message('fresh', '接下来看看其他事情。')),
        ])
        self.assertEqual(self.speech_record(), before)
        self.assertEqual(journal.read_bytes(), before_journal)
        self.assert_outputs([])

    def test_unknown_second_part_stops_suffix_and_restart_does_not_replay_prefix(self):
        self.stopped_by_delivery('unknown')

    def test_failed_second_part_stops_suffix_and_restart_does_not_retry_the_group(self):
        self.stopped_by_delivery('failed')

    def interrupted_between_parts(self, *, cancel=False):
        followup = '情况有变化，先别接着说旧进度。'
        if cancel:
            self.install_task()
        else:
            self.install_parts_task(states=['sent', 'not_attempted', 'not_attempted'],
                                    followup=followup)
        self.speech_callback = lambda facts: self.decide_parts(facts, delay=3000)
        interruption = (self.send(self.message('cancel', '/cancel')) if cancel else [
            {'send': self.message('changed', followup)}, self.wait_receipt('changed', 'Merged')])
        ending = ([self.wait_receipt('working', 'Failed'), self.wait_command('working', 'finish'),
                   {'touch': str(self.release_tool)}] if cancel else self.finish_work())
        self.run_parts([
            {'send': self.message('working', natural.INITIAL)},
            {'wait_file': str(self.tool_started)}, *self.checkpoint('first-sent'),
            *interruption, *self.checkpoint('suffix-stopped'), *ending,
        ], checkpoints={'first-sent': self.first_sent,
                        'suffix-stopped': lambda: self.wait_parts(
                            ['sent', 'not_attempted', 'not_attempted'], parent='failed')})
        self.assert_outputs(PARTS[:1])
        if cancel:
            self.assertEqual(len(self.main_requests), 1, '取消不得再次发起业务模型调用')
        else:
            self.assertEqual(self.executions.read_text(), 'x')

    def test_new_input_between_parts_preserves_sent_prefix_and_stops_old_suffix(self):
        self.interrupted_between_parts()

    def test_cancel_between_parts_preserves_sent_prefix_without_sending_the_suffix(self):
        self.interrupted_between_parts(cancel=True)

    def test_restart_with_second_part_sending_marks_unknown_and_never_continues(self):
        self.install_task()
        original = self.callback

        def hold_after_tool(body):
            result = original(body)
            if self.all_results(body):
                self.gate('main-consumed-tool').touch()
                self.run_release.wait(15)
            return result

        self.callback = hold_after_tool
        self.speech_callback = self.decide_parts
        stop = self.gate('stop-owned-process')

        def sending_without_receipt():
            self.wait_parts(['sent', 'sending', 'planned'], parent='sending')
            self.wait_until(lambda: len(self.live_outputs()) == 2, '第二条尚未交给真实桥接')
            self.release_tool.touch()
            self.wait_until(self.executions.exists, '停止宿主前隔离业务进程必须已收尾')
            self.wait_until(self.gate('main-consumed-tool').exists, '主轮尚未收到工具完成回执')
            self.assertEqual(self.executions.read_text(), 'x')

        self.run_parts([
            {'send': self.message('working', natural.INITIAL)},
            {'wait_file': str(self.tool_started)}, *self.checkpoint('first-sent'),
            {'push_mode': {'hold': True}}, *self.checkpoint('second-sending'),
            {'touch': str(stop)}, {'wait_file': str(self.gate('never-release'))},
        ], checkpoints={'first-sent': self.first_sent,
                        'second-sending': sending_without_receipt}, stop_at=stop)
        self.assert_receipt(['sent', 'sending', 'planned'], parent='sending')
        self.assert_outputs(PARTS[:2])
        before = self.speech_record()['receipt']['delivery']['parts'][0]
        self.callback = lambda _: dialogue.text_response('新输入已收到，不重做原检查。')
        self.speech_callback = lambda _: self.fail('重启不能再次调用旧分条的发言判断')
        self.run_parts([
            *self.send(self.message('fresh', '原检查不用重做，换个话题。')),
            *self.checkpoint('recovered'),
        ], checkpoints={'recovered': lambda: self.wait_parts(
            ['sent', 'unknown', 'not_attempted'], parent='unknown')})
        self.assertEqual(self.speech_record()['receipt']['delivery']['parts'][0], before)
        self.assert_outputs([])
        self.assertEqual(self.executions.read_text(), 'x')

    def test_current_segment_off_combines_natural_parts_without_losing_text(self):
        merged = ['\n\n'.join(PARTS)]
        self.install_parts_task(states=['sent'], texts=merged)
        self.speech_callback = self.decide_parts
        self.run_parts([
            *self.send(self.message('segment-off', '/segment off')),
            *self.checkpoint('segment-off-silence'),
            {'send': self.message('working', natural.INITIAL)},
            {'wait_file': str(self.tool_started)}, *self.checkpoint('merged-sent'),
            *self.finish_work(),
        ], checkpoints={'segment-off-silence': lambda: self.wait_command_silence('segment-off'),
                        'merged-sent': lambda: self.wait_until(
            lambda: bool(self.speech_records()) and self.speech_record()['state'] == 'sent',
            '关闭分段后的完整消息没有送达')})
        self.assert_receipt(['sent'], parent='sent', texts=merged)
        self.assert_outputs(merged)

    def test_default_part_limit_merges_the_tail_without_dropping_requested_text(self):
        requested = PARTS + ['另外那条线索尚未证实。']
        actual = PARTS[:2] + ['\n\n'.join(requested[2:])]
        self.install_parts_task(states=['sent'] * 3, texts=actual)
        self.speech_callback = lambda facts: self.decide_parts(facts, texts=requested, delay=1000)
        self.run_parts([
            {'send': self.message('working', natural.INITIAL)},
            {'wait_file': str(self.tool_started)}, *self.checkpoint('limited-sent'),
            *self.finish_work(),
        ], checkpoints={'limited-sent': lambda: self.wait_until(
            lambda: bool(self.speech_records()) and self.speech_record()['state'] == 'sent',
            '默认上限合并后的三条消息没有送达')})
        self.assert_receipt(['sent'] * 3, parent='sent', texts=actual)
        self.assertEqual(self.speech_record()['decision']['parts'], [
            {'text': text, 'after_ms': 0 if index == 0 else 1000}
            for index, text in enumerate(requested)])
        self.assert_outputs(actual)

    def test_pending_input_cannot_upgrade_fifth_part_after_its_real_window_is_used(self):
        followup = '把眼下能确认的情况分别说说。'
        texts = PARTS + ['尚未确定的地方先留着。', '这条超过真实窗口余额，不应投递。']
        states = ['sent'] * 4 + ['not_attempted']

        def respond(body):
            self.main_requests.append(body)
            if not self.all_results(body):
                return self.choose('run_command', self.command)
            self.assertEqual(self.users(body), [natural.INITIAL, followup])
            self.assertTrue(self.all_results(body)[0]['success'])
            self.assert_handoff(body, kind=INPUT_KIND, states=states, texts=texts)
            return dialogue.text_response('内部结束，窗口不足不是发送成功。')

        self.callback = respond
        self.speech_callback = lambda facts: self.decide_parts(
            facts, kind=INPUT_KIND, texts=texts, delay=500)
        self.run_parts([
            *self.send(self.message('segment-five', '/segment parts 5')),
            *self.checkpoint('segment-five-silence'),
            {'send': self.message('working', natural.INITIAL)},
            {'wait_file': str(self.tool_started)},
            {'send': self.message('followup', followup)}, self.wait_receipt('followup', 'Merged'),
            *self.checkpoint('window-used'), *self.finish_work(),
        ], checkpoints={'segment-five-silence': lambda: self.wait_command_silence('segment-five'),
                        'window-used': lambda: self.wait_parts(
            states, kind=INPUT_KIND, parent='failed', texts=texts)})
        self.assert_outputs(texts[:4], anchor='followup')
        self.assertTrue(all(event['type'] == 'passive_push' for event in self.live_outputs()))
        self.assertEqual(self.executions.read_text(), 'x')


class NaturalSpeechSourcePartsAcceptance(NaturalSpeechPartsAcceptance):
    """真正的后台来源只在全组结束时结算一次，不拿前台进度代替来源测试。"""
    for name in dir(NaturalSpeechPartsAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    reminders = reminder.LiveReminderAcceptance.reminders
    create_due = reminder.LiveReminderAcceptance.create_due

    def source_parts(self, *, second_unknown=False):
        reminder_id = self.create_due()
        texts = ['该喝点水了。', '刚才记下的提醒已经到时间。', '这几条属于同一次提醒。']
        kind = 'reminder_due'
        final_states = ['sent', 'unknown', 'not_attempted'] if second_unknown else ['sent'] * 3

        def decide(facts):
            self.assertEqual(facts['trigger'], 'background_event')
            self.assertEqual(facts['host_event']['kind'], kind)
            self.assertEqual(facts['host_event']['reminder']['text'], '喝水')
            return {'action': 'speak', 'after_ms': 0, 'parts': [
                {'text': text, 'after_ms': 0 if index == 0 else 1800}
                for index, text in enumerate(texts)]}

        def first_without_source_completion():
            self.wait_parts(['sent', 'planned', 'planned'], kind=kind,
                            parent='sending', texts=texts)
            source = self.reminders()[0]
            self.assertEqual(source['id'], reminder_id)
            self.assertEqual(source['status'], 'Delivering')
            self.assertEqual(len(source['attempts']), 1)
            self.assertIsNone(source['attempts'][0]['result'])
            self.assertIsNone(source['attempts'][0]['finished_at_ms'])
            self.assertEqual([event['text'] for event in self.live_outputs()], texts[:1])

        def source_completed():
            self.wait_parts(final_states, kind=kind,
                            parent='unknown' if second_unknown else 'sent', texts=texts)
            status = 'Unknown' if second_unknown else 'Delivered'
            self.wait_until(lambda: self.reminders()[0]['status'] == status,
                            '后台来源没有按完整分条结果结算')
            [attempt] = self.reminders()[0]['attempts']
            if second_unknown:
                self.assertIsNone(attempt['finished_at_ms'])
                self.assertEqual(attempt['result'], 'unknown')
            else:
                self.assertIsNotNone(attempt['finished_at_ms'])
                parts = self.speech_record(kind)['receipt']['delivery']['parts']
                self.assertEqual(attempt['result'], {'sent': {
                    'platform_message_id': parts[-1]['delivery']['platform_message_id']}})

        self.speech_callback = decide
        self.run_live([
            *self.checkpoint('source-prefix'),
            *([{'push_mode': {'ok': False, 'failure_kind': 'timeout'}}] if second_unknown else []),
            *self.checkpoint('source-settled'),
        ], more=['--reminders', '--segmented'], checkpoints={
            'source-prefix': first_without_source_completion, 'source-settled': source_completed})
        outputs = self.live_outputs()
        self.assertEqual([event['text'] for event in outputs], texts[:2] if second_unknown else texts)
        self.assertEqual(len({event['id'] for event in outputs}), len(outputs))
        self.assertTrue(all(event['type'] == 'push' for event in outputs))
        self.assertEqual(self.reminders()[0]['id'], reminder_id)
        receipts = self.speech_record(kind)['receipt']['delivery']['parts']
        for frame, receipt in zip(outputs, receipts):
            if receipt['delivery']['state'] == 'sent':
                self.assertEqual(receipt['delivery']['platform_message_id'], 'push-' + frame['id'])
        source_before, speech_before = self.reminders(), self.speech_record(kind)
        calls_before = len(self.speech_requests)
        self.speech_callback = lambda _: self.fail('已结算来源不得在重启后重新决定或重放')
        self.run_live([{'delay_ms': 2300}], more=['--reminders', '--segmented'])
        self.assertEqual(self.reminders(), source_before)
        self.assertEqual(self.speech_record(kind), speech_before)
        self.assertEqual(len(self.speech_requests), calls_before)
        self.assert_outputs([])

    def test_background_source_completes_once_only_after_every_part_is_sent(self):
        self.source_parts()

    def test_background_source_partial_unknown_stops_and_is_not_reclaimed_after_restart(self):
        self.source_parts(second_unknown=True)


if __name__ == '__main__':
    unittest.main(verbosity=2)
