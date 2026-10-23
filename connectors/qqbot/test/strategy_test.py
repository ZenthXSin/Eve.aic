"""真实 QQ 宿主：修正实例、跨目标验证、回退与重启；模型/QQ 为本地替身。"""
import json
import os
import subprocess
import unittest
import practice_test
import toolforge_test
from interest_test import MINDUSTRY
from memory_test import BINARY, ROOT

SECOND = toolforge_test.SECOND
THIRD = toolforge_test.THIRD
FOURTH = '我还想为 Mindustry 做一种地板，但是不会做'
FIFTH = '我喜欢 Mindustry 的建筑，想做一个仓库但是还不会'


class StrategyAcceptance(toolforge_test.ToolForgeAcceptance):
    for _name in dir(toolforge_test.ToolForgeAcceptance):
        if _name.startswith('test_'): locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.broken = False
        self.observe = self.observe_strategy
        self.draft = self.strategy_draft

    def run_eve(self, script, learning=True, real=False, **options):
        return practice_test.PracticeAcceptance.run_eve(
            self, script, research=False, real=real,
            more=['--strategy-learning', '--autonomy-budget', '100'] if learning else [], **options)

    def observe_strategy(self, batch):
        result = self.observe_more(batch)
        for evidence in batch['evidence']:
            if evidence['source']['user_text'] in (FOURTH, FIFTH):
                result['updates'].append({'target': {'new': {'topic': 'Mindustry 地板' if evidence['source']['user_text'] == FOURTH else 'Mindustry 仓库'}},
                    'statements': [{'kind': 'interest', 'quote': evidence['source']['user_text'], 'evidence_id': evidence['id']}]})
        return result

    def strategy_draft(self, request):
        if self.broken:
            return practice_test.mod('Walll')
        if request.get('guidance'):
            return practice_test.mod('Wall')
        return self.fixing_draft(request)

    def events(self):
        return self.documents().get('eve.practice-strategy', {}).get('strategies.v1', {}).get('snapshot', {}).get('events', [])

    def case(self, text, count, real=False):
        def settled():
            self.wait_for(lambda: self.practiced(count), '实践没有结束', 35 if real else 20)
            self.wait_for(lambda: any('Proposed' in e for e in self.events()), '没有保存策略来源', 5)
            selected = [e['Selected'] for e in self.events() if 'Selected' in e]
            self.wait_for(lambda: len([e for e in self.events() if 'Observed' in e]) == len(selected), '没有结算策略效果', 5)
        self.inspect_run(settled, prefix=self.send(self.message('case-' + str(count), text)),
                         max_executions=4, timeout=75 if real else 30, real=real)

    def test_repairs_become_cross_goal_guidance_then_regression_returns_to_baseline(self):
        self.case(MINDUSTRY, 1)
        self.assertEqual(len(self.kind('draft')), 2)
        self.case(SECOND, 2)
        self.case(THIRD, 3)
        self.assertEqual([len(r['attempts']) for r in self.practice_runs()], [2, 1, 1])
        for request in self.kind('draft')[2:]:
            decoded = json.loads(request['body']['messages'][-1]['content'])
            self.assertEqual(decoded['guidance']['version'], 1)
            self.assertIn('Walll', str(decoded['guidance']['previous_issues']))
            events = request['state']['eve.practice-strategy']['strategies.v1']['snapshot']['events']
            self.assertTrue(any(e.get('Selected', {}).get('run_id') == decoded['run_id'] for e in events), '选择必须先于模型请求保存')
        effects = [e['Observed']['effect'] for e in self.events() if 'Observed' in e]
        self.assertEqual(effects, [{'Verified': {'attempts': 1}}, {'Verified': {'attempts': 1}}])
        self.broken = True
        self.case(FOURTH, 4)
        self.assertEqual(self.practice_runs()[-1]['status'], 'Unverified')
        self.assertEqual([e['Observed']['effect'] for e in self.events() if 'Observed' in e][-1], 'Failed')
        self.broken = False
        self.case(FIFTH, 5)
        self.assertEqual(len(self.practice_runs()[-1]['attempts']), 2)
        self.assertTrue(all('guidance' not in json.loads(r['body']['messages'][-1]['content']) for r in self.kind('draft')[-2:]))
        before = self.events(); calls = len(self.requests); launches = len(self.launches())
        self.inspect_run(self.quiet)
        self.assertEqual(self.events(), before)
        self.assertEqual((len(self.requests), len(self.launches())), (calls, launches))

    def test_disabled_by_default_and_requires_a_runtime(self):
        self.inspect_run(lambda: self.wait_for(self.practiced, '实践没有结束', 20),
                         prefix=self.send(self.message('casual', MINDUSTRY)), learning=False, max_executions=4)
        self.assertEqual(self.events(), [])
        self.assertTrue(all('guidance' not in json.loads(r['body']['messages'][-1]['content']) for r in self.kind('draft')))
        child = subprocess.run([str(BINARY), '--strategy-learning', '--interest-learning', '--agent', str(ROOT / 'AGENT.md')],
                               env={'PATH': os.environ['PATH']}, capture_output=True, text=True, timeout=5)
        self.assertNotEqual(child.returncode, 0)
        self.assertIn('实践运行环境', child.stderr)

    @unittest.skipUnless(practice_test.REAL_JAR, '需要 EVE_MINDUSTRY_SERVER_JAR 和 Java')
    def test_real_runtime_verifies_source_and_independent_reuse(self):
        self.case(MINDUSTRY, 1, real=True)
        self.case(SECOND, 2, real=True)
        self.assertEqual([len(r['attempts']) for r in self.practice_runs()], [2, 1])
        self.assertEqual([e['Observed']['effect'] for e in self.events() if 'Observed' in e], [{'Verified': {'attempts': 1}}])


if __name__ == '__main__':
    unittest.main()
