"""真实 Eve 二进制的入站前恢复屏障；模型和 QQ 都使用本测试持有的本地替身。

同一状态依次以完整、最小、完整组合恢复。屏障内保存实际持久字节证据，
确认后才允许后台学习、QQ 桥和 Web 工作；失败请求不得产生就绪信号。
"""
import contextlib
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import traceback
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from fixture_auth import bootstrap_test_admin, configure_fixture_progress


ROOT = Path(__file__).resolve().parents[3]
BINARY = Path(os.environ.get('EVE_QQBOT_BINARY', ROOT / 'target/debug/eve-qqbot')).resolve()
RUNTIME = ROOT / 'scripts/self_upgrade_runtime.py'
BRIDGE = ROOT / 'connectors/qqbot/test/fake-bridge.mjs'
REQUEST_ENV = 'EVE_STARTUP_CHECKPOINT_REQUEST'
TASK_KEY = 'tasks.v1'
LEDGERS = {
    'tasks': ('eve.task', TASK_KEY, 'format_version'),
    'phrases': ('eve.training', 'phrases.v1', 'version'),
    'receipts': ('eve.channel.qqbot', 'receipts.v1', 'version'),
}
WORDS = ['这游戏yyds', '确实确实，我也这么想', '今天又摸鱼了', '晚上开黑吗',
         '这个boss好难打', '确实确实，太对了', '周末去看电影', '那家店的奶茶yyds',
         '我先去吃饭', '回来再聊', '你觉得呢', '就这样吧']


def digest(data):
    return hashlib.sha256(data).hexdigest()


def encoded(value):
    return json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode()


def atomic_new(path, value):
    """在同目录创建完整控制文件，已存在的请求或确认不能被覆盖。"""
    data = encoded(value)
    with tempfile.NamedTemporaryFile(dir=path.parent) as temporary:
        temporary.write(data)
        temporary.flush()
        os.fsync(temporary.fileno())
        os.link(temporary.name, path)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


