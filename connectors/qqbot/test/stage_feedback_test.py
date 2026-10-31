"""实际宿主：阶段消息、长命令心跳、主动配额、取消与独立进程恢复；仅本地替身。"""
import json
import sys
import time
import unittest
import progress_narration_test as progress
import dialogue_tools_test as dialogue
import memory_test

class StageFeedbackAcceptance(progress.ProgressNarrationAcceptance):
    for name in dir(progress.ProgressNarrationAcceptance):
        if name.startswith('test_'): locals()[name] = None
    del name
    receipt = memory_test.MemoryAcceptance.receipt

    def run_stage(self, script, **options):
        cfg = json.loads(self.developer_config.read_text()); cfg["max_tool_rounds"] = 64
        self.developer_config.write_text(json.dumps(cfg))
        env = {'EVE_DEVELOPER_CONFIG': str(self.developer_config),
               'EVE_PROGRESS_STAGE_UPDATES': 'true', 'EVE_PROGRESS_MIN_INTERVAL_MS': '1000',
               'EVE_PROGRESS_SLOW_AFTER_MS': '1000', 'EVE_CONTEXT_COMPACT_AT_TOKENS': '0',
               'EVE_QQ_LIVE_INTERACTION': 'true'}
        env.update(options.pop('env_extra', {}))
        options.setdefault("more", ["--max-tool-rounds", "64"])
        self.run_eve(script, interest=False, memory=False, research=False, practice=False,
                     env_extra=env, timeout=60, **options)
        self.events = [json.loads(line) for line in (self.work/f'events-{self.runs}.jsonl').read_text().splitlines()]

    def progress(self, id):
        return [e for e in self.events if e.get('type') == 'progress' and e.get('id') == id]

    def test_six_meaningful_stages_keep_final_reply_and_restart_does_not_replay(self):
        def respond(body):
            done = len(self.results(body))
            if done == 6:
                self.assertTrue(all(r['sent'] for r in self.results(body)))
                return dialogue.text_response('核对完成')
            if done: time.sleep(1.15)
            response = dialogue.tool_response([('send_message', {'text': f'已核对第 {done+1} 项，接下来查看下一项'})])
            response['choices'][0]['message']['tool_calls'][0]['id'] += str(done)
            return response
        self.callback = respond
        self.run_stage(self.send(self.message('stages', '分阶段检查', expected='核对完成')),
                       env_extra={'EVE_PROGRESS_SLOW_AFTER_MS': '0'})
        stages = self.progress('stages')
        self.assertEqual(len(stages), 6)
        self.assertEqual([p.get('proactive',False) for p in stages], [False,False,True,True,True,True])
        before = self.receipt('stages')
        self.assertEqual(before['state'], 'Sent')
        self.assertTrue(all(p['state'] == 'Sent' for p in before['progress']))
        self.assertEqual(self.documents()['eve.channel.qqbot']['receipts.v1']['version'], 8)
        self.callback = lambda body: dialogue.text_response('新的聊天正常')
        self.run_stage(self.send(self.message('after', '重新启动', expected='新的聊天正常')))
        self.assertEqual(self.receipt('stages'), before)
        self.assertEqual(self.progress('stages'), [])

    def test_group_limits_ordinary_progress_but_authorization_remains_independent(self):
        def respond(body):
            results = self.results(body)
            done = len(results)
            if done < 3:
                if done: time.sleep(1.15)
                response = dialogue.tool_response([('send_message', {'text': ['开始核对现有文件。', '已有一项实际回执。', '这条多余进度应被限制。'][done]})])
            elif done == 3:
                self.assertEqual([r['sent'] for r in results], [True, True, False])
                response = dialogue.tool_response([('send_message', {'text': '下一步会改写共享分支，需要明确授权。', 'purpose': 'authorization'})])
            else:
                self.assertTrue(results[-1]['sent'])
                self.assertTrue(results[-1]['message_id'])
                self.assertTrue(results[-1]['session_id'])
                return dialogue.text_response('授权问题已送达，独立核对完成；等待回复后才执行依赖动作。')
            response['choices'][0]['message']['tool_calls'][0]['id'] += str(done)
            return response
        self.callback = respond
        self.run_stage(self.send(self.message('group-stages', 'eve 分阶段核对并单独询问授权', scope='group', expected='授权问题已送达，独立核对完成；等待回复后才执行依赖动作。')),
                       env_extra={'EVE_PROGRESS_SLOW_AFTER_MS': '0'})
        sent = self.progress('group-stages')
        self.assertEqual(len(sent), 3, '两个普通进度加一个独立授权问题')
        self.assertTrue(all(not p.get('proactive', False) for p in sent))
        record = self.receipt('group-stages')
        self.assertEqual(record['state'], 'Sent')
        self.assertEqual([p.get('necessary', False) for p in record['progress']], [False, False, True])
        before = record
        self.callback = lambda body: dialogue.text_response('重启后继续等待授权，不重发原问题。')
        self.run_stage(self.send(self.message('group-after', 'eve 重启后核对', scope='group', expected='重启后继续等待授权，不重发原问题。')))
        self.assertEqual(self.receipt('group-stages'), before)
        self.assertEqual(self.progress('group-stages'), [])

    def test_one_long_command_sends_start_and_repeated_truthful_waiting(self):
        original_narrator = self.provider_response
        def narrate(body, kind, fallback):
            if kind == 'progress':
                facts = json.loads(body['messages'][-1]['content'])
                if facts['phase'] == 'waiting':
                    self.assertFalse(body.get('tools'))
                    self.assertFalse(facts['group'])
                    self.narrations.append(facts)
                    return dialogue.text_response(f"这一步已等待{facts['waiting_seconds']}秒，仍在等命令回执。")
            return original_narrator(body, kind, fallback)
        self.provider_response = narrate
        def respond(body):
            if not self.results(body):
                return dialogue.tool_response([('run_command', {'program': sys.executable,
                    'args': ['-c', "from pathlib import Path; import time\nwhile not Path('release-waiting').exists(): time.sleep(.01)\nprint('verified')"]})])
            self.assertIn('verified', json.dumps(self.results(body)))
            return dialogue.text_response('命令返回并已核对')
        def release_after_two_delivered_waits():
            def delivered():
                ledger = self.documents().get('eve.channel.qqbot', {}).get('receipts.v1', {})
                receipt = next((r for r in ledger.get('entries', []) if r['message']['id'] == 'long'), {})
                return sum(p['state'] == 'Sent' and p['text'].startswith('这一步已等待')
                           for p in receipt.get('progress', [])) >= 2
            self.wait_until(delivered, 'two actual waiting messages were not delivered')
            (self.source / 'release-waiting').touch()
        self.callback = respond
        self.run_stage([{'send': self.message('long', '执行长命令', expected='命令返回并已核对')},
                        *self.checkpoint('waiting-stages'),
                        {'wait_command': {'id': 'long', 'type': 'reply'}},
                        self.wait_receipt('long')],
                       checkpoints={'waiting-stages': release_after_two_delivered_waits})
        stages = self.progress('long')
        self.assertEqual(stages[0]['text'], '先核对这两步的实际结果。')
        self.assertTrue(any(f['phase'] == 'start' for f in self.narrations))
        self.assertGreaterEqual(len(stages), 3)
        waiting = [p for p in stages if p['text'].startswith('这一步已等待')]
        self.assertGreaterEqual(len(waiting), 2)
        self.assertEqual(len({p['text'] for p in waiting}), len(waiting), '重复正文仍被宿主拒绝')
        self.assertTrue(any(p.get('proactive') for p in waiting))
        self.assertEqual(self.receipt('long')['state'], 'Sent')

    def test_failed_stage_stops_further_progress_but_keeps_work_and_final_reply(self):
        def respond(body):
            if not self.results(body):
                return dialogue.tool_response([('send_message', {'text':'开始检查'})])
            if len(self.results(body)) == 1:
                time.sleep(1.15)
                return dialogue.tool_response([('echo', {'text':'真实执行结果'})])
            return dialogue.text_response('已有真实执行结果')
        self.callback = respond
        self.run_stage(self.send(self.message('failed', '检查一下', expected='已有真实执行结果') | {'fail_progress':True}))
        self.assertEqual(len(self.progress('failed')),1)
        self.assertEqual(self.receipt('failed')['state'],'Sent')

    def test_cancel_keeps_old_stage_but_suppresses_later_waiting_and_final(self):
        self.response_gates[('chat',2)]='never-release'
        self.callback=lambda body: dialogue.tool_response([('send_message', {'text':'开始检查'})])
        self.run_stage([{'send':self.message('cancelled','检查') | {'expected_type':'finish'}},
            {'wait_command':{'id':'cancelled','type':'progress'}},
            {'wait_file':str(self.arrived('chat',2))},
            *self.send(self.message('cancel','/cancel',contains='已取消')),
            {'wait_command':{'id':'cancelled','type':'finish'}}])
        self.assertEqual(len(self.progress('cancelled')),1)
        self.assertFalse([e for e in self.events if e.get('id')=='cancelled' and e.get('type') in ('reply','segment')])

if __name__ == '__main__': unittest.main()
