"""实际宿主：阶段消息、长命令心跳、主动配额、取消与独立进程恢复；仅本地替身。"""
import json
import sys
import time
import unittest
import developer_tools_test as developer
import dialogue_tools_test as dialogue
import memory_test

class StageFeedbackAcceptance(developer.DeveloperAcceptance):
    for name in dir(developer.DeveloperAcceptance):
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

    def test_one_long_command_sends_start_and_repeated_truthful_waiting(self):
        def respond(body):
            if not self.results(body):
                return dialogue.tool_response([('run_command', {'program':sys.executable,
                    'args':['-c','import time; time.sleep(4.4); print("verified")']})])
            self.assertIn('verified', json.dumps(self.results(body)))
            return dialogue.text_response('命令返回并已核对')
        self.callback = respond
        self.run_stage(self.send(self.message('long', '执行长命令', expected='命令返回并已核对')))
        stages = self.progress('long')
        self.assertTrue(stages[0]['text'].startswith('我先进行命令执行'))
        self.assertGreaterEqual(len(stages), 4)
        self.assertTrue(any('仍在进行中' in p['text'] and p['proactive'] for p in stages))
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
