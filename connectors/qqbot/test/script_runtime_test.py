"""真实 QQ 宿主中的 Rust 插件修复交互；模型/QQ 为本地替身，编译及运行使用实际沙箱。"""
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

import developer_tools_test as developer
from dialogue_tools_test import text_response, tool_response


@unittest.skipUnless(sys.platform == 'linux', 'Rust 隔离编译目前只支持 Linux')
class ScriptRepairAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith('test_'):
            locals()[_name] = None
    del _name

    def setUp(self):
        self.assertTrue(Path('/usr/bin/bwrap').is_file(), '本验收要求真实 bubblewrap，不能降级为宿主执行或静默跳过')
        toolchain = os.environ.get('EVE_TEST_PLUGIN_TOOLCHAIN')
        command = ['rustup', 'run', toolchain, 'rustc'] if toolchain else ['rustc']
        result = subprocess.run([*command, '--print', 'sysroot'], capture_output=True, text=True, check=True)
        self.compiler = str(Path(result.stdout.strip()) / 'bin/rustc')
        super().setUp()
        config = json.loads(self.developer_config.read_text())
        config['max_tool_rounds'] = 12
        self.developer_config.write_text(json.dumps(config))

    def tools(self, operations):
        response = tool_response(operations)
        for call in response['choices'][0]['message']['tool_calls']:
            call['id'] += f'-phase-{self.chat_rounds}'
        return response

    def execute_script(self, script):
        self.run_eve(script, interest=False, memory=True, research=False, practice=False,
            env_extra={'EVE_DEVELOPER_CONFIG': str(self.developer_config), 'EVE_SCRIPT_RUSTC': self.compiler})

    def test_concrete_parameter_preflight_does_not_write_then_execution_uses_original_authority(self):
        source = '''import json
x=json.loads(input())
def rpc(value):
 print(json.dumps(value),flush=True)
 return json.loads(input())['result']
bad=rpc({'op':'check_tool','name':'write_file','arguments':{'path':'checked.txt','content':'kept'}})
good_args={'path':'checked.txt','content':'kept','expected_sha256':''}
good=rpc({'op':'check_tool','name':'write_file','arguments':good_args})
written=rpc({'op':'tool','name':'write_file','arguments':good_args}) if x['arguments'].get('execute') else None
print(json.dumps({'op':'return','value':{'bad':bad,'good':good,'written':written}}),flush=True)
'''
        spec = {'name': 'checked-write', 'language': 'python', 'source': source,
                'dependencies': {}, 'tools': ['write_file'],
                'state_schema': {'version': 1, 'fields': {}}, 'initial_state': {}}
        version = None
        def respond(body):
            nonlocal version
            results = self.results(body)
            if not results:
                return self.tools([('save_script_plugin', {'spec': spec})])
            if len(results) == 1:
                version = results[0]['version']['reference']
                return self.tools([('activate_script_plugin', {'version': version})])
            if len(results) == 2:
                return self.tools([('run_script_plugin', {'version': version, 'arguments': {'execute': False}})])
            receipt = results[-1]['receipt']
            self.assertEqual(receipt['status'], 'succeeded')
            value = receipt['result']
            self.assertEqual(value['bad'], {'allowed': False, 'executed': False,
                'authorization_reserved': False, 'reason': 'rejected'})
            self.assertEqual(value['good'], {'allowed': True, 'executed': False,
                'authorization_reserved': False, 'reason': 'preflight_passed'})
            if len(results) == 3:
                self.assertIsNone(value['written'])
                self.assertEqual(receipt['evidence'], [])
                self.assertFalse((self.source / 'checked.txt').exists(), '预检不能写入文件')
                return self.tools([('run_script_plugin', {'version': version, 'arguments': {'execute': True}})])
            self.assertEqual(len(results), 4)
            self.assertTrue(value['written']['success'])
            self.assertEqual([e['operation'] for e in receipt['evidence']], ['tool'])
            self.assertEqual([e['target'] for e in receipt['evidence']], ['write_file'])
            self.assertEqual((self.source / 'checked.txt').read_text(), 'kept')
            return text_response('参数预检没有写文件，实际执行后才保存成功')
        self.callback = respond
        self.execute_script(self.send(self.message('check-operation', '先预检写文件，再实际保存',
            expected='参数预检没有写文件，实际执行后才保存成功')))
        self.assertEqual(len(self.kind('chat')), 5)
        self.assertEqual(len(self.kind('review')), 0)

    def test_readonly_diagnostic_plugin_reads_own_finished_turns_and_model_state_across_restart(self):
        source = '''import json
input()
def rpc(value):
 print(json.dumps(value),flush=True)
 return json.loads(input())['result']
forged=rpc({'op':'check_tool','name':'read_context_record','arguments':{'action':'recent','user_id':'other'}})
recent=rpc({'op':'tool','name':'read_context_record','arguments':{'action':'recent','limit':1}})
model=rpc({'op':'tool','name':'inspect_model_runtime','arguments':{'purpose':'dialogue'}})
print(json.dumps({'op':'return','value':{'forged':forged,'recent':recent,'model':model}}),flush=True)
'''
        spec = {'name': 'readonly-diagnostics', 'language': 'python', 'source': source,
                'dependencies': {}, 'tools': ['read_context_record', 'inspect_model_runtime'],
                'state_schema': {'version': 1, 'fields': {}}, 'initial_state': {}}
        version, receipts = None, []
        def respond(body):
            nonlocal version
            current = next(m['content'] for m in reversed(body['messages']) if m['role'] == 'user')
            if current == '保存前轮证据':
                return text_response('前轮证据已保存')
            results = self.results(body)
            if current == '其他人不能运行这个版本':
                if not results:
                    return self.tools([('run_script_plugin', {'version': version, 'arguments': {}})])
                self.assertIn('error', results[-1])
                return text_response('没有当前会话的诊断版本')
            if not results:
                if version is None:
                    return self.tools([('save_script_plugin', {'spec': spec})])
                return self.tools([('run_script_plugin', {'version': version, 'arguments': {}})])
            if 'receipt' not in results[-1]:
                self.assertNotIn('error', results[-1], '诊断工具必须在脚本绑定前装配；失败不能循环调用')
                if len(results) == 1:
                    version = results[0]['version']['reference']
                    return self.tools([('activate_script_plugin', {'version': version})])
                return self.tools([('run_script_plugin', {'version': version, 'arguments': {}})])
            receipt = results[-1]['receipt']
            self.assertEqual(receipt['status'], 'succeeded')
            self.assertEqual([e['target'] for e in receipt['evidence']], ['read_context_record', 'inspect_model_runtime'])
            value = receipt['result']
            self.assertFalse(value['forged']['allowed'])
            self.assertEqual(value['recent']['scope'], 'current_only')
            [record] = value['recent']['records']
            self.assertEqual(record['status'], 'Completed', '运行中的本轮不能被当作已结束证据')
            self.assertEqual(record['turn_id'], len(receipts) + 1)
            self.assertFalse(record['automatic_replay'])
            self.assertEqual(value['model']['purpose'], 'dialogue')
            self.assertFalse(value['model']['configuration_written'])
            self.assertFalse(value['model']['provider_availability_verified'])
            receipts.append(receipt)
            return text_response('诊断读取成功，没有改模型或重放工具')
        self.callback = respond
        self.execute_script([
            *self.send(self.message('prior-evidence', '保存前轮证据', expected='前轮证据已保存')),
            *self.send(self.message('diagnose', '创建诊断插件读取证据', expected='诊断读取成功，没有改模型或重放工具')),
        ])
        self.execute_script(self.send(self.message('diagnose-again', '重启后再运行诊断', expected='诊断读取成功，没有改模型或重放工具')))
        ledger = self.work / 'state/script-plugins/state/ledger.json'
        before = ledger.read_bytes()
        self.execute_script(self.send(self.message('foreign-diagnostics', '其他人不能运行这个版本', user='user-2',
            expected='没有当前会话的诊断版本')))
        self.assertEqual(ledger.read_bytes(), before)
        self.assertEqual(len(receipts), 2)
        self.assertEqual(len(self.kind('review')), 0)
        self.assertEqual(self.chat_rounds, 9, '诊断不得增加模型探测请求或编造修复任务')
        self.assertFalse((self.work / 'state/configuration/config.json').exists())

    def test_compile_failure_is_repaired_verified_and_enabled_in_one_turn_then_survives_restart(self):
        spec = {'name': 'repair-example', 'language': 'rust', 'source': 'fn main(){ missing_identifier(); }',
                'dependencies': {}, 'tools': [], 'state_schema': {'version': 1, 'fields': {}}, 'initial_state': {}}
        fixed_source = 'use std::io; fn main(){let mut s=String::new(); io::stdin().read_line(&mut s).unwrap(); println!("{}", r#"{\"op\":\"return\",\"value\":{\"verified\":42}}"#);}'
        version = None
        receipt = None
        def respond(body):
            nonlocal version, receipt
            results = self.results(body)
            if not results:
                return self.tools([('save_script_plugin', {'spec': spec})])
            if len(results) == 1:
                failed = results[0]
                self.assertEqual([failed[k] for k in ('success', 'saved', 'compiled', 'executed')], [False] * 4)
                self.assertEqual(failed['phase'], 'compilation')
                self.assertIsNone(failed['version'])
                self.assertIn('missing_identifier', failed['compilation']['diagnostics'])
                self.assertNotEqual(failed['compilation']['exit_code'], 0)
                self.assertLessEqual(len(failed['compilation']['diagnostics'].encode()), 8192)
                return self.tools([('save_script_plugin', {'spec': {**spec, 'source': fixed_source}})])
            if len(results) == 2:
                self.assertTrue(results[1]['saved'])
                self.assertTrue(results[1]['compiled'])
                self.assertFalse(results[1]['executed'])
                version = results[1]['version']['reference']
                return self.tools([('activate_script_plugin', {'version': version})])
            if len(results) == 3:
                self.assertEqual(results[2]['activated'], version)
                return self.tools([('run_script_plugin', {'version': version, 'arguments': {}, 'dry_run': True})])
            if len(results) == 4:
                self.assertFalse(results[3]['executed'])
                self.assertTrue(results[3]['preflight']['dry_run'])
                return self.tools([('run_script_plugin', {'version': version, 'arguments': {}})])
            if len(results) == 5:
                self.assertTrue(results[4]['success'])
                receipt = results[4]['receipt']
                self.assertEqual(receipt['status'], 'succeeded')
                self.assertEqual(receipt['result'], {'verified': 42})
                self.assertEqual(receipt['version'], version)
                return self.tools([('list_script_plugins', {'call_id': receipt['call_id']})])
            self.assertEqual(len(results), 6)
            self.assertEqual(results[5]['receipt'], receipt)
            return text_response('已按真实诊断修复，隔离运行返回42，已启用并核对回执')
        self.callback = respond
        self.execute_script(self.send(self.message('repair', '编写Rust工具，遇到编译错误自行修复并实际验证',
            expected='已按真实诊断修复，隔离运行返回42，已启用并核对回执')))
        self.assertEqual(len(self.kind('chat')), 7)
        self.assertEqual(len(self.kind('review')), 0, '纯无外部能力插件不需要额外模型审查请求')
        self.assertEqual(self.turn('编写Rust工具，遇到编译错误自行修复并实际验证')['status']['state'], 'Completed')
        ledger = self.work / 'state/script-plugins/state/ledger.json'
        before = ledger.read_bytes()
        def restored(body):
            current = next(m['content'] for m in reversed(body['messages']) if m['role'] == 'user')
            results = self.results(body)
            if current == '查看其他会话的执行记录':
                if not results:
                    return self.tools([('list_script_plugins', {'call_id': receipt['call_id']})])
                self.assertIn('error', results[-1])
                return text_response('其他会话无权读取')
            if not results:
                return self.tools([('list_script_plugins', {})])
            [entry] = results[0]['versions']
            self.assertEqual(entry['selected'], version)
            self.assertEqual(entry['version']['reference'], version)
            if len(results) == 1:
                return self.tools([('list_script_plugins', {'call_id': receipt['call_id']})])
            self.assertEqual(results[1]['receipt'], receipt)
            return text_response('重启后版本和回执一致，没有重编译或重跑')
        self.callback = restored
        self.execute_script([*self.send(self.message('restored', '读取上次工具状态', expected='重启后版本和回执一致，没有重编译或重跑')),
            *self.send(self.message('foreign', '查看其他会话的执行记录', user='user-2', expected='其他会话无权读取'))])
        self.assertEqual(ledger.read_bytes(), before, '只读恢复和拒绝越界查询不得改写插件账本')
        self.assertEqual(self.errors, [])


if __name__ == '__main__':
    unittest.main()
