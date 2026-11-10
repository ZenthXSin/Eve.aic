"""真实 QQ 入口：委派改代码、冻结测试、收尾比对；模型与 QQ 均为本地替身。

EVE_QQBOT_BINARY 指向统一构建的二进制。EVE_WORKSPACE_NODE_E2E_ARTIFACTS 可保存完整
请求/响应、原持久记录和断言摘要；不裁剪协议 tools，不调用外部模型或 QQ。
"""
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import sqlite3
import struct
import subprocess
import traceback
import unittest

import dialogue_tools_test as dialogue
import live_dialogue_test as live


TOOL = 'run_workspace_node_test'
SELECTOR = {'kind': 'workspace_node_test', 'directory': 'project', 'entry': 'test.cjs'}
INITIAL = '把 project/value.cjs 改为导出 42，使用 project/test.cjs 测试后直接告诉我结果。'
GOOD = 'module.exports = 42;\n'
BAD = 'module.exports = 99;\n'
PASSED = '项目测试通过，收尾核对时源码仍与测试输入一致。'
CHANGED = '测试之后源码发生变化，目前不能认定修改后的项目通过。'


def sha(data):
    return hashlib.sha256(data).hexdigest()


def trusted_program(name):
    found = shutil.which(name)
    if not found:
        raise AssertionError('Linux 验收缺少可信宿主工具：' + name)
    path = Path(found).resolve(strict=True)
    assert path.is_file() and path.stat().st_mode & 0o100, path
    return path


def interpreter_of(binary):
    # 仅解析宿主信任的 Node ELF；绝不对模型创建的输入查询依赖。
    with binary.open('rb') as source:
        header = source.read(64)
        assert header[:6] == b'\x7fELF\x02\x01', 'fixture requires little-endian ELF64 Node'
        offset = struct.unpack_from('<Q', header, 32)[0]
        size, count = struct.unpack_from('<HH', header, 54)
        assert size >= 56 and 0 < count <= 1024
        for index in range(count):
            source.seek(offset + index * size)
            program = source.read(56)
            if struct.unpack_from('<I', program)[0] == 3:
                at, length = struct.unpack_from('<Q', program, 8)[0], struct.unpack_from('<Q', program, 32)[0]
                assert 2 <= length <= 4096
                source.seek(at)
                value = source.read(length)
                assert value.endswith(b'\0')
                result = Path(value[:-1].decode())
                assert result.is_absolute()
                return result
    raise AssertionError('trusted Node lacks PT_INTERP')


def prepare_runtime(root):
    node, bwrap, ldd = (trusted_program(name) for name in ('node', 'bwrap', 'ldd'))
    interpreter = interpreter_of(node)
    loader = interpreter.resolve(strict=True)
    libraries = {interpreter: loader}
    pending, seen = [node], set()
    while pending:
        binary = pending.pop()
        if binary in seen:
            continue
        seen.add(binary)
        result = subprocess.run([str(ldd), str(binary)], capture_output=True, text=True,
                                env={'PATH': '/usr/bin:/bin', 'LC_ALL': 'C'}, check=True)
        assert 'not found' not in result.stdout, result.stdout
        for line in result.stdout.splitlines():
            words = line.split()
            if not words or words[0].startswith('linux-vdso') or line.strip() == 'statically linked':
                continue
            lookup = Path(words[words.index('=>') + 1] if '=>' in words else words[0])
            assert lookup.is_absolute(), line
            resolved = lookup.resolve(strict=True)
            assert resolved.is_file(), resolved
            libraries[lookup] = resolved
            if resolved != loader:
                pending.append(resolved)
    root.mkdir()
    files = {Path('runtime/node'): node}
    files.update({path.relative_to('/'): original for path, original in libraries.items()})
    for relative, original in files.items():
        destination = root / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(original, destination)
        destination.chmod(original.stat().st_mode & 0o777)
    manifest = []
    for path in sorted(root.rglob('*'), key=lambda path: path.relative_to(root).as_posix()):
        assert not path.is_symlink()
        directory = path.is_dir()
        manifest.append({'path': path.relative_to(root).as_posix(), 'directory': directory,
                         'executable_bits': path.stat().st_mode & 0o111,
                         'bytes': 0 if directory else path.stat().st_size,
                         'sha256': None if directory else sha(path.read_bytes())})
    # Published runtime protocol from FrozenNodeRuntime::bundle_sha256, including struct field order.
    digest = sha(json.dumps(['frozen-node-rootfs:v1', 'runtime/node', manifest],
                            ensure_ascii=False, separators=(',', ':')).encode())
    return digest, bwrap, manifest