@unittest.skipUnless(os.name == 'posix' and Path('/proc/self/stat').exists(), '文件协议要求 Linux 进程身份')
class StartupCheckpointAcceptance(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='eve-startup-checkpoint-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.state = self.root / 'state'
        self.state.mkdir()
        self.empty_path = self.root / 'empty-path'
        self.empty_path.mkdir()
        self.node = shutil.which('node')
        self.assertIsNotNone(self.node)
        self.assertTrue(BINARY.is_file(), '先在 CI 编译实际 eve-qqbot')
        self.binary_sha = digest(BINARY.read_bytes())
        self.runtime_sha = digest(RUNTIME.read_bytes())
        self.requests = []
        self.errors = []
        self.runs = 0
        self._seed()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    outer.assertEqual(self.path, '/v1/chat/completions')
                    body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                    latest = body['messages'][-1].get('content') or ''
                    decoded = json.loads(latest)
                    outer.requests.append(body)
                    outer.assertEqual(decoded, {'messages': WORDS}, '没有业务入站时只有到期说法批次可请求模型')
                    content = encoded({'phrases': [{'kind': 'term', 'text': 'yyds',
                        'meaning': '表示非常好', 'example': WORDS[0]}]}).decode()
                    response = {'choices': [{'index': 0, 'finish_reason': 'stop',
                        'message': {'role': 'assistant', 'content': content}}]}
                    payload = encoded(response)
                    self.send_response(200)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception:
                    outer.errors.append(traceback.format_exc())
                    self.send_error(500)

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self._stop_server)

    def _stop_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)

    def _seed(self):
        scope = {'session_id': 'startup-fixture', 'user_id': 'startup-fixture'}
        documents = {
            'eve.task': {TASK_KEY: {'format_version': 5, 'tasks': []}},
            'eve.training': {'phrases.v1': {'version': 2, 'next': 1,
                'pending': [{'scope': scope, 'text': text} for text in WORDS],
                'learned': [], 'inflight': []}},
            'eve.channel.qqbot': {'receipts.v1': {'version': 10, 'entries': []}},
        }
        self.state_path = self.state / 'state.json'
        self.write_documents(documents)

    def write_documents(self, documents):
        # 仅在本测试持有的 Eve 进程尚未启动时构造旧状态。
        self.state_path.write_bytes(encoded({'version': 1, 'entries': {
            owner: {key: list(encoded(value)) for key, value in entries.items()}
            for owner, entries in documents.items()}}))

    def documents(self):
        return {owner: {key: json.loads(bytes(raw)) for key, raw in entries.items()}
                for owner, entries in json.loads(self.state_path.read_bytes())['entries'].items()}

    def summaries(self):
        entries = json.loads(self.state_path.read_bytes())['entries']
        result = {}
        for name, (owner, key, version) in LEDGERS.items():
            raw = entries.get(owner, {}).get(key)
            if raw is None:
                result[name] = {'present': False, 'version': None, 'bytes': 0, 'sha256': None}
            else:
                raw = bytes(raw)
                result[name] = {'present': True, 'version': json.loads(raw)[version],
                                'bytes': len(raw), 'sha256': digest(raw)}
        return result

    def wait(self, run, predicate, message, timeout=30):
        deadline = time.monotonic() + timeout
        while True:
            result = predicate()
            if result:
                return result
            self.assertFalse(self.errors, '\n'.join(self.errors))
            self.assertIsNone(run['process'].poll(), message + '\n' + self.logs(run))
            self.assertLess(time.monotonic(), deadline, message + '\n' + self.logs(run))
            time.sleep(.02)

    @staticmethod
    def logs(run):
        return run['stderr'].read_text(errors='replace')

    @contextlib.contextmanager
    def launch(self, full, timeout_ms=10_000, request=None):
        self.runs += 1
        directory = self.root / ('run-' + str(self.runs))
        directory.mkdir(mode=0o700)
        run = {name: directory / name for name in ('bridge-pid', 'go', 'processed', 'stop', 'stdout', 'stderr', 'events')}
        scenario = directory / 'scenario.json'
        scenario.write_bytes(encoded({'pid_file': str(run['bridge-pid']), 'events_file': str(run['events']),
            'wait_timeout_ms': 40_000,
            'script': [{'wait_file': str(run['go'])}, {'send': {
                'id': 'checkpoint-' + str(self.runs), 'scope': 'c2c', 'target_id': 'user-1',
                'user_id': 'user-1', 'text': '/help', 'original_text': '/help', 'accept_any': True}},
                {'wait_command': {'id': 'checkpoint-' + str(self.runs), 'type': 'reply'}},
                {'touch': str(run['processed'])}, {'wait_file': str(run['stop'])}]}))
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        run['port'] = port
        if request is None:
            request = directory / 'request.json'
            atomic_new(request, {'format_version': 1, 'nonce': os.urandom(32).hex(),
                'source_revision': 'b' * 40, 'binary_sha256': self.binary_sha,
                'runtime_path': str(RUNTIME), 'runtime_sha256': self.runtime_sha,
                'state_directory': str(self.state), 'checkpoint_path': str(directory / 'checkpoint.json'),
                'acknowledgement_path': str(directory / 'ack.json'), 'timeout_ms': timeout_ms})
        run['request'] = request
        request_value = json.loads(request.read_bytes())
        run['checkpoint'] = Path(request_value['checkpoint_path'])
        run['ack'] = Path(request_value['acknowledgement_path'])
        environment = {key: os.environ[key] for key in ('SystemRoot', 'TEMP', 'TMP') if key in os.environ}
        environment.update(PATH=str(self.empty_path), QQBOT_APP_ID='1904159860',
            QQBOT_APP_SECRET='startup-fixture-qq-credential', EVE_OPENAI_API_KEY='startup-fixture-model-credential',
            EVE_OPENAI_BASE_URL=f'http://127.0.0.1:{self.server.server_port}',
            EVE_OPENAI_PROTOCOL='chat', EVE_LLM_RESPONSE_MODE='complete',
            EVE_WEB_TOKEN='startup-fixture-web-token-not-a-model-key',
            EVE_QQ_LIVE_INTERACTION='false', **{REQUEST_ENV: str(request)})
        bootstrap_test_admin(self.state, environment, users=('user-1',))
        command = [str(BINARY), *([] if full else ['--minimal-profile']),
            '--state-dir', str(self.state), '--agent', str(ROOT / 'AGENT.md'), '--node', self.node,
            '--bridge-script', str(BRIDGE), '--bridge-arg', str(scenario),
            '--web-listen', '127.0.0.1:' + str(port)]
        with run['stdout'].open('wb') as stdout, run['stderr'].open('wb') as stderr:
            run['process'] = subprocess.Popen(command, env=configure_fixture_progress(environment),
                cwd=self.root, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr)
        try:
            yield run
        finally:
            if run['process'].poll() is None:
                run['process'].terminate()
                try:
                    run['process'].wait(timeout=8)
                except subprocess.TimeoutExpired:
                    run['process'].kill()
                    run['process'].wait(timeout=5)

    def assert_closed_ingress(self, run):
        self.assertFalse(run['bridge-pid'].exists(), '确认前不能创建 Node 桥进程')
        self.assertNotIn('EVE_QQBOT_READY', self.logs(run))
        self.assertNotIn('EVE_WEB_READY', self.logs(run))
        with socket.socket() as client:
            client.settimeout(.1)
            self.assertNotEqual(client.connect_ex(('127.0.0.1', run['port'])), 0, '确认前不能监听 Web')

    def checkpoint(self, run, full, stability=True):
        self.wait(run, run['checkpoint'].is_file, '未到达实际恢复检查点')
        payload = run['checkpoint'].read_bytes()
        checkpoint = json.loads(payload)
        self.assertEqual(checkpoint['request_sha256'], digest(run['request'].read_bytes()))
        self.assertEqual(checkpoint['pid'], run['process'].pid)
        stat = Path(f'/proc/{run["process"].pid}/stat').read_text().rsplit(') ', 1)[1].split()
        self.assertEqual(checkpoint['process_start_ticks'], int(stat[19]))
        self.assertEqual(Path(checkpoint['exe_path']), BINARY)
        self.assertEqual(checkpoint['binary_sha256'], self.binary_sha)
        self.assertEqual(checkpoint['runtime_sha256'], self.runtime_sha)
        self.assertEqual(checkpoint['state_directory'], str(self.state))
        self.assertEqual(checkpoint['automatic_capabilities'], full)
        self.assertEqual(checkpoint['ledgers'], self.summaries())
        self.assertEqual({name: value['version'] for name, value in checkpoint['ledgers'].items()},
                         {'tasks': 5, 'phrases': 2, 'receipts': 10})
        saved = self.state_path.read_bytes()
        requests = len(self.requests)
        for _ in range(12 if stability else 1):
            self.assert_closed_ingress(run)
            self.assertEqual(self.state_path.read_bytes(), saved, '等待证明期间不能由后台改写业务状态')
            self.assertEqual(len(self.requests), requests, '等待证明期间不能执行模型请求')
            if stability:
                time.sleep(.1)
        return checkpoint, payload

    def acknowledge(self, run, checkpoint, payload, **changes):
        # 独立测试证明文件确实落盘；发布时由最终 runtime 捕获完整冷/热记录再写此确认。
        proof = {'checkpoint_sha256': digest(payload), 'state_sha256': digest(self.state_path.read_bytes()),
                 'ledgers': self.summaries()}
        proof_path = run['ack'].with_name('preservation.json')
        atomic_new(proof_path, proof)
        acknowledgement = {'format_version': 1, 'nonce': checkpoint['nonce'], 'pid': checkpoint['pid'],
            'process_start_ticks': checkpoint['process_start_ticks'], 'checkpoint_sha256': digest(payload),
            'preservation_sha256': digest(proof_path.read_bytes()), 'accepted': True}
        acknowledgement.update(changes)
        atomic_new(run['ack'], acknowledgement)

    def ready_and_stop(self, run):
        self.wait(run, lambda: 'EVE_WEB_READY' in self.logs(run) and 'EVE_QQBOT_READY' in self.logs(run),
                  '确认后 QQ 和 Web 未正常就绪')
        self.assertTrue(run['bridge-pid'].is_file())
        run['go'].touch()
        self.wait(run, run['processed'].is_file, '确认后真实 QQ 命令未处理')
        run['stop'].touch()
        self.assertEqual(run['process'].wait(timeout=15), 0, self.logs(run))
        summary = json.loads(run['stdout'].read_bytes())
        self.assertTrue(summary['closed'])
        self.assertFalse(summary['terminal_error'])
        self.assertFalse(self.errors, '\n'.join(self.errors))

    def test_full_minimal_full_restore_gates_ingress_and_due_learning(self):
        with self.launch(full=True) as run:
            checkpoint, payload = self.checkpoint(run, full=True)
            self.assertEqual(self.requests, [])
            self.assertEqual(len(self.documents()['eve.training']['phrases.v1']['pending']), len(WORDS))
            self.acknowledge(run, checkpoint, payload)
            self.wait(run, lambda: self.documents()['eve.training']['phrases.v1']['learned'], '确认后说法批次未继续学习')
            self.ready_and_stop(run)
        learned = self.documents()['eve.training']['phrases.v1']['learned']
        self.assertEqual(len(learned), 1)
        self.assertEqual(len(self.requests), 1)
        for full in (False, True):
            with self.subTest(full=full), self.launch(full=full) as run:
                checkpoint, payload = self.checkpoint(run, full=full)
                self.assertEqual(self.documents()['eve.training']['phrases.v1']['learned'], learned)
                self.acknowledge(run, checkpoint, payload)
                self.ready_and_stop(run)
            self.assertEqual(self.documents()['eve.training']['phrases.v1']['learned'], learned)
            self.assertEqual(len(self.requests), 1, '已完成说法批次不能在切换组合后重放')

    @staticmethod
    def learning_scope():
        identity = 'qq:' + digest(encoded(['1904159860', 'c2c', 'user-1', 'user-1']))
        return {'session_id': identity, 'user_id': identity}

    @staticmethod
    def old_receipt(identifier, text, content_type=None):
        message = {'id': identifier, 'scope': 'c2c', 'target_id': 'user-1',
                   'user_id': 'user-1', 'text': text}
        if content_type is not None:
            message['attachments'] = [{'content_type': content_type,
                                       'url': 'https://example.invalid/old-attachment'}]
        return {'app_id': '1904159860', 'message': message, 'state': 'Failed', 'reply': None}

    def seed_learning_history(self, receipts, phrases, expression_rows=()):
        documents = self.documents()
        documents['eve.training'] = {
            'modes.v1': {'version': 1, 'modes': [{'scope': self.learning_scope(), 'enabled': True}]},
            'phrases.v1': phrases,
        }
        if expression_rows:
            documents['eve.training']['expression.v1'] = {'version': 1, 'rows': expression_rows}
        documents['eve.channel.qqbot']['receipts.v1']['entries'] = receipts
        self.write_documents(documents)

    def expression_records(self):
        path = self.state / 'training-expressions.sqlite3'
        if not path.exists():
            return []
        with contextlib.closing(sqlite3.connect(path.as_uri() + '?mode=ro', uri=True)) as connection:
            connection.row_factory = sqlite3.Row
            return [dict(row) for row in connection.execute(
                'SELECT session_id,user_id,id,body,chars,paragraphs,question,formal,casual,active '
                'FROM expressions ORDER BY seq')]

    def assert_no_history_replay(self, run, old_receipts):
        self.assertEqual(self.requests, [], '本地恢复不能请求模型或由模型重放工具')
        receipts = self.documents()['eve.channel.qqbot']['receipts.v1']['entries']
        self.assertEqual(receipts[:len(old_receipts)], old_receipts, '失败回执及原文必须原样保留')
        sent = [event for event in map(json.loads, run['events'].read_text().splitlines())
                if event['direction'] == 'out' and event['type'] in
                ('reply', 'segment', 'progress', 'push', 'file', 'sticker')]
        self.assertEqual([(event['type'], event['id']) for event in sent],
                         [('reply', 'checkpoint-' + str(self.runs))], '只有本次新发的查询命令可以回复')
        self.assertEqual(self.documents()['eve.task'][TASK_KEY]['tasks'], [])

    def test_missing_expression_index_skips_old_attachments_and_recovers_failed_text_once(self):
        scope = self.learning_scope()
        text = '今天有空就聊一会儿'
        receipts = [self.old_receipt('old-image', '这张图真好看', 'image/png'),
                    self.old_receipt('old-image-only', '', 'image/jpeg'),
                    self.old_receipt('old-file', '这里是附件说明', 'application/pdf'),
                    self.old_receipt('old-command', '先稍等\n /train start'),
                    self.old_receipt('old-text', text)]
        unknown = self.old_receipt('old-unknown-image', '未确认发送的图片说明', 'image/png')
        unknown.update(state='Unknown', reply='尚未确认送达的旧回复')
        receipts.append(unknown)
        foreign = self.old_receipt('other-app', '其他应用的原话')
        foreign['app_id'] = 'other-app'
        receipts.append(foreign)
        self.seed_learning_history(receipts, {'version': 2, 'next': 1, 'pending': [],
                                              'learned': [], 'inflight': []})
        self.assertNotIn('expression.v1', self.documents()['eve.training'])
        self.assertEqual(self.expression_records(), [])
        preserved_rows = None
        for full in (True, False, True):
            with self.subTest(full=full, restart=self.runs), self.launch(full=full) as run:
                checkpoint, payload = self.checkpoint(run, full=full)
                self.assertEqual(self.requests, [])
                self.assertEqual(self.documents()['eve.training']['phrases.v1']['pending'],
                                 [{'scope': scope, 'text': text}], '未采集附件不能因重启首次进入待学批次')
                rows = self.expression_records()
                self.assertEqual([(row['id'], row['body'], row['active']) for row in rows],
                                 [(digest(b'old-text'), digest(text.encode()), 1)])
                if preserved_rows is not None:
                    self.assertEqual(rows, preserved_rows, '重启不得重复学习已恢复的普通文本')
                preserved_rows = rows
                self.assertEqual(self.documents()['eve.channel.qqbot']['receipts.v1']['entries'][:len(receipts)],
                                 receipts)
                self.acknowledge(run, checkpoint, payload)
                self.ready_and_stop(run)
                self.assert_no_history_replay(run, receipts)

    def test_existing_attachment_sources_pending_learned_and_retry_failure_survive_restart(self):
        scope = self.learning_scope()
        receipts = [self.old_receipt('learned-image', '这游戏yyds', 'image/png'),
                    self.old_receipt('pending-image', '已经采集的图片说明', 'image/png'),
                    self.old_receipt('reset-image', '已经重置的图片说明', 'image/png')]
        retries = [self.old_receipt('retry-image-' + str(index), '历史图片说明' + str(index), 'image/png')
                   for index in range(len(WORDS))]
        receipts.extend(retries)
        rows = [{'scope': scope, 'id': digest(entry['message']['id'].encode()),
                 'body': digest(entry['message']['text'].encode()),
                 'chars': len(entry['message']['text']), 'paragraphs': 1,
                 'question': False, 'formal': False, 'casual': False,
                 'active': entry['message']['id'] != 'reset-image'} for entry in receipts]
        phrases = {'version': 2, 'next': 3,
            'pending': [{'scope': scope, 'text': receipts[1]['message']['text']}],
            'learned': [{'scope': scope, 'phrase': {'id': 'p1', 'kind': 'term', 'text': 'yyds',
                'meaning': '表示非常好', 'example': receipts[0]['message']['text'], 'uses': 1, 'updated_at_ms': 1}}],
            'inflight': [{'batch': {'id': 'b2', 'scope': scope, 'messages': [entry['message']['text'] for entry in retries],
                'attempt': 1, 'learner_version': 1},
                'phase': {'state': 'retry_wait', 'failure': 'server_error', 'ready_at_ms': 2 ** 63}}]}
        self.seed_learning_history(receipts, phrases, rows)
        original_training = json.loads(self.state_path.read_bytes())['entries']['eve.training']
        preserved_rows = None
        expression_sha = None
        for full in (True, False, True):
            with self.subTest(full=full, restart=self.runs), self.launch(full=full) as run:
                checkpoint, payload = self.checkpoint(run, full=full)
                self.assertEqual(self.documents()['eve.training']['phrases.v1'], phrases)
                restored = self.expression_records()
                self.assertEqual(len(restored), len(rows))
                self.assertEqual([row['id'] for row in restored], [row['id'] for row in rows])
                self.assertEqual([row['active'] for row in restored], [row['active'] for row in rows])
                if preserved_rows is not None:
                    self.assertEqual(restored, preserved_rows)
                preserved_rows = restored
                current_sha = digest((self.state / 'training-expressions.sqlite3').read_bytes())
                if expression_sha is not None:
                    self.assertEqual(current_sha, expression_sha, '重启不改写已有 SQLite 表达来源')
                expression_sha = current_sha
                self.assertEqual(json.loads(self.state_path.read_bytes())['entries']['eve.training'],
                                 original_training, '原去重、重置、待学、已学和重试失败字节不得改写')
                self.acknowledge(run, checkpoint, payload)
                self.ready_and_stop(run)
                self.assert_no_history_replay(run, receipts)
                self.assertEqual(digest((self.state / 'training-expressions.sqlite3').read_bytes()), expression_sha)
                self.assertEqual(json.loads(self.state_path.read_bytes())['entries']['eve.training'], original_training)
        self.assertEqual(self.documents()['eve.training']['phrases.v1'], phrases)

    def test_timeout_does_not_start_bridge_web_or_due_learning(self):
        with self.launch(full=True, timeout_ms=1000) as run:
            self.checkpoint(run, full=True, stability=False)
            self.assertNotEqual(run['process'].wait(timeout=10), 0)
            self.assertIn('等待恢复证明超时', self.logs(run))
            self.assert_closed_ingress(run)
        self.assertEqual(self.requests, [])

    def test_stale_confirmation_never_opens_minimal_ingress(self):
        with self.launch(full=False) as run:
            checkpoint, payload = self.checkpoint(run, full=False)
            self.acknowledge(run, checkpoint, payload, pid=0)
            self.assertNotEqual(run['process'].wait(timeout=10), 0)
            self.assert_closed_ingress(run)
        self.assertEqual(self.requests, [])

    def test_bad_receipts_are_preserved_before_any_checkpoint(self):
        document = json.loads(self.state_path.read_bytes())
        document['entries']['eve.channel.qqbot']['receipts.v1'] = list(b'{"version":255,"entries":[]}')
        self.state_path.write_bytes(encoded(document))
        original = self.state_path.read_bytes()
        for full in (True, False):
            with self.subTest(full=full), self.launch(full=full) as run:
                self.assertNotEqual(run['process'].wait(timeout=15), 0)
                self.assertFalse(run['checkpoint'].exists())
                self.assert_closed_ingress(run)
                self.assertEqual(self.state_path.read_bytes(), original)
        self.assertEqual(self.requests, [])

    def test_reused_request_cannot_reuse_old_confirmation(self):
        with self.launch(full=False) as first:
            checkpoint, payload = self.checkpoint(first, full=False)
            self.acknowledge(first, checkpoint, payload)
            self.ready_and_stop(first)
        original = self.state_path.read_bytes()
        with self.launch(full=False, request=first['request']) as second:
            self.assertNotEqual(second['process'].wait(timeout=10), 0)
            self.assert_closed_ingress(second)
            self.assertEqual(self.state_path.read_bytes(), original)


if __name__ == '__main__':
    unittest.main()
