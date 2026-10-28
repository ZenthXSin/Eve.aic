"""隔离 Eve 连续会话验收：真实模型或显式确定性故障注入，绝不启动正式 QQ。

计划是有顺序的对话/断言，不把独立进程的 echo 次数冒充连续交互。
输出保存输入、模型决策、工具回执、状态检查与累计预算；凭据只在上游代理内存中使用。
"""
import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

from fixture_auth import bootstrap_test_admin, is_progress_request, progress_fixture_text

ROOT = Path(__file__).resolve().parents[3]
BRIDGE = Path(__file__).with_name('fake-bridge.mjs')


def sha(data):
    return hashlib.sha256(data).hexdigest()


def response(message):
    return {'choices': [{'index': 0, 'finish_reason': 'tool_calls' if message.get('tool_calls') else 'stop',
                         'message': {'role': 'assistant', **message}}]}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


class ContinuousTrial:
    def __init__(self, args):
        self.args = args
        self.plan = json.loads(args.plan.read_text())
        self.rounds = self.plan['rounds']
        self.root = args.output.resolve()
        previous = json.loads((self.root / 'report.json').read_text()) if args.resume else {}
        if len(self.rounds) + len(previous.get('rounds', [])) < args.minimum_rounds:
            raise ValueError('计划轮数不足')
        if len({r['id'] for r in self.rounds}) != len(self.rounds):
            raise ValueError('轮次 ID 必须唯一')
        if any(not r.get('checks') for r in self.rounds):
            raise ValueError('每轮必须有可验证断言')
        self.root.mkdir(mode=0o700, parents=True, exist_ok=args.resume)
        self.started = time.monotonic() - previous.get('elapsed_seconds', 0)
        self.binary = self.root / args.binary.name
        if args.resume:
            if previous.get('binary_sha256') != sha(self.binary.read_bytes()) or previous.get('official_qq_connected') is not False:
                raise ValueError('只允许续接同一已校验隔离程序')
            if previous.get('model') != (args.model if args.real_model else 'deterministic-fixture'):
                raise ValueError('续接不能静默更换模型')
        else:
            shutil.copy2(args.binary, self.binary)
        self.state = self.root / 'state'
        self.workspace = self.root / 'workspace'
        self.workspace.mkdir(exist_ok=args.resume)
        for relative, content in ({} if args.resume else self.plan.get('seed_files', {})).items():
            target = (self.root / relative).resolve()
            target.relative_to(self.root)
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open('x', encoding='utf-8') as handle:
                handle.write(content)
        if self.plan.get('owner_fixture') and not args.resume:
            principal = 'qq:' + sha(json.dumps(['1904159860', 'c2c', 'user-1', 'user-1'], separators=(',', ':')).encode())
            config = self.state / 'configuration' / 'config.json'
            config.parent.mkdir(parents=True, exist_ok=True)
            config.write_text(json.dumps({'format_version': 1, 'current': {'revision': 1, 'namespaces': {
                'runtime.people': {'schema_version': 1, 'values': {'owner_name': '青湾验收主人', 'owner_principals': principal}}}}, 'backups': []}, ensure_ascii=False))
        self.key = ''
        if args.real_model:
            if not args.credentials_file or not args.endpoint or not args.model:
                raise ValueError('真实模型需要已授权凭据文件、endpoint、model')
            self.key = json.loads(args.credentials_file.read_text())['EVE_OPENAI_API_KEY']
            if urllib.parse.urlsplit(args.endpoint).scheme != 'https':
                raise ValueError('上游模型必须使用 HTTPS')
        self.local_token = secrets.token_urlsafe(32)
        self.lock = threading.Lock()
        self.requests = json.loads((self.root / 'model-requests.json').read_text()) if args.resume else []
        self.scripted_index = {}
        self.current = None
        self.child = None
        self.phase = max((r['phase'] for r in previous.get('rounds', [])), default=0)
        self.input_tokens = previous.get('input_tokens_accounted', 0)
        self.input_reserved = 0
        self.output_tokens = previous.get('output_tokens_accounted', 0)
        self.output_reserved = 0
        self.abort_reason = None
        self.upstream_errors = 0
        self.report = previous or {'mode': 'real_model' if args.real_model else 'deterministic_fixture',
                       'model': args.model if args.real_model else 'deterministic-fixture',
                       'official_qq_connected': False, 'planned_rounds': len(self.rounds),
                       'binary_sha256': sha(self.binary.read_bytes()), 'rounds': [],
                       'limits': {k: getattr(args, k) for k in ['max_requests', 'max_input_tokens',
                                  'max_output_tokens', 'per_request_output', 'total_seconds', 'round_seconds']}}
        limits = {k: getattr(args, k) for k in ['max_requests', 'max_input_tokens',
                  'max_output_tokens', 'per_request_output', 'total_seconds', 'round_seconds']}
        self.report.setdefault('attempts', []).append({'resume': args.resume, 'plan_sha256': sha(args.plan.read_bytes()),
            'reason': self.plan.get('resume_reason'), 'completed_before': len(self.report['rounds']), 'limits': limits})
        self.report['limits'] = limits
        self.report['planned_rounds'] = len(self.report['rounds']) + len(self.rounds)
        self.report.pop('fatal', None)
        self.report['passed'] = False
        self.report.setdefault('source_head', subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip())
        self.report.setdefault('source_diff_sha256', sha(subprocess.check_output(['git', 'diff', 'HEAD', '--'], cwd=ROOT)))
        self.report['plan_sha256'] = sha(args.plan.read_bytes())
        self.opener = urllib.request.build_opener(NoRedirect())
        trial = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                if self.path != '/v1/chat/completions' or self.headers.get('Authorization') != 'Bearer ' + trial.local_token:
                    self.send_error(401)
                    return
                size = int(self.headers.get('Content-Length', '0'))
                if not 0 < size <= 2 * 1024 * 1024:
                    self.send_error(413)
                    return
                try:
                    body = json.loads(self.rfile.read(size))
                    data, status = trial.model(body)
                except Exception as error:
                    trial.abort_reason = 'gateway_' + (str(error) if isinstance(error, RuntimeError) else type(error).__name__)
                    data, status = {'error': {'message': trial.abort_reason}}, 503
                encoded = json.dumps(data, ensure_ascii=False).encode()
                try:
                    self.send_response(status)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(encoded)))
                    self.end_headers()
                    self.wfile.write(encoded)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.save('plan-' + str(len(self.report['attempts'])) + '.json', self.plan)

    def sanitize(self, value):
        text = json.dumps(value, ensure_ascii=False)
        for secret in [self.key, self.local_token]:
            if secret:
                text = text.replace(secret, '[redacted]')
        return json.loads(text)

    def save(self, name, value):
        path = self.root / name
        temporary = path.with_suffix(path.suffix + '.tmp')
        temporary.write_text(json.dumps(self.sanitize(value), ensure_ascii=False, indent=2) + '\n')
        temporary.replace(path)

    def expand(self, value):
        if isinstance(value, str):
            return value.replace('{output}', str(self.root)).replace('{workspace}', str(self.workspace))
        if isinstance(value, list):
            return [self.expand(v) for v in value]
        if isinstance(value, dict):
            return {k: self.expand(v) for k, v in value.items()}
        return value

    def model(self, body):
        started = time.monotonic()
        body['stream'] = False
        body['model'] = self.args.model if self.args.real_model else 'deterministic-fixture'
        # 不能只靠提供商自报预算；每次请求先预留最大输出。
        body['max_tokens'] = min(int(body.get('max_tokens', self.args.per_request_output)), self.args.per_request_output)
        encoded = json.dumps(body, ensure_ascii=False).encode()
        with self.lock:
            if self.abort_reason or time.monotonic() - self.started > self.args.total_seconds:
                raise RuntimeError('total_deadline_or_previous_failure')
            if len(self.requests) >= self.args.max_requests:
                raise RuntimeError('request_budget')
            if self.input_tokens + self.input_reserved + len(encoded) > self.args.max_input_tokens:
                raise RuntimeError('input_budget')
            if self.output_tokens + self.output_reserved + body['max_tokens'] > self.args.max_output_tokens:
                raise RuntimeError('output_budget')
            self.input_reserved += len(encoded)
            self.output_reserved += body['max_tokens']
            latest_user = next((m.get('content', '') for m in reversed(body.get('messages', [])) if m.get('role') == 'user'), '')
            record = {'index': len(self.requests) + 1, 'round': self.current['id'],
                      'origin': 'current_dialogue' if self.current['text'] in str(latest_user) else 'asynchronous_or_guided',
                      'request': body}
            self.requests.append(record)
        status, result = 200, {}
        try:
            if self.args.real_model:
                request = urllib.request.Request(self.args.endpoint, data=encoded,
                    headers={'Content-Type': 'application/json', 'Authorization': 'Bearer ' + self.key})
                with self.opener.open(request, timeout=min(90, self.args.round_seconds)) as upstream:
                    payload = upstream.read(2 * 1024 * 1024 + 1)
                    if len(payload) > 2 * 1024 * 1024:
                        raise ValueError('response_limit')
                    result, status = json.loads(payload), upstream.status
                    self.upstream_errors = 0
            elif is_progress_request(body):
                result = response({'content': progress_fixture_text(json.loads(body['messages'][-1]['content']))})
            else:
                key = self.current['id']
                index = self.scripted_index.get(key, 0)
                replies = self.current.get('fixture_responses', [])
                if index >= len(replies):
                    raise RuntimeError('unscripted_model_request_' + key)
                self.scripted_index[key] = index + 1
                result = response(self.expand(replies[index]))
            if not result.get('choices'):
                raise ValueError('missing_choices')
        except urllib.error.HTTPError as error:
            status, result = error.code, {'error': {'message': 'upstream_http_' + str(error.code)}}
            self.upstream_errors += 1
            if status not in (429, 502, 503, 504) or self.upstream_errors >= 3:
                self.abort_reason = result['error']['message']
        except Exception as error:
            status, result = 503, {'error': {'message': type(error).__name__}}
            self.abort_reason = 'model_' + type(error).__name__
        finally:
            with self.lock:
                usage = result.get('usage') or {}
                # 无 usage 时使用 UTF-8 字节数作为保守上界，输出按预留额度计。
                self.input_tokens += max(0, int(usage.get('prompt_tokens', len(encoded))))
                self.output_tokens += max(0, int(usage.get('completion_tokens', body['max_tokens'])))
                self.input_reserved -= len(encoded)
                self.output_reserved -= body['max_tokens']
                record.update(response=result, http_status=status,
                              elapsed_ms=round((time.monotonic() - started) * 1000))
                self.save('model-requests.json', self.requests)
        return result, status

    def documents(self):
        path = self.state / 'state.json'
        if not path.exists():
            return {}
        raw = json.loads(path.read_bytes())
        return {owner: {key: json.loads(bytes(v)) for key, v in rows.items()}
                for owner, rows in raw['entries'].items()}

    def wait_file(self, path):
        until = time.monotonic() + self.args.round_seconds
        while not path.exists():
            if self.abort_reason:
                raise RuntimeError(self.abort_reason)
            if self.child.poll() is not None:
                raise RuntimeError('host_exited_before_checkpoint')
            if time.monotonic() > until or time.monotonic() - self.started > self.args.total_seconds:
                raise RuntimeError('round_or_total_deadline')
            time.sleep(0.05)

    def check(self, spec, record, docs):
        kind = spec['kind']
        if kind == 'reply_contains':
            return spec['value'] in record['reply']
        if kind == 'reply_excludes':
            return spec['value'] not in record['reply']
        if kind in ['context_contains', 'context_excludes']:
            context = json.dumps([r['request']['messages'] for r in self.requests if r['round'] == record['id']], ensure_ascii=False)
            found = spec['value'] in context
            return found if kind == 'context_contains' else not found
        if kind == 'tool_called':
            return spec['name'] in [c['function']['name'] for c in record['tool_calls']]
        if kind == 'tool_result_equals':
            ids = [c['id'] for c in record['tool_calls'] if c['function']['name'] == spec['name']]
            for call_id in ids:
                if call_id not in record['tool_receipts']:
                    continue
                value = json.loads(record['tool_receipts'][call_id])
                try:
                    for key in spec['path']:
                        value = value[key]
                    if value == spec['value']:
                        return True
                except (KeyError, IndexError, TypeError):
                    continue
            return False
        if kind == 'tool_result_contains':
            return spec['value'] in json.dumps(record['tool_receipts'], ensure_ascii=False)
        if kind == 'task_status':
            tasks = docs.get('eve.task', {}).get('tasks.v1', {}).get('tasks', [])
            return any(t.get('title') == spec['title'] and t.get('status') == spec['value'] for t in tasks)
        if kind == 'state_equals':
            value = docs
            for key in spec['path']:
                value = value[key]
            return value == spec['value']
        if kind in ['file_equals', 'file_exists']:
            path = (self.root / spec['path']).resolve()
            path.relative_to(self.root)
            return path.is_file() and (kind == 'file_exists' or path.read_text() == spec['value'])
        if kind == 'receipt_sent':
            return record['receipt'].get('state') == 'Sent'
        raise ValueError('未知断言类型: ' + kind)

    def boot(self, rounds):
        self.phase += 1
        steps = []
        for item in rounds:
            message = {'id': item['id'], 'scope': item.get('scope', 'c2c'),
                       'user_id': item.get('user', 'user-1'), 'target_id': item.get('target', item.get('user', 'user-1')),
                       'text': item['text'], 'accept_any': True, **item.get('message_options', {})}
            steps.extend(self.expand(item.get('before_steps', [])))
            steps.append({'send': message})
            steps.extend(self.expand(item.get('after_send_steps', [])))
            steps += [{'wait_receipt': {'path': str(self.state / 'state.json'), 'id': item['id'], 'state': 'Sent'}},
                      {'touch': str(self.root / (item['id'] + '.done'))},
                      {'wait_file': str(self.root / (item['id'] + '.continue'))}]
        events = self.root / f'events-{self.phase}.jsonl'
        self.save(f'scenario-{self.phase}.json', {'script': steps, 'events_file': str(events),
            'error_file': str(self.root / f'bridge-error-{self.phase}'),
            'wait_timeout_ms': (self.args.round_seconds + 10) * 1000})
        env = {k: os.environ[k] for k in ['PATH', 'SystemRoot', 'TEMP', 'TMP'] if k in os.environ}
        env.update(self.expand(self.plan.get('environment', {})))
        if any(k.endswith(('_API_KEY', '_TOKEN', '_SECRET')) for k in self.plan.get('environment', {})):
            raise ValueError('计划不能提供真实凭据')
        env.update(QQBOT_APP_ID='1904159860', QQBOT_APP_SECRET='continuous-local-fixture',
            EVE_OPENAI_API_KEY=self.local_token, EVE_OPENAI_BASE_URL=f'http://127.0.0.1:{self.server.server_port}',
            EVE_OPENAI_MODEL=self.args.model if self.args.real_model else 'deterministic-fixture',
            EVE_OPENAI_PROTOCOL='chat', EVE_LLM_RESPONSE_MODE='complete', EVE_OPENAI_REASONING_EFFORT='none',
            EVE_OPENAI_MAX_OUTPUT_TOKENS=str(self.args.per_request_output),
            EVE_PROGRESS_STAGE_UPDATES='false', EVE_PROGRESS_SLOW_AFTER_MS='0', EVE_PROGRESS_TOOL_MILESTONES='false')
        bootstrap_test_admin(self.state, env, users=('user-1',))
        if self.plan.get('developer', False):
            principal = 'qq:' + sha(json.dumps(['1904159860', 'c2c', 'user-1', 'user-1'], separators=(',', ':')).encode())
            self.save('developer.json', {'workspace': str(self.workspace), 'storage_directory': str(self.root / 'developer-state'),
                'operators': [principal], 'commands': False, 'github': False, 'max_tool_rounds': 12})
            env['EVE_DEVELOPER_CONFIG'] = str(self.root / 'developer.json')
        command = [str(self.binary), '--state-dir', str(self.state), '--agent', str(ROOT / 'AGENT.md'),
            '--bridge-script', str(BRIDGE), '--bridge-arg', str(self.root / f'scenario-{self.phase}.json'),
            '--max-tool-rounds', '12', *self.expand(self.plan.get('arguments', []))]
        self.handles = [(self.root / f'stdout-{self.phase}.log').open('wb'), (self.root / f'stderr-{self.phase}.log').open('wb')]
        self.child = subprocess.Popen(command, cwd=ROOT, env=env, stdin=subprocess.DEVNULL,
                                      stdout=self.handles[0], stderr=self.handles[1])
        return events

    def execute(self):
        try:
            phases = []
            for item in self.rounds:
                if not phases or item.get('restart_before'):
                    phases.append([])
                phases[-1].append(item)
            for rounds in phases:
                self.current = rounds[0]
                events = self.boot(rounds)
                for item in rounds:
                    self.current = item
                    started = time.monotonic()
                    self.wait_file(self.root / (item['id'] + '.done'))
                    docs = self.documents()
                    frames = [json.loads(line) for line in events.read_text().splitlines()]
                    selected = [e for e in frames if e.get('id') == item['id'] and e.get('direction') == 'out']
                    receipt = next(e for e in docs['eve.channel.qqbot']['receipts.v1']['entries'] if e['message']['id'] == item['id'])
                    calls, receipts = [], {}
                    round_requests = [r for r in self.requests if r['round'] == item['id']]
                    for request in round_requests:
                        for choice in request.get('response', {}).get('choices', []):
                            calls += choice['message'].get('tool_calls', [])
                        messages = request['request']['messages']
                        latest_user = max((i for i, m in enumerate(messages) if m.get('role') == 'user'), default=-1)
                        for message in messages[latest_user + 1:]:
                            if message.get('role') == 'tool':
                                receipts[message.get('tool_call_id', str(len(receipts)))] = message.get('content')
                    record = {'id': item['id'], 'phase': self.phase, 'input': item['text'],
                              'reply': '\n'.join(e.get('text', '') for e in selected if e['type'] in ['reply', 'segment']),
                              'tool_calls': calls, 'tool_receipts': receipts, 'receipt': receipt,
                              'request_indexes': [r['index'] for r in round_requests],
                              'elapsed_seconds': round(time.monotonic() - started, 3), 'checks': []}
                    for spec in item['checks']:
                        try:
                            passed = self.check(spec, record, docs)
                            check = {'assertion': spec, 'passed': bool(passed)}
                        except Exception as error:
                            check = {'assertion': spec, 'passed': False, 'error': type(error).__name__}
                        record['checks'].append(check)
                    record['passed'] = all(c['passed'] for c in record['checks'])
                    record['resolves'] = item.get('resolves', [])
                    if record['passed']:
                        for old in self.report['rounds']:
                            if old['id'] in record['resolves']:
                                old['resolved_by'] = record['id']
                    self.report['rounds'].append(record)
                    self.save('report.json', self.report)
                    for relative in item.get('idle_wait_files', []):
                        target = (self.root / relative).resolve()
                        target.relative_to(self.root)
                        self.wait_file(target)
                    print(json.dumps({'round': item['id'], 'passed': record['passed'], 'model_requests': len(self.requests)}, ensure_ascii=False), flush=True)
                    # 下一轮开始前切换归属，避免桥接门打开后的模型请求计入旧轮。
                    next_index = rounds.index(item) + 1
                    if next_index < len(rounds):
                        self.current = rounds[next_index]
                    (self.root / (item['id'] + '.continue')).touch()
                self.child.wait(timeout=20)
                if self.child.returncode:
                    raise RuntimeError('host_nonzero_exit')
                for handle in self.handles:
                    handle.close()
            self.report['all_rounds_passed_first_attempt'] = all(r['passed'] for r in self.report['rounds'])
            self.report['passed'] = all(r['passed'] or r.get('resolved_by') for r in self.report['rounds']) and len(self.report['rounds']) >= self.args.minimum_rounds
        except Exception as error:
            self.report.update(passed=False, fatal=type(error).__name__ + ': ' + str(error))
        finally:
            if self.child and self.child.poll() is None:
                self.child.terminate()
                try:
                    self.child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.child.kill()
                    self.child.wait()
            self.server.shutdown()
            self.server.server_close()
            self.report.update(completed_rounds=len(self.report['rounds']), model_requests=len(self.requests),
                input_tokens_accounted=self.input_tokens, output_tokens_accounted=self.output_tokens,
                elapsed_seconds=round(time.monotonic() - self.started, 3),
                unresolved=[r['id'] for r in self.report['rounds'] if not r['passed'] and not r.get('resolved_by')],
                resolved_failures=[{'round': r['id'], 'resolved_by': r['resolved_by']} for r in self.report['rounds'] if r.get('resolved_by')],
                unexecuted=[r['id'] for r in self.rounds if r['id'] not in {x['id'] for x in self.report['rounds']}])
            self.save('report.json', self.report)
        return self.report['passed']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--plan', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/eve-qqbot')
    parser.add_argument('--real-model', action='store_true')
    parser.add_argument('--resume', action='store_true', help='续接相同输出目录/二进制/模型，保留前序失败和累计预算')
    parser.add_argument('--credentials-file', type=Path)
    parser.add_argument('--endpoint')
    parser.add_argument('--model')
    parser.add_argument('--minimum-rounds', type=int, default=50)
    parser.add_argument('--max-requests', type=int, default=240)
    parser.add_argument('--max-input-tokens', type=int, default=5_000_000)
    parser.add_argument('--max-output-tokens', type=int, default=96_000)
    parser.add_argument('--per-request-output', type=int, default=2048)
    parser.add_argument('--total-seconds', type=int, default=2700)
    parser.add_argument('--round-seconds', type=int, default=180)
    args = parser.parse_args()
    if min(args.minimum_rounds, args.max_requests, args.max_input_tokens, args.max_output_tokens,
           args.per_request_output, args.total_seconds, args.round_seconds) <= 0:
        parser.error('预算与期限必须为正数')
    raise SystemExit(0 if ContinuousTrial(args).execute() else 1)


if __name__ == '__main__':
    main()