class WorkspaceNodeAcceptance(live.LiveToolAcceptance):
    for name in dir(live.LiveToolAcceptance):
        if name.startswith('test_'):
            locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        self.main_requests, self.child_requests, self.responses = [], [], []
        self.events = []
        self.summary, self.parent_result, self.runtime_manifest = {}, None, []
        self.launches = self.work / 'trusted-bwrap-launches.jsonl'
        self.project = self.source / 'project'
        self.project.mkdir()
        (self.project / 'value.cjs').write_text('module.exports = 0;\n')
        (self.project / 'test.cjs').write_text(
            "const assert = require('node:assert/strict');\n"
            "assert.equal(require('./value.cjs'), 42);\n"
            "console.log('WORKSPACE_NODE_REAL_TEST_PASSED');\n")

    def provider_response(self, body, kind, fallback):
        # Persist incoming requests before assertions, so a broken host/fixture cannot lose evidence.
        output = os.environ.get('EVE_WORKSPACE_NODE_E2E_ARTIFACTS')
        if output:
            destination = Path(output) / self._testMethodName
            destination.mkdir(parents=True, exist_ok=True)
            with (destination / 'requests.jsonl').open('a') as stream:
                stream.write(json.dumps({'kind': kind, 'body': body}, ensure_ascii=False) + '\n')
        try:
            result = super().provider_response(body, kind, fallback)
        except Exception:
            # Let the owned process end normally; retain the failed assertion for run_eve to report.
            self.errors.append(traceback.format_exc())
            result = dialogue.text_response('本地验收检测到错误，停止本次执行。')
        self.responses.append({'kind': kind, 'body': result})
        return result

    def records(self):
        database = self.work / 'state/subagent-runs.sqlite3'
        if not database.exists():
            return []
        with closing(sqlite3.connect(database)) as connection:
            return [{'id': row[0], 'frozen': json.loads(row[1]), 'state': json.loads(row[2])}
                    for row in connection.execute('SELECT r.id,r.frozen,s.state FROM runs r JOIN run_states s ON r.id=s.id')]

    def tearDown(self):
        output = os.environ.get('EVE_WORKSPACE_NODE_E2E_ARTIFACTS')
        try:
            if output:
                destination = Path(output) / self._testMethodName
                destination.mkdir(parents=True, exist_ok=True)
                data = {'requests': self.requests, 'progress_requests': self.progress_requests,
                        'responses': self.responses, 'events': getattr(self, 'events', []), 'errors': self.errors,
                        'records': self.records(), 'parent_result': self.parent_result,
                        'runtime_manifest': self.runtime_manifest, 'summary': self.summary,
                        'launches': self.launches.read_text().splitlines() if self.launches.exists() else []}
                (destination / 'evidence.json').write_text(json.dumps(data, ensure_ascii=False, indent=2))
                for pattern in ('scenario-*.json', 'events-*.jsonl', 'error-*.txt', 'panel-*.stderr'):
                    for source in self.work.glob(pattern):
                        shutil.copyfile(source, destination / source.name)
        finally:
            super().tearDown()

    def environment(self, configured):
        env = {'EVE_DEVELOPER_CONFIG': str(self.developer_config),
               'EVE_PROGRESS_STAGE_UPDATES': 'false', 'EVE_PROGRESS_TOOL_MILESTONES': 'false',
               'EVE_PROGRESS_SLOW_AFTER_MS': '0', 'EVE_MODEL_USE_CODING_ROLE': 'auxiliary_small',
               'EVE_MODELS_AUXILIARY_SMALL_ENABLED': 'true',
               'EVE_MODELS_AUXILIARY_SMALL_PROVIDER': 'openai',
               'EVE_MODELS_AUXILIARY_SMALL_MODEL': 'local-workspace-coding',
               'EVE_MODELS_AUXILIARY_SMALL_CREDENTIAL_REF': 'env:EVE_AUXILIARY_API_KEY',
               'EVE_AUXILIARY_API_KEY': 'local-workspace-fixture-key',
               'EVE_OPENAI_AUXILIARY_BASE_URL': f'http://127.0.0.1:{self.server.server_port}',
               'EVE_OPENAI_AUXILIARY_PROTOCOL': 'chat', 'EVE_OPENAI_AUXILIARY_REASONING_EFFORT': 'high'}
        if configured:
            rootfs = self.work / 'trusted-node-rootfs'
            digest, bwrap, self.runtime_manifest = prepare_runtime(rootfs)
            launcher = self.work / 'trusted-bwrap'
            # Count actual sandbox launch admissions, then exec the trusted real bubblewrap.
            # This host-owned instrumentation is outside the workspace and never supplied by the model.
            launcher.write_text('#!/bin/sh\n' + 'printf "%s\\n" launch >> ' + shlex.quote(str(self.launches)) +
                                '\nexec ' + shlex.quote(str(bwrap)) + ' "$@"\n')
            launcher.chmod(0o700)
            env.update(EVE_WORKSPACE_NODE_ROOTFS=str(rootfs), EVE_WORKSPACE_NODE_ROOTFS_SHA256=digest,
                       EVE_WORKSPACE_NODE_BINARY='runtime/node', EVE_WORKSPACE_NODE_BWRAP=str(launcher))
        return env

    def selector_advertised(self, body):
        definition = next(tool['function'] for tool in body['tools'] if tool['function']['name'] == 'run_subagent')
        verification = definition['parameters']['properties']['verification']
        return any(item.get('properties', {}).get('kind', {}).get('const') == 'workspace_node_test'
                   for item in verification['oneOf'])

    def task(self):
        return self.choose('run_subagent', {'agent_id': 'coding', 'max_steps': 8,
                           'task': INITIAL, 'verification': SELECTOR})

    def install(self, change_after_test=False):
        def respond(body):
            leaf = '你是 Eve 的子代理' in body['messages'][0]['content']
            (self.child_requests if leaf else self.main_requests).append(body)
            results = self.all_results(body)
            self.assertIn(TOOL, self.definitions(body))
            if leaf:
                self.assertEqual(body['model'], 'local-workspace-coding')
                if not results:
                    return self.choose('write_file', {'path': 'project/value.cjs', 'content': GOOD,
                        'expected_sha256': sha(b'module.exports = 0;\n')})
                if len(results) == 1:
                    self.assertEqual(results[-1]['sha256'], sha(GOOD.encode()), results[-1])
                    return self.choose(TOOL, {'directory': 'project', 'entry': 'test.cjs'})
                if len(results) == 2:
                    self.assertEqual(results[-1]['receipt']['outcome'], {'status': 'exited', 'exit_code': 0, 'signal': None}, results[-1])
                    self.assertIn('WORKSPACE_NODE_REAL_TEST_PASSED', results[-1]['receipt']['stdout'])
                    if change_after_test:
                        return self.choose('write_file', {'path': 'project/value.cjs', 'content': BAD,
                            'expected_sha256': sha(GOOD.encode())})
                    return self.choose('read_file', {'path': 'project/value.cjs'})
                self.assertEqual(len(results), 3)
                self.assertEqual(results[-1]['sha256'], sha((BAD if change_after_test else GOOD).encode()))
                return dialogue.text_response('项目文件已修改并实际执行一次测试，后续操作已完成。')
            self.assertTrue(self.selector_advertised(body))
            if not results:
                return self.task()
            if len(results) == 1:
                self.parent_result = results[0]
                self.assertEqual(results[0]['status'], 'verification_mismatch' if change_after_test else 'completed', results[0])
                self.assertEqual(results[0]['completed'], not change_after_test)
                if change_after_test:
                    self.assertEqual(results[0]['verification']['reason'], 'workspace_changed_since_test')
                self.assertIsNotNone(results[0].get('receipt_evidence'))
                return self.choose('send_message', {'text': CHANGED if change_after_test else PASSED})
            self.assertEqual(len(results), 2)
            self.assertTrue(results[-1]['sent'])
            return dialogue.text_response('内部结束，最终说明已直接发送。')
        self.callback = respond

    def run_workspace(self, configured=True):
        return self.run_live(self.send(self.message('workspace', INITIAL)),
            more=['--subagents'], env_extra=self.environment(configured), panel=True, timeout=60)

    def assert_finished(self, changed):
        self.assertEqual(len(self.child_requests), 4)
        self.assertEqual(len(self.main_requests), 3)
        self.assertEqual(self.launches.read_text().splitlines(), ['launch'], '测试只真实启动一次')
        self.assertEqual([event['text'] for event in self.progress('workspace')], [CHANGED if changed else PASSED])
        [record] = self.records()
        state = record['state']
        [evidence] = state['workspace_node_evidence']
        current = state['workspace_observation']
        self.assertEqual(evidence['input_sha256'] == current['input_sha256'], not changed)
        self.assertEqual(state['verification']['status'], 'mismatch' if changed else 'verified')
        calls = [call for message in state['checkpoint']['messages'] for call in message.get('tool_calls', [])]
        self.assertEqual([call['name'] for call in calls], ['write_file', TOOL, 'write_file' if changed else 'read_file'])
        self.assertEqual(state['usage']['tool_calls'], 3)
        self.assertEqual(record['frozen']['verification']['kind'], 'workspace_node_test')
        self.assertNotIn('input_sha256', record['frozen']['verification'])
        self.assertEqual(self.parent_result['usage']['run_id'], record['id'])
        self.summary = {'completed': not changed, 'main_requests': len(self.main_requests),
                        'child_requests': len(self.child_requests), 'physical_test_launches': 1,
                        'verification': state['verification'], 'input_matches_final': not changed,
                        'original_witness_count': 1, 'parent_direct_send': True}

    def test_edit_test_read_and_direct_parent_reply_reuses_original_evidence(self):
        self.install()
        self.run_workspace()
        self.assert_finished(False)
        # 重启同一业务状态，故意不再装配运行器；只读原记录，不需要 rootfs 或重跑。
        saved = self.records()
        original = self.parent_result['receipt_evidence']
        restart_requests = []

        def read_saved(body):
            restart_requests.append(body)
            self.assertNotIn(TOOL, self.definitions(body))
            # 新用户轮保留完整历史，但按本轮用户消息之后的工具结果推进。
            results = self.results(body)
            if not results:
                return self.choose('list_subagent_runs', {'run_id': saved[0]['id'], 'evidence': True})
            if len(results) == 1:
                self.assertEqual(results[0]['runs'], [original])
                self.assertFalse(results[0]['automatic_resume'])
                self.assertEqual(results[0]['runs'][0]['current_artifact_validity'], 'not_checked')
                return self.choose('send_message', {'text': '已读取原测试证据，没有重新运行测试。'})
            self.assertEqual(len(results), 2)
            self.assertTrue(results[-1]['sent'])
            return dialogue.text_response('原记录读取结束。')

        self.callback = read_saved
        self.run_live(self.send(self.message('restart', '读取刚才那次测试的原证据，不要重新执行。')),
            more=['--subagents'], env_extra=self.environment(False), panel=True, timeout=60)
        self.assertEqual(len(restart_requests), 3)
        self.assertEqual(self.records(), saved)
        self.assertEqual(self.launches.read_text().splitlines(), ['launch'])
        self.assertEqual([event['text'] for event in self.progress('restart')], ['已读取原测试证据，没有重新运行测试。'])
        self.summary.update(restart_requests=3, restart_without_runtime=True,
                            saved_record_unchanged=True, physical_test_launches_after_restart=1)

    def test_edit_after_success_requires_new_verification(self):
        self.install(change_after_test=True)
        self.run_workspace()
        self.assert_finished(True)

    def test_unconfigured_runtime_hides_tool_and_selector_and_rejects_dispatch(self):
        def respond(body):
            self.assertNotIn('你是 Eve 的子代理', body['messages'][0]['content'])
            self.main_requests.append(body)
            self.assertNotIn(TOOL, self.definitions(body))
            self.assertFalse(self.selector_advertised(body))
            results = self.all_results(body)
            if not results:
                return self.task()
            self.assertNotEqual(results[-1].get('completed'), True)
            self.parent_result = results[-1]
            return dialogue.text_response('没有运行时，未启动测试任务。')
        self.callback = respond
        self.run_workspace(configured=False)
        self.assertEqual(len(self.main_requests), 2)
        self.assertEqual(self.records(), [])
        self.assertFalse(self.launches.exists())
        self.summary = {'runtime_configured': False, 'tool_advertised': False,
                        'selector_advertised': False, 'child_requests': 0,
                        'physical_test_launches': 0, 'dispatch_rejected': True}


if __name__ == '__main__':
    unittest.main()
