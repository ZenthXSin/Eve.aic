import asyncio
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'adapters/antigravity'))
import bridge


class AdapterTest(unittest.TestCase):
    def setUp(self):
        self.body = {'model': bridge.MODEL, 'messages': [{'role': 'user', 'content': 'hi'}], 'tools': [
            {'type': 'function', 'function': {'name': 'lookup', 'parameters': {'type': 'object',
             'properties': {'key': {'type': 'string'}}, 'required': ['key']}}}]}
        self.data, self.validators, _ = bridge.prepare(self.body)

    def test_input(self):
        event = json.loads(self.data)
        self.assertEqual(event['event'], 'user')
        self.assertEqual(json.loads(event['message']['content'].split('\n', 1)[1])['messages'], self.body['messages'])
        self.body['messages'][0]['content'] = [{'type': 'image_url'}]
        with self.assertRaises(bridge.Failure):
            bridge.prepare(self.body)

    def test_tool_calls(self):
        output = {'structured_output': {'text': '', 'calls': [{'name': 'lookup', 'arguments': '{"key":"a"}'}]}}
        choice = bridge.completion(output, self.validators)['choices'][0]
        self.assertEqual(choice['finish_reason'], 'tool_calls')
        self.assertTrue(choice['message']['tool_calls'][0]['id'].startswith('call_'))
        for name, arguments in [('missing', '{}'), ('lookup', '{}'), ('lookup', 'bad')]:
            output['structured_output']['calls'] = [{'name': name, 'arguments': arguments}]
            with self.assertRaises(bridge.Failure):
                bridge.completion(output, self.validators)

    def cli(self, code, timeout=4, model=bridge.MODEL):
        with tempfile.TemporaryDirectory() as temp:
            cli = Path(temp)/'agy'
            cli.write_text('#!/usr/bin/python3\nimport sys,json,time\n' + code)
            cli.chmod(0o700)
            return asyncio.run(bridge.run_cli(self.data, None, timeout=timeout, cli=str(cli), model=model))

    def test_selected_cli_model(self):
        self.body['model'] = 'gemini-4-neon-high'
        bridge.prepare(self.body)
        result = self.cli('assert sys.argv[sys.argv.index("--model")+1] == "gemini-4-neon-high"\n'
                          'print(json.dumps({"event":"result","result":{"status":"SUCCESS","structured_output":{"text":"OK","calls":[]}}}))',
                          model=self.body['model'])
        self.assertEqual(bridge.completion(result, {}, self.body['model'])['model'], self.body['model'])
        self.body['model'] = '--no-sandbox'
        with self.assertRaises(bridge.Failure):
            bridge.prepare(self.body)

    def test_responses_tool_roundtrip(self):
        raw = {'model': 'gemini-4-neon-high', 'instructions': 'sys', 'input': [
            {'role': 'user', 'content': [{'type': 'input_text', 'text': 'hi'}]}],
            'tools': [{'type': 'function', **self.body['tools'][0]['function']}],
            'max_output_tokens': 8192, 'reasoning': {'effort': 'high'}, 'stream': True}
        chat = bridge.responses_chat(raw)
        self.assertEqual(chat['messages'][0], {'role': 'system', 'content': 'sys'})
        self.assertEqual(chat['max_completion_tokens'], 8192)
        _, validators, effort = bridge.prepare(chat)
        self.assertEqual(effort, 'high')
        completion = bridge.completion({'structured_output': {'text': 'ok', 'calls': [
            {'name': 'lookup', 'arguments': '{"key":"a"}'}]}, 'usage': {'input_tokens': 9,
            'output_tokens': 4, 'total_tokens': 13, 'cache_read_tokens': 3}}, validators, chat['model'])
        value = bridge.responses_result(completion)
        self.assertEqual(value['status'], 'completed')
        self.assertEqual(value['model'], chat['model'])
        self.assertEqual(value['usage']['input_tokens_details'], {'cached_tokens': 3})
        self.assertEqual(value['output'][0]['content'][0]['text'], 'ok')
        call_id = value['output'][1]['call_id']
        raw['input'].extend(value['output'] + [{'type': 'function_call_output', 'call_id': call_id, 'output': 'a'}])
        replay = bridge.responses_chat(raw)
        bridge.prepare(replay)
        self.assertEqual(replay['messages'][-2]['tool_calls'][0]['id'], call_id)
        self.assertEqual(replay['messages'][-1], {'role': 'tool', 'tool_call_id': call_id, 'content': 'a'})

    def test_responses_unsupported_input(self):
        for fields in ({'previous_response_id': 'resp_x'}, {'tools': [{'type': 'web_search'}]},
                       {'input': [{'role': 'user', 'content': [{'type': 'input_image'}]}]},
                       {'input': [{'type': 'function_call', 'name': 'lookup'}]}):
            with self.subTest(fields=fields), self.assertRaises(bridge.Failure):
                bridge.responses_chat({'input': 'hi', **fields})

    def test_result_only(self):
        result = self.cli('assert json.load(sys.stdin)["event"] == "user"\n'
                          'print(json.dumps({"event":"step_update","step_update":{"text_delta":"not a reply"}}))\n'
                          'print(json.dumps({"event":"result","result":{"status":"SUCCESS","structured_output":{"text":"OK","calls":[]}}}))')
        self.assertEqual(bridge.completion(result, {})['choices'][0]['message']['content'], 'OK')

    def test_cumulative_counters(self):
        bases = {'input_tokens':30384, 'output_tokens':4, 'total_tokens':30388, 'cache_read_tokens':0}
        end = {'input_tokens':30662, 'output_tokens':8, 'total_tokens':30670, 'cache_read_tokens':30214}
        self.assertEqual(bridge.turn_usage(end, bases), {'prompt_tokens':30492,'completion_tokens':4,
                         'total_tokens':30496,'prompt_tokens_details':{'cached_tokens':30214}})
        self.assertIsNone(bridge.turn_usage(bases, end))
        self.assertEqual(bridge.turn_usage({key:val for key,val in bases.items() if key!='cache_read_tokens'}),
                         {'completion_tokens':4})

    def test_cache_scope(self):
        body = dict(self.body, prompt_cache_key='eve')
        self.assertEqual(bridge.responses_chat(dict(input='hi', model='m', prompt_cache_key='eve'))['prompt_cache_key'], 'eve')
        self.assertIsNone(bridge.scope(self.body,'chat'))
        for changed in (dict(model='m2'),dict(tools=[]),dict(prompt_cache_key='agent'),dict(reasoning_effort='high'),dict(max_completion_tokens=1)):
            self.assertNotEqual(bridge.scope(body,'chat'),bridge.scope(dict(body,**changed),'chat'))
        self.assertNotEqual(bridge.scope(body,'chat'),bridge.scope(body,'responses'))

    def test_reuse_and_invalidate(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            for name in ('brain','conversations','crashes','log'): (root/name).mkdir()
            cache = bridge.Sessions(root)
            cid = '12345678-1234-1234-1234-123456789abc'
            calls = []
            async def fake(data, effort, conn, model, conversation):
                calls.append((data,conversation)); (root/'conversations'/(cid+'.db')).touch()
                return {'conversation_id':cid, 'num_turns':2 if conversation else 1,
                        'structured_output':{'text':'ok','calls':[]},'usage':{'input_tokens':20, 'output_tokens':2,
                        'total_tokens':22,'cache_read_tokens':30 if conversation else 10}}
            body = dict(self.body, prompt_cache_key='eve'); body['messages'] = list(body['messages'])
            def generate():
                data, validators, effort = bridge.prepare(body)
                return cache.generate(body,'chat',data,validators,effort,None)
            with patch.object(bridge,'run_cli',fake):
                response = generate(); self.assertIsNone(calls[-1][1]); reply=response['choices'][0]['message']
                body['messages'] += [reply, {'role':'user','content':'next'}]
                response = generate(); self.assertEqual(calls[-1][1],cid)
                self.assertEqual(response['usage']['prompt_tokens_details']['cached_tokens'],20)
                body['messages'] += [reply, {'role':'user','content':'next'}]; body['messages'][0]={'role':'user','content':'compressed'}
                generate(); self.assertIsNone(calls[-1][1])
                body['messages'] += [{'role':'assistant','content':'different'}, {'role':'user','content':'next'}]
                generate(); self.assertIsNone(calls[-1][1])
            body['messages'] += [reply, {'role':'user','content':'next'}]
            with patch.object(bridge,'run_cli',AsyncMock(side_effect=bridge.Failure(504,'timeout'))) as failed:
                with self.assertRaises(bridge.Failure): generate()
                self.assertEqual(failed.await_count,1)
                self.assertFalse(cache.rows); self.assertFalse(list((root/'conversations').iterdir()))

    def test_transient_tail_is_current_turn_only(self):
        hints = [{'role':'user','content':value} for value in ('synthetic first hint','synthetic next hint')]
        allowed = {bridge.hashlib.sha256(row['content'].encode()).hexdigest() for row in hints}
        original = [{'role':'system','content':'policy'}, {'role':'user','content':'keep this fact'}]
        reply = {'role':'assistant','content':'ok'}
        event = {'role':'user','content':'new event'}
        def unpack(data):
            return json.loads(json.loads(data)['message']['content'].split('\n',1)[1])
        with patch.object(bridge,'TURN_HINTS',allowed):
            body = dict(self.body, messages=original+[hints[0]])
            self.assertEqual(unpack(bridge.prepare(body)[0])['current_turn_hint'],hints[0])
            self.assertEqual(unpack(bridge.prepare(body)[0])['messages'],original)
            session = bridge.Session('synthetic',len(original),bridge.fingerprint(original),
                                     bridge.reply_signature(reply),{},1,0)
            durable, current = bridge.split_turn(original+[reply,event,hints[1]])
            data, reason = session.delta(durable,current)
            self.assertEqual(reason,'matched'); self.assertEqual(unpack(data)['messages'],[event])
            self.assertEqual(unpack(data)['current_turn_hint'],hints[1]); self.assertNotIn(b'synthetic first hint',data)
            self.assertEqual(session.delta(original+[reply],hints[1])[1],'matched')
            self.assertEqual(session.delta(original[:1]+[event,reply],hints[1])[1],'history_changed')
            self.assertEqual(session.delta(original+[event,reply],hints[1])[1],'reply_changed')
            self.assertEqual(session.delta(original+[reply,original[0]],hints[1])[1],'tail_changed')
            for last in (dict(hints[0],content=hints[0]['content']+'!'),dict(hints[0],role='tool'),dict(hints[0],extra=1)):
                self.assertEqual(bridge.split_turn(original+[last]),(original+[last],None))
            self.assertEqual(bridge.split_turn(original+[hints[1],event]),(original+[hints[1],event],None))

    def test_no_cli_tools(self):
        with self.assertRaises(bridge.Failure) as problem:
            self.cli('print(json.dumps({"event":"step_update","step_update":{"tool_info":{}}}))')
        self.assertEqual(problem.exception.code, 'unexpected_cli_tool')

    def test_login_error(self):
        with self.assertRaises(bridge.Failure) as problem:
            self.cli('print("authentication required",file=sys.stderr);sys.exit(1)')
        self.assertEqual(problem.exception.code, 'antigravity_login_required')

    def test_timeout(self):
        with self.assertRaises(bridge.Failure) as problem:
            self.cli('time.sleep(5)', .05)
        self.assertEqual(problem.exception.status, 504)


if __name__ == '__main__':
    unittest.main()
