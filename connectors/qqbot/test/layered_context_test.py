"""真实QQ宿主长程上下文/原文档案回归；模型与平台为本地确定性替身。"""
import hashlib
import json
import unittest

import dialogue_tools_test
from dialogue_tools_test import text_response, tool_response
from request_messages import conversation_messages


class LayeredContextAcceptance(dialogue_tools_test.DialogueToolsAcceptance):
    for _name in dir(dialogue_tools_test.DialogueToolsAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def test_long_project_retains_raw_archive_and_scope_across_restart(self):
        original = 'ARCHIVE_BLUE_ROOT 验收口令是 quartz-73852-ALPHA。原文必须完整保留。'
        samples, observed, references, denied, trace, digests = [], [], [], [], [], []

        def respond(body):
            if body['messages'][0].get('content', '').startswith('你是对话历史压缩器'):
                data = json.loads(body['messages'][1]['content'])
                covered = data['previous_turns'] + len(data['turns'])
                self.assertFalse(body.get('tools'))
                self.assertEqual([t['number'] for t in data['turns']],
                                 list(range(data['previous_turns'] + 1, covered + 1)))
                digests.append(covered)
                return text_response(json.dumps({'points': [{'text': '项目步骤已有记录，原文仍以宿主档案为准。', 'turns': [covered]}]}, ensure_ascii=False))
            messages = conversation_messages(body['messages'])
            latest = max(i for i, m in enumerate(messages) if m['role'] == 'user')
            query = messages[latest]['content']
            trace.append(str(query)[:250])
            results = [json.loads(m['content']) for m in messages[latest + 1:] if m['role'] == 'tool']
            if query.startswith('WORKSTEP-'):
                samples.append((int(query.split(':', 1)[0].split('-')[1]),
                                len(json.dumps(body['messages'], ensure_ascii=False).encode())))
                return text_response('已继续本项目。')
            if query == '请查原始 ARCHIVE_BLUE_ROOT 档案并核验实际原文。':
                if not results:
                    observed.append(('recall_context', json.dumps(body['messages'], ensure_ascii=False)))
                    return tool_response([('read_context_record', {'action': 'search', 'query': 'ARCHIVE_BLUE_ROOT'})])
                if len(results) == 1:
                    records = results[0].get('records', [])
                    if not records:
                        observed.append(('missing_archive', results[0]))
                        return text_response('未找到档案。')
                    references.append(records[-1]['reference'])
                    reply = tool_response([('read_context_record', {'action': 'read', 'reference': references[-1]})])
                    reply['choices'][0]['message']['tool_calls'][0]['id'] += '-read'
                    return reply
                observed.append(('archive', results[-1]))
                return text_response('已核验原始档案。')
            if query == 'OTHER_SCOPE_READ':
                if not results:
                    observed.append(('foreign_context', json.dumps(body['messages'], ensure_ascii=False)))
                    return tool_response([('read_context_record', {'action': 'read', 'reference': references[-1]})])
                denied.append(results[-1])
                return text_response('异会话未读取原文。')
            if query == 'OWNER_RESTORE':
                if not results:
                    return tool_response([('read_context_record', {'action': 'read', 'reference': references[-1]})])
                observed.append(('restored', results[-1]))
                return text_response('重启后原文仍在。')
            return text_response('已登记原始档案。')

        self.callback = respond
        # 此容量曲线固定使用200k token和64 KiB历史窗口，不依赖可调整的产品默认值。
        environment = {'EVE_CONTEXT_ADAPTIVE_LIMIT_TOKENS': '200000', 'EVE_HISTORY_MAX_BYTES': '65536'}
        script = self.send(self.message('archive-root', original, expected='已登记原始档案。'))
        for i in range(1, 73):
            script += self.send(self.message(f'work-{i}', f'WORKSTEP-{i}: 当前项目第{i}步。' + ('独立步骤材料。' * 240), expected='已继续本项目。'))
        script += self.send(self.message('archive-query', '请查原始 ARCHIVE_BLUE_ROOT 档案并核验实际原文。', expected='已核验原始档案。'))
        try:
            self.run_eve(script, interest=False, research=False, practice=False, panel=True, timeout=180, env_extra=environment)
        except Exception:
            print('LAYERED_FAILURE_TRACE ' + json.dumps({'queries': trace[-5:], 'provider_errors': self.errors,
                  'events': [v for v in (json.loads(line) for line in (self.work / f'events-{self.runs}.jsonl').read_text().splitlines()) if v.get('direction') == 'out'][-4:]}, ensure_ascii=False), flush=True)
            raise
        self.assertEqual(len(samples), 72)
        self.assertFalse(any(line.startswith('EVE_CONTEXT_COMPACTION ')
                             for line in self.panel_stderr.read_text().splitlines()),
                         '分层请求不能依靠主动压缩才能装下；后台原前缀维护单独记录')
        self.assertEqual(digests, sorted(set(digests)), '成功后台摘要的原前缀覆盖必须前进')
        self.assertTrue(all(0 < covered <= 73 for covered in digests))
        print('LAYERED_BACKGROUND_DIGESTS ' + json.dumps(digests), flush=True)
        self.assertTrue(references)
        context = next(v for k, v in observed if k == 'recall_context')
        self.assertIn('quartz-73852-ALPHA', context, '相关早期完整轮次应在工具检索前被选回')
        archive = next(v for k, v in observed if k == 'archive')
        self.assertIn(original, archive['record_json_fragment'])
        self.assertTrue(archive['original_unmodified'])
        self.assertIsNone(archive['next_offset'])
        self.assertEqual(hashlib.sha256(archive['record_json_fragment'].encode()).hexdigest(), archive['sha256'])
        late = [size for number, size in samples if number >= 48]
        self.assertLessEqual(max(late), min(late) * 1.1, '工作集达到窗口后不能仍随总历史线性膨胀')
        self.assertLess(max(late), 250000)
        print('LAYERED_REQUEST_BYTES ' + json.dumps({str(n): size for n, size in samples if n in [16, 32, 48, 64, 72]}), flush=True)

        self.run_eve([*self.send(self.message('foreign-read', 'OTHER_SCOPE_READ', user='user-2', expected='异会话未读取原文。')),
                      *self.send(self.message('restore-read', 'OWNER_RESTORE', expected='重启后原文仍在。'))], interest=False, research=False, practice=False, timeout=45, env_extra=environment)
        self.assertEqual(len(denied), 1)
        self.assertIn('error', denied[0])
        self.assertNotIn('quartz-73852-ALPHA', next(v for k, v in observed if k == 'foreign_context'))
        self.assertEqual(next(v for k, v in observed if k == 'restored')['sha256'], archive['sha256'])
        persisted = self.documents()['eve.session']['sessions.v1']['sessions']
        self.assertTrue(any(len(s['turns']) >= 75 for s in persisted.values()), '历史选择不能删掉原始轮次')


if __name__ == '__main__':
    unittest.main()
