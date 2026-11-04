"""实际 QQ 宿主：到期提醒仅是事实，由同一 Eve 决定说、等或沉默。模型/平台用本地替身。"""
from contextlib import closing
import json
import sqlite3
import unittest

import dialogue_tools_test as dialogue
import live_dialogue_test as live


class LiveReminderAcceptance(live.LiveToolAcceptance):
    for name in dir(live.LiveToolAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def reminders(self):
        return self.documents().get('eve.reminder', {}).get('reminders.v1', {}).get('reminders', [])

    def create_due(self):
        def respond(body):
            results = self.all_results(body)
            if not results:
                return self.choose('set_reminder', {'text': '喝水', 'in_minutes': 1})
            if len(results) == 1:
                self.assertTrue(results[0]['saved'])
                return self.choose('send_message', {'text': '一会儿提醒你'})
            return dialogue.text_response('内部结束')
        self.callback = respond
        self.run_live(self.send(self.message('create', '一分钟后提醒我喝水')), more=['--reminders'])
        [reminder] = self.reminders()
        self.assertEqual(reminder['attempts'], [])
        # 进程停止时只把时间整体前移，模拟真实到期，不修改动作和回执。
        state = json.loads(self.state_path.read_text())
        ledger = self.documents()['eve.reminder']['reminders.v1']
        for item in ledger['reminders']:
            for field in ('created_at_ms', 'due_at_ms', 'closed_at_ms'):
                if item[field] is not None:
                    item[field] -= 61_000
        state['entries']['eve.reminder']['reminders.v1'] = list(json.dumps(ledger, ensure_ascii=False).encode())
        self.state_path.write_text(json.dumps(state))
        self.callback = lambda _: dialogue.text_response('没有另选文字')
        return reminder['id']

    def await_state(self, wanted):
        self.wait_until(lambda: any(event['facts'].get('kind') == 'reminder_due' and state == wanted
                                   for event, state, _ in self.speech_rows()), '发言决定未达到 ' + wanted)

    def test_eve_can_decline_due_reminder_without_claim_or_restart_retry(self):
        self.create_due()
        self.speech_callback = lambda facts: {'action': 'silence'}
        self.run_live(self.checkpoint('silenced'), more=['--reminders'],
                      checkpoints={'silenced': lambda: self.await_state('silence')})
        self.assertEqual(self.reminders()[0]['attempts'], [])
        self.assertFalse([e for e in self.events if e.get('type') == 'push'])
        count = len(self.speech_requests)
        self.run_live([{'delay_ms': 2300}], more=['--reminders'])
        self.assertEqual(len(self.speech_requests), count)
        self.assertEqual(self.reminders()[0]['attempts'], [])
        self.assertFalse([e for e in self.events if e.get('type') == 'push'])

    def test_wait_survives_restart_then_eve_authors_exact_words_and_gets_actual_receipt(self):
        reminder_id = self.create_due()
        self.speech_callback = lambda _: {'action': 'wait', 'after_ms': 60_000}
        self.run_live(self.checkpoint('waiting'), more=['--reminders'],
                      checkpoints={'waiting': lambda: self.await_state('waiting')})
        self.assertEqual(self.reminders()[0]['attempts'], [])
        count = len(self.speech_requests)
        self.run_live([{'delay_ms': 2300}], more=['--reminders'])
        self.assertEqual(len(self.speech_requests), count, '未到 Eve 选的时间不能提前复审')
        with closing(sqlite3.connect(self.work / 'state/speech.sqlite3')) as db:
            db.execute("UPDATE speech SET due=0 WHERE state='waiting'")
            db.commit()
        def decide(facts):
            self.assertEqual(facts['host_event']['kind'], 'reminder_due')
            self.assertEqual(facts['host_event']['reminder']['text'], '喝水')
            return {'action': 'speak', 'text': '刚才说的水，现在喝两口吧', 'after_ms': 0}
        self.speech_callback = decide
        self.run_live(self.checkpoint('sent'), more=['--reminders'],
                      checkpoints={'sent': lambda: self.await_state('sent')})
        [push] = [e for e in self.events if e.get('type') == 'push']
        self.assertEqual(push['text'], '刚才说的水，现在喝两口吧')
        self.assertEqual(push['id'], reminder_id)
        [attempt] = self.reminders()[0]['attempts']
        self.assertEqual(attempt['result'], {'sent': {'platform_message_id': 'push-' + reminder_id}})
        self.assertEqual(self.reminders()[0]['status'], 'Delivered')

    def test_cancelled_source_invalidates_delayed_words_before_delivery(self):
        reminder_id = self.create_due()
        self.speech_callback = lambda _: {'action': 'speak', 'text': '旧提醒这句不应发出', 'after_ms': 60_000}
        self.run_live(self.checkpoint('ready'), more=['--reminders'],
                      checkpoints={'ready': lambda: self.await_state('ready')})
        self.assertEqual(self.reminders()[0]['attempts'], [])
        cancel_requested = False
        def cancel(body):
            nonlocal cancel_requested
            if not cancel_requested:
                cancel_requested = True
                return self.choose('cancel_reminder', {'reminder_id': reminder_id})
            self.assertTrue(self.all_results(body)[-1].get('cancelled'), self.all_results(body)[-1])
            return dialogue.text_response('内部已取消，无需补话')
        self.callback = cancel
        self.run_live(self.send(self.message('cancel-due', '不用提醒喝水了')), more=['--reminders'])
        self.assertEqual(self.reminders()[0]['status'], 'Cancelled')
        with closing(sqlite3.connect(self.work / 'state/speech.sqlite3')) as db:
            db.execute("UPDATE speech SET due=0 WHERE state='ready'")
            db.commit()
        self.run_live([{'delay_ms': 2300}], more=['--reminders'])
        self.assertEqual(self.reminders()[0]['attempts'], [])
        self.assertFalse([e for e in self.events if e.get('type') in ('push', 'progress')])

    def test_invalid_speech_response_leaves_source_unclaimed_without_fallback_text(self):
        self.create_due()
        self.speech_callback = lambda _: {'action': 'speak', 'text': '', 'after_ms': 0}
        self.run_live(self.checkpoint('invalid-waits'), more=['--reminders'],
                      checkpoints={'invalid-waits': lambda: self.await_state('waiting')})
        self.assertEqual(self.reminders()[0]['attempts'], [])
        self.assertFalse([e for e in self.events if e.get('type') == 'push'])


if __name__ == '__main__':
    unittest.main()
