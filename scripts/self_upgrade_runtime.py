"""Eve 实时升级监督器：等待回复送达，交接完整运行资源，保留业务状态。"""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time

VERSION = 1
MAX_JSON = 1024 * 1024
# 文件状态用 JSON 整数数组保存插件字节；16 MiB 回执可膨胀为约 64 MiB。
MAX_STATE_JSON = 256 * 1024 * 1024
RECOVERY_STOP = 78
LEASE_SECONDS = 24 * 3600
RUNTIME_SCRIPTS = ['scripts/build_resources.py', 'scripts/self_upgrade_runtime.py', 'scripts/source_update.py']
RUNTIME_CONTRACT = dict(host_protocol=1, scripts=RUNTIME_SCRIPTS)
HANDOFF_EXIT = 75
MANAGED_FAILURE_EXIT = 76

def value_digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True,
                                    separators=(',', ':')).encode()).hexdigest()

def prepare_lease(directory, manifest):
    """租约独立于不可变候选；重新准备只续租，不覆盖已发布的字节。"""
    now = int(time.time() * 1000)
    return dict(format_version=1, candidate_sha256=manifest['candidate_sha256'],
                manifest_sha256=digest(directory / 'manifest.json'),
                plan_sha256=value_digest(upgrade_plan(manifest)),
                validation_sha256=value_digest(manifest['checks']), state_schema_version=1,
                created_at_ms=now, expires_at_ms=now + LEASE_SECONDS * 1000)

def upgrade_plan(manifest):
    plan = dict(operation='switch_binary_preserve_state', candidate_sha256=manifest['candidate_sha256'],
                state_schema_version=1, validation_policy='format-clippy-tests-release-build-v1')
    if manifest.get('runtime') is not None:
        plan.update(operation='switch_runtime_preserve_state', runtime=manifest['runtime'])
    return plan

def source_revision(manifest):
    source = manifest['source']
    if source['diff'] != hashlib.sha256(b'').hexdigest() or source['untracked'] != hashlib.sha256(b'').hexdigest():
        return None
    revision = source['head']
    return revision if isinstance(revision, str) and len(revision) == 40 and all(c in '0123456789abcdef' for c in revision) else None

def validate_lease(request, directory, manifest):
    lease = request.get('lease')
    if lease is None:
        raise ValueError('prepare_lease_missing')
    fields = {'format_version', 'candidate_sha256', 'manifest_sha256', 'plan_sha256',
              'validation_sha256', 'state_schema_version', 'created_at_ms', 'expires_at_ms'}
    if (not isinstance(lease, dict) or set(lease) != fields
            or type(lease['format_version']) is not int or lease['format_version'] != 1
            or type(lease['state_schema_version']) is not int or lease['state_schema_version'] != 1
            or any(type(lease[k]) is not int for k in ('created_at_ms', 'expires_at_ms'))
            or lease['expires_at_ms'] <= lease['created_at_ms']
            or lease['expires_at_ms'] - lease['created_at_ms'] > LEASE_SECONDS * 1000):
        raise ValueError('prepare_lease_changed')
    expected = prepare_lease(directory, manifest)
    if any(lease[k] != expected[k] for k in fields - {'created_at_ms', 'expires_at_ms'}):
        raise ValueError('prepare_lease_changed')
    now = int(time.time() * 1000)
    if lease['created_at_ms'] > now + 1000 or lease['expires_at_ms'] <= now:
        raise ValueError('prepare_lease_expired')
    return lease

def digest(path):
    limit = 2 * 1024 * 1024 * 1024
    if not path.is_file() or path.is_symlink() or path.stat().st_size > limit:
        raise ValueError('invalid_binary_or_resource')
    with path.open('rb') as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > limit:
            raise ValueError('invalid_binary_or_resource')
        expected = metadata.st_size
        hashed, count = hashlib.sha256(), 0
        while True:
            block = stream.read(min(65536, expected + 1 - count))
            if not block:
                break
            count += len(block)
            if count > expected:
                raise ValueError('binary_or_resource_changed_during_read')
            hashed.update(block)
        if count != expected:
            raise ValueError('binary_or_resource_changed_during_read')
        return hashed.hexdigest()

def unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError('duplicate_field')
        value[key] = item
    return value

def read_json(path, limit=MAX_JSON):
    if path.is_symlink() or not path.is_file() or path.stat().st_size > limit:
        raise ValueError('invalid_json_file')
    # Windows 上另一进程用 os.replace 原子替换同一文件的瞬间，打开会短暂报 PermissionError；
    # 只对这一种错误稍等重试，其他错误照常抛出。
    for attempt in range(20):
        try:
            with path.open('rb') as stream:
                metadata = os.fstat(stream.fileno())
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > limit:
                    raise ValueError('invalid_json_file')
                raw = stream.read(limit + 1)
                if len(raw) > limit:
                    raise ValueError('json_file_grew_beyond_limit')
                return json.loads(raw, object_pairs_hook=unique_object)
        except PermissionError:
            if os.name != 'nt' or attempt == 19:
                raise
            time.sleep(0.05)

def atomic(path, value):
    with tempfile.NamedTemporaryFile(mode='w', encoding='utf8', dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            json.dump(value, stream, ensure_ascii=False, sort_keys=True)
            stream.flush()
            os.fsync(stream.fileno())
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    # Windows 上目标文件正被其他进程读取时替换会短暂报 PermissionError；同 read_json 只对此重试。
    for attempt in range(20):
        try:
            os.replace(temporary, path)
            return
        except PermissionError:
            if os.name != 'nt' or attempt == 19:
                temporary.unlink(missing_ok=True)
                raise
            time.sleep(0.05)

def sha(value):
    return isinstance(value, str) and len(value) == 64 and all(c in '0123456789abcdef' for c in value)

def argument(command, name):
    index = command.index(name)
    return command[index + 1]

# 已从宿主移除的启动参数（连同参数值）；旧部署的启动命令带着它们时，候选版本启动前去掉。
REMOVED_FLAGS = ['--practice-mindustry-json-rules', '--practice-python']

def without_removed_flags(command):
    result = list(command)
    for flag in REMOVED_FLAGS:
        while flag in result:
            index = result.index(flag)
            del result[index:index + 2]
    return result

def replace_argument(command, name, value):
    result = list(command)
    result[result.index(name) + 1] = str(value)
    return result

def validate_candidate(request, candidates):
    required = {'format_version', 'candidate_sha256', 'candidate_directory', 'session_id', 'requested_by', 'turn_id'}
    if not isinstance(request, dict) or set(request) not in (required, required | {'lease'}) or type(request['format_version']) is not int or request['format_version'] != VERSION or not sha(request['candidate_sha256']):
        raise ValueError('invalid_request')
    if type(request['turn_id']) is not int or request['turn_id'] <= 0:
        raise ValueError('invalid_turn')
    directory = Path(request['candidate_directory'])
    if directory.is_symlink() or directory.resolve() != candidates.resolve() / request['candidate_sha256']:
        raise ValueError('candidate_outside_host_directory')
    manifest = read_json(directory / 'manifest.json')
    if (not isinstance(manifest, dict) or type(manifest.get('format_version')) is not int
            or manifest.get('format_version') != VERSION or manifest.get('prepared') is not True
            or manifest.get('success') is not True or manifest.get('candidate_sha256') != request['candidate_sha256']
            or len(manifest.get('checks', [])) != 4
            or any(check.get('success') is not True for check in manifest['checks'])):
        raise ValueError('candidate_not_verified')
    binary = directory / ('eve-qqbot.exe' if os.name == 'nt' else 'eve-qqbot')
    declared_binary = Path(manifest['candidate_path'])
    # /data -> /srv/... 等宿主目录别名可以指向同一个候选；只规范化父目录，
    # 仍拒绝最终文件符号链接、不同文件名和候选根目录外的文件。
    if (not declared_binary.is_absolute() or declared_binary.is_symlink()
            or declared_binary.name != binary.name
            or declared_binary.parent.resolve() != binary.parent.resolve()
            or digest(binary) != manifest['binary_sha256']):
        raise ValueError('candidate_binary_changed')
    resources = manifest['resources']
    if not isinstance(resources, dict) or len(resources) > 10000:
        raise ValueError('invalid_resources')
    required_resources = {'AGENT.md', 'connectors/qqbot/bridge.mjs', 'connectors/qqbot/bridge-core.mjs',
                          'connectors/qqbot/package.json', 'connectors/qqbot/package-lock.json'}
    if not required_resources <= set(resources):
        raise ValueError('missing_runtime_resources')
    runtime = manifest.get('runtime')
    if runtime is not None and (runtime != RUNTIME_CONTRACT or not set(RUNTIME_SCRIPTS) <= set(resources)):
        raise ValueError('invalid_runtime_contract')
    if (directory / 'resources').is_symlink():
        raise ValueError('resource_symlink')
    total = 0
    for name, expected in resources.items():
        relative = PurePosixPath(name)
        if (relative.is_absolute() or '..' in relative.parts or str(relative) != name
                or not (name == 'AGENT.md' or name.startswith(('connectors/qqbot/', 'vendor/'))
                        or runtime is not None and name in RUNTIME_SCRIPTS)):
            raise ValueError('invalid_resource_path')
        path = directory / 'resources'
        for part in relative.parts:
            path /= part
            if path.is_symlink():
                raise ValueError('resource_symlink')
        if digest(path) != expected:
            raise ValueError('resource_changed')
        total += path.stat().st_size
        if total > 64 * 1024 * 1024:
            raise ValueError('resources_too_large')
    identity = {'source': manifest['source'], 'binary_sha256': manifest['binary_sha256'], 'resources': resources}
    source_diff = manifest.get('source_diff')
    if source_diff is not None:
        if (not isinstance(source_diff, dict) or set(source_diff) != {'path', 'sha256', 'base_revision', 'target_revision'}
                or source_diff['path'] != 'source-diff.patch' or not sha(source_diff['sha256'])
                or any(not isinstance(source_diff[k], str) or len(source_diff[k]) != 40
                       or any(c not in '0123456789abcdef' for c in source_diff[k]) for k in ('base_revision', 'target_revision'))
                or source_diff['target_revision'] != manifest['source']['head']):
            raise ValueError('invalid_source_diff')
        path = directory / 'source-diff.patch'
        if path.is_symlink() or not path.is_file() or path.stat().st_size > 128 * 1024:
            raise ValueError('invalid_source_diff_file')
        with path.open('rb') as stream:
            raw = stream.read(128 * 1024 + 1)
        if len(raw) > 128 * 1024 or hashlib.sha256(raw).hexdigest() != source_diff['sha256']:
            raise ValueError('source_diff_changed')
        text = raw.decode('utf8')
        if any(line == 'GIT binary patch' or line.startswith(('Binary files ', 'Submodule '))
               or line.startswith('index ') and line.endswith(' 160000')
               or line in ('new file mode 160000', 'deleted file mode 160000',
                           'old mode 160000', 'new mode 160000') for line in text.splitlines()):
            raise ValueError('source_diff_not_reviewable')
        if source_revision(manifest) is None:
            raise ValueError('source_diff_requires_clean_committed_source')
        identity['source_diff'] = source_diff
    if runtime is not None:
        if source_diff is None:
            raise ValueError('source_diff_required')
        identity['runtime'] = runtime
    actual = hashlib.sha256(json.dumps(identity, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    if actual != request['candidate_sha256']:
        raise ValueError('candidate_identity_changed')
    return binary, directory / 'resources', manifest

def receipt_fingerprint(receipt):
    """完整记录摘要；历史记录只要变化，就重新视作本次运行的在途记录。"""
    return hashlib.sha256(json.dumps(receipt, ensure_ascii=False, sort_keys=True,
                                     separators=(',', ':')).encode()).hexdigest()

def validate_receipt_archive(state_path, receipts):
    if receipts['version'] != 6 and not (receipts['version'] in (7, 8, 9, 10) and 'archive' in receipts):
        if 'archive' in receipts:
            raise ValueError('unexpected_receipt_archive_binding')
        return
    import sqlite3
    binding = receipts.get('archive')
    if not isinstance(binding, dict) or set(binding) != {'identity', 'records'} or not isinstance(binding['identity'], str) or len(binding['identity']) != 32 or not all(c in '0123456789abcdefABCDEF' for c in binding['identity']) or type(binding['records']) is not int or binding['records'] <= 0:
        raise ValueError('invalid_receipt_archive_binding')
    database = state_path.parent / 'qq-receipts.sqlite3'
    if not database.is_file() or database.is_symlink():
        raise ValueError('receipt_archive_missing')
    connection = sqlite3.connect(database.as_uri() + '?mode=ro', uri=True, timeout=2)
    try:
        identity = connection.execute('SELECT version,identity FROM metadata').fetchall()
        count = connection.execute('SELECT count(*) FROM receipts').fetchone()[0]
        if identity != [(1,binding['identity'])] or count < binding['records']:
            raise ValueError('receipt_archive_mismatch')
    finally:
        connection.close()

def inherited_receipts(state_path):
    """启动子进程前捕获历史未决回执；仅供门禁比较，不改写、不重放。"""
    if not state_path.exists():
        return frozenset()
    state = read_json(state_path, MAX_STATE_JSON)
    if state.get('version') != 1:
        raise ValueError('unknown_state_version')
    raw = state['entries'].get('eve.channel.qqbot', {}).get('receipts.v1')
    if raw is None:
        return frozenset()
    receipts = json.loads(bytes(raw), object_pairs_hook=unique_object)
    if receipts['version'] not in (1, 2, 3, 4, 5, 6, 7, 8, 9, 10):
        raise ValueError('unknown_ledger_version')
    validate_receipt_archive(state_path, receipts)
    validate_receipt_entries(receipts)
    return frozenset(receipt_fingerprint(r) for r in receipts['entries']
                     if r['state'] in ('Processing', 'ReplyPending')
                     or any(p.get('state') == 'Sending' for p in r.get('progress', []))
                     or sticker_in_flight(r))

def sticker_in_flight(receipt):
    """格式 5 正文发完后附带的表情包：待发或发送中都算在途投递。"""
    sticker = receipt.get('sticker')
    return isinstance(sticker, dict) and sticker.get('state') in ('Pending', 'Sending') \
        and receipt['state'] == 'Sent'

def session_turns(state_path, session, version):
    """冷索引逐轮核验，不恢复整本历史，也不修改冷文件。"""
    if version == 1:
        yield from session['turns']
        return
    if version != 2 or set(session) != {'key', 'revision', 'turns'}:
        raise ValueError('invalid_session_index')
    revision = 0
    records = session['turns']
    if not isinstance(records, list) or type(session['revision']) is not int:
        raise ValueError('invalid_session_index')
    for position, record in enumerate(records, 1):
        if (not isinstance(record, dict) or set(record) != {'hash', 'bytes'}
                or not sha(record['hash']) or type(record['bytes']) is not int
                or not 0 < record['bytes'] <= 32 * 1024 * 1024):
            raise ValueError('invalid_session_archive_reference')
        path = state_path.parent / 'session-archive' / (record['hash'] + '.json')
        if path.is_symlink() or not path.is_file() or path.stat().st_size != record['bytes']:
            raise ValueError('invalid_session_archive_file')
        with path.open('rb') as stream:
            raw = stream.read(record['bytes'] + 1)
        if len(raw) != record['bytes'] or hashlib.sha256(raw).hexdigest() != record['hash']:
            raise ValueError('session_archive_digest_mismatch')
        turn = json.loads(raw, object_pairs_hook=unique_object)
        if (not isinstance(turn, dict) or set(turn) != {'id', 'input', 'status'}
                or type(turn['id']) is not int or turn['id'] != position
                or not isinstance(turn['input'], str) or not turn['input'].strip()
                or not isinstance(turn['status'], dict)):
            raise ValueError('invalid_session_archive_turn')
        status = turn['status']
        kind = status.get('state')
        if kind in ('Pending', 'Interrupted'):
            if set(status) != {'state'} or kind == 'Pending' and position != len(records):
                raise ValueError('invalid_session_archive_status')
        elif kind == 'Completed':
            messages = status.get('messages')
            if (set(status) != {'state', 'messages'} or not isinstance(messages, list) or len(messages) < 2
                    or not isinstance(messages[0], dict) or not isinstance(messages[-1], dict)
                    or messages[0].get('role') != 'User' or messages[0].get('text') != turn['input']
                    or messages[-1].get('role') != 'Assistant' or not isinstance(messages[-1].get('text'), str)
                    or messages[-1].get('tool_calls')):
                raise ValueError('invalid_session_archive_completion')
        elif kind == 'Failed':
            if set(status) != {'state', 'failure'} or not isinstance(status['failure'], dict):
                raise ValueError('invalid_session_archive_failure')
        else:
            raise ValueError('unknown_session_archive_status')
        revision += 1 if kind == 'Pending' else 2
        yield turn
    if revision != session['revision']:
        raise ValueError('session_archive_revision_mismatch')

def delivery_documents(state_path, inherited=frozenset()):
    """共用的回执格式、归档、分段及进展校验；不修改任何状态。"""
    state = read_json(state_path, MAX_STATE_JSON)
    if state['version'] != 1:
        raise ValueError('unknown_state_version')
    entries = state['entries']
    def document(owner, key):
        return json.loads(bytes(entries[owner][key]), object_pairs_hook=unique_object)
    sessions = document('eve.session', 'sessions.v1')
    receipts = document('eve.channel.qqbot', 'receipts.v1')
    if sessions['format_version'] not in (1, 2) or receipts['version'] not in (1, 2, 3, 4, 5, 6, 7, 8, 9, 10):
        raise ValueError('unknown_ledger_version')
    if sessions['format_version'] == 2:
        if set(sessions) != {'format_version', 'sessions'} or not isinstance(sessions['sessions'], dict):
            raise ValueError('invalid_session_index')
        for identity, session in sessions['sessions'].items():
            if (not isinstance(session, dict) or not isinstance(session.get('key'), dict)
                    or set(session['key']) != {'session_id', 'user_id'}
                    or session['key']['session_id'] != identity
                    or any(not isinstance(value, str) or not value or len(value.encode()) > 256
                           or value.strip() != value or any(ord(c) < 32 or ord(c) == 127 for c in value)
                           for value in session['key'].values())):
                raise ValueError('invalid_session_index_identity')
    validate_receipt_archive(state_path, receipts)
    return sessions, receipts, validate_receipt_entries(receipts, inherited)

def validate_receipt_anchor_version(receipts):
    """格式 10 一经使用便保留；null 与 Rust 的 None 相同，不暗中升级旧账本。"""
    if not isinstance(receipts.get('entries'), list):
        raise ValueError('invalid_receipt_ledger')
    for receipt in receipts['entries']:
        if not isinstance(receipt, dict) or not isinstance(receipt.get('progress', []), list):
            raise ValueError('invalid_progress_ledger')
        if receipts['version'] < 10 and any(isinstance(part, dict) and part.get('reply_anchor') is not None
                                          for part in receipt.get('progress', [])):
            raise ValueError('invalid_reply_anchor_version')

def validate_receipt_entries(receipts, inherited=frozenset()):
    """只核对当前分区；热区不能借冷区补齐锚点，完整冷区也应单独调用。"""
    version = receipts['version']
    if type(version) is not int or version not in range(1, 11):
        raise ValueError('unknown_ledger_version')
    if not isinstance(receipts.get('entries'), list):
        raise ValueError('invalid_receipt_ledger')
    validate_receipt_anchor_version(receipts)
    settled = True
    for receipt in receipts['entries']:
        if version == 10:
            validate_anchored_receipt_shape(receipt)
        control_turn = receipt.get('control_turn_id')
        if control_turn is not None and (version < 9 or type(control_turn) is not int
                                        or control_turn <= 0 or receipt['state'] != 'Handled'):
            raise ValueError('invalid_handled_turn_binding')
        progress = receipt.get('progress', [])
        if not isinstance(progress, list) or len(progress) > (64 if version >= 8 else 3):
            raise ValueError('invalid_progress_ledger')
        if progress and version < 3:
            raise ValueError('invalid_progress_ledger')
        if version < 10 and sum(p.get('proactive') is not True for p in progress if isinstance(p, dict)) > 3:
            raise ValueError('invalid_passive_progress_budget')
        sticker = receipt.get('sticker')
        if sticker is not None and (version < 5 or not isinstance(sticker, dict)
                                    or set(sticker) != {'path', 'state'}
                                    or not isinstance(sticker['path'], str) or not sticker['path']
                                    or sticker['state'] not in ('Pending', 'Sending', 'Sent', 'Failed', 'Skipped', 'Unknown')):
            raise ValueError('invalid_sticker_ledger')
        if sticker_in_flight(receipt) and receipt_fingerprint(receipt) not in inherited:
            settled = False
        passive = {}
        for part in progress:
            authored = version >= 9 and isinstance(part, dict) and part.get('decision_id') is not None
            allowed_fields = {'text', 'state', 'proactive', 'necessary', 'reply_anchor'}
            if version >= 9:
                allowed_fields |= {'decision_id', 'platform_message_id', 'image_path'}
            if (not isinstance(part, dict) or not {'text', 'state'} <= set(part)
                    or not set(part) <= allowed_fields
                    or ('necessary' in part and (type(part['necessary']) is not bool
                        or version < 10 and part['necessary'] is not True))
                    or ('proactive' in part and (version < 8 or type(part['proactive']) is not bool
                        or version < 10 and part['proactive'] is not True))
                    or not isinstance(part['text'], str) or not part['text'].strip()
                    or len(part['text'].encode()) > (32768 if authored else 1024)
                    or part['state'] not in ('Sending', 'Sent', 'Failed', 'Unknown')):
                raise ValueError('invalid_progress_ledger')
            if authored and (not platform_id(part['decision_id'])
                    or part['state'] == 'Sent' and not platform_id(part.get('platform_message_id'))
                    or part.get('platform_message_id') is not None and
                       (not platform_id(part['platform_message_id']) or part['state'] != 'Sent')):
                raise ValueError('invalid_expression_receipt')
            image = part.get('image_path')
            if image is not None and (not authored or not isinstance(image, str) or not image
                    or len(image.encode()) > 4096 or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in image)):
                raise ValueError('invalid_expression_image')
            if not authored and part.get('platform_message_id') is not None:
                raise ValueError('unexpected_expression_receipt')
            anchor = part.get('reply_anchor')
            if anchor is not None and (not platform_id(anchor) or anchor == receipt['message']['id']
                    or receipt['message']['scope'] != 'group' or part.get('proactive', False) or not authored):
                raise ValueError('invalid_reply_anchor')
            if version == 10 and not part.get('proactive', False):
                effective = anchor if anchor is not None else receipt['message']['id']
                passive[effective] = passive.get(effective, 0) + 1
                if passive[effective] > 3:
                    raise ValueError('invalid_passive_progress_budget')
            if part['state'] == 'Sending' and receipt_fingerprint(receipt) not in inherited:
                settled = False
        if version == 10:
            original = passive.get(receipt['message']['id'], 0)
            segments = receipt.get('segments')
            if original + (len(segments['parts']) if segments is not None else 1) + int(sticker is not None) > 5:
                raise ValueError('invalid_passive_progress_budget')
    validate_merged_receipts(receipts)
    return settled

def platform_id(value):
    return (isinstance(value, str) and 0 < len(value.encode()) <= 253 and value.strip() == value
            and not any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in value))

def validate_anchored_receipt_shape(receipt):
    """v10 的身份与原窗口结构；分段和表情也必须计入原窗口额度。"""
    def qq_id(value):
        return (isinstance(value, str) and 0 < len(value) <= 128
                and all(c.isascii() and (c.isalnum() or c in '_-') for c in value))
    if (not isinstance(receipt, dict) or not {'app_id', 'message', 'state'} <= set(receipt)
            or not set(receipt) <= {'app_id', 'message', 'state', 'reply', 'control_turn_id',
                                   'segments', 'progress', 'merged_into', 'sticker'}
            or not qq_id(receipt['app_id']) or not isinstance(receipt['message'], dict)
            or receipt['state'] not in ('Processing', 'ReplyPending', 'Sent', 'Failed', 'Unknown', 'Merged', 'Handled')):
        raise ValueError('invalid_receipt_ledger')
    message = receipt['message']
    if (not {'id', 'scope', 'target_id', 'user_id', 'text'} <= set(message)
            or not platform_id(message['id']) or message['scope'] not in ('c2c', 'group')
            or not qq_id(message['target_id']) or not qq_id(message['user_id'])
            or message['scope'] == 'c2c' and message['target_id'] != message['user_id']
            or not isinstance(message['text'], str) or len(message['text'].encode()) > 32768):
        raise ValueError('invalid_receipt_identity')
    reply, segments, sticker = (receipt.get(key) for key in ('reply', 'segments', 'sticker'))
    state = receipt['state']
    into = receipt.get('merged_into')
    if (reply is not None and (not isinstance(reply, str) or not reply.strip() or len(reply.encode()) > 32768)
            or state in ('ReplyPending', 'Sent', 'Unknown') and reply is None
            or state in ('Handled', 'Merged') and any(value is not None for value in (reply, segments, sticker))
            or (state == 'Merged') != (into is not None)
            or into is not None and (not platform_id(into) or into == message['id'])):
        raise ValueError('invalid_receipt_ledger')
    if sticker is not None:
        if (not isinstance(sticker, dict) or not isinstance(sticker.get('path'), str)
                or len(sticker['path'].encode()) > 4096
                or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in sticker['path'])
                or state not in ('ReplyPending', 'Sent', 'Failed', 'Unknown')
                or state == 'ReplyPending' and sticker.get('state') != 'Pending'
                or state == 'Failed' and sticker.get('state') not in ('Pending', 'Skipped')):
            raise ValueError('invalid_sticker_ledger')
    if segments is not None:
        validate_receipt_segments(segments, reply, state)

def validate_receipt_segments(segments, reply, state):
    if (not isinstance(segments, dict) or set(segments) != {'planner', 'parts'}
            or not isinstance(segments['planner'], str) or not 0 < len(segments['planner']) <= 64
            or not all(c.isascii() and (c.isalnum() or c in '._-') for c in segments['planner'])
            or not isinstance(segments['parts'], list) or not 2 <= len(segments['parts']) <= 8
            or not isinstance(reply, str)):
        raise ValueError('invalid_segment_ledger')
    encoded, end, states, ranges = reply.encode(), 0, [], []
    for part in segments['parts']:
        if (not isinstance(part, dict) or set(part) != {'start', 'end', 'state'}
                or type(part['start']) is not int or type(part['end']) is not int
                or part['start'] < end or not part['start'] < part['end'] <= len(encoded)):
            raise ValueError('invalid_segment_ledger')
        try:
            gap = encoded[end:part['start']].decode()
            text = encoded[part['start']:part['end']].decode()
        except UnicodeError as error:
            raise ValueError('invalid_segment_ledger') from error
        if gap.strip() or text.strip() != text or all(c.isspace() or c == '\ufeff' for c in text):
            raise ValueError('invalid_segment_ledger')
        end = part['end']
        states.append(part['state'])
        ranges.append((part['start'], part['end']))
    sent = 0
    while sent < len(states) and states[sent] == 'Sent':
        sent += 1
    rest = states[sent:]
    pair = {'ReplyPending': ('Sending', 'Pending'), 'Failed': ('Failed', 'Skipped'),
            'Unknown': ('Unknown', 'Skipped')}.get(state)
    if encoded[end:].decode().strip() or not (state == 'Sent' and not rest or pair and rest
            and all(value == pair[1] for value in (rest[1:] if rest[0] == pair[0] else rest))):
        raise ValueError('invalid_segment_ledger')
    # 与分段契约一致：完整代码围栏必须由同一片段覆盖。
    spans, opened, offset = [], None, 0
    lines = encoded.split(b'\n')
    for index, line in enumerate(lines):
        if index < len(lines) - 1:
            line += b'\n'
        content = line.rstrip(b'\n\r')
        body = content.lstrip(b' \t')
        indent = len(content) - len(body)
        marker = body[:1]
        run = len(body) - len(body.lstrip(marker)) if marker in (b'`', b'~') else 0
        if run >= 3:
            info = body[run:]
            if opened is None and (marker == b'~' or b'`' not in info):
                opened = (offset + indent, marker, run)
            elif opened is not None and marker == opened[1] and run >= opened[2] and not info.decode().strip():
                spans.append((opened[0], offset + indent + run))
                opened = None
        offset += len(line)
    if opened is not None:
        spans.append((opened[0], len(reply.rstrip().encode())))
    if any(not any(start <= left and right <= end for start, end in ranges) for left, right in spans):
        raise ValueError('invalid_segment_ledger')

def validate_ended_exploration(task, version):
    """只读核对探索终态；没有小结或经历确认不代表仍在执行。"""
    execution, usage = task.get('execution'), task.get('usage')
    checkpoint = execution.get('checkpoint') if isinstance(execution, dict) else None
    owner, spec = task.get('owner'), task.get('spec')
    def unsigned(value, maximum=2**64 - 1):
        return type(value) is int and 0 <= value <= maximum
    if (version == 1 or not isinstance(execution, dict) or execution.get('kind') != 'exploration'
            or execution.get('session_end_reason') not in ('natural_stop', 'tool_budget_exhausted',
                                                         'deadline_expired', 'recovery_limit_reached')
            or execution.get('completion_contract') is not None or execution.get('progress_notices', []) != []
            or execution.get('resume_at_ms') is not None or not unsigned(execution.get('recoveries'), 3)
            or task.get('cancel_requested') is not False or task.get('notice') is not None
            or task.get('failure') is not None or not isinstance(task.get('result'), str)
            or len(task['result'].encode()) > 16384
            or not isinstance(owner, dict) or set(owner) != {'session_id', 'user_id'}
            or any(not isinstance(value, str) or not value for value in owner.values())
            or not isinstance(spec, dict) or not unsigned(spec.get('max_steps'), 5000) or spec['max_steps'] == 0
            or not isinstance(usage, dict) or set(usage) != {'tool_calls', 'model_requests'}
            or any(not unsigned(value, 2**32 - 1) for value in usage.values())
            or not isinstance(checkpoint, dict) or type(checkpoint.get('version')) is not int
            or checkpoint['version'] not in (1, 2) or checkpoint.get('scope') != owner
            or checkpoint.get('pending_calls') != []):
        raise ValueError('invalid_exploration_session_end')
    times = [task.get(key) for key in ('created_at_ms', 'started_at_ms', 'finished_at_ms')]
    if any(not unsigned(at) or at == 0 for at in times) or times != sorted(times):
        raise ValueError('invalid_exploration_session_end')
    created, started, finished = times
    last_finished, acknowledged = (execution.get(key) for key in
                                   ('last_finished_at_ms', 'exploration_acknowledged_at_ms'))
    deadline, authorization = execution.get('deadline_at_ms'), execution.get('authorization_deadline_at_ms')
    if (last_finished is not None and (not unsigned(last_finished) or last_finished != finished)
            or acknowledged is not None and (not unsigned(acknowledged) or acknowledged < finished)
            or deadline is not None and (not unsigned(deadline) or deadline <= started)
            or authorization is not None and (not unsigned(authorization) or authorization <= created)
            or deadline is not None and authorization is not None and deadline > authorization):
        raise ValueError('invalid_exploration_session_end')
    used, requests = checkpoint.get('used_tool_calls'), checkpoint.get('provider_requests')
    delegated, delegated_requests = (checkpoint.get(key, 0) for key in
                                     ('delegated_tool_calls', 'delegated_model_requests'))
    if (not unsigned(used, spec['max_steps']) or not unsigned(delegated, spec['max_steps'])
            or not unsigned(requests, spec['max_steps'] + 16)
            or not unsigned(delegated_requests, spec['max_steps'] * 2 + 16)
            or checkpoint['version'] == 1 and (delegated or delegated_requests)
            or used + delegated > spec['max_steps']
            or usage != {'tool_calls': used + delegated, 'model_requests': requests + delegated_requests}):
        raise ValueError('invalid_exploration_session_end')
    messages = checkpoint.get('messages')
    if not isinstance(messages, list) or not messages:
        raise ValueError('invalid_exploration_checkpoint')
    calls, results, batches, pending = set(), set(), 0, set()
    for message in messages:
        if (not isinstance(message, dict) or message.get('role') not in ('System', 'User', 'Assistant', 'Tool')
                or not isinstance(message.get('tool_calls'), list) or not isinstance(message.get('tool_results'), list)
                or bool(pending) != (message['role'] == 'Tool')):
            raise ValueError('invalid_exploration_checkpoint')
        role, text = message['role'], message.get('text')
        if (role in ('System', 'User') and (not isinstance(text, str) or not text.strip())
                or role == 'Assistant' and (text is not None if message['tool_calls'] else
                                           not isinstance(text, str) or not text.strip())
                or role == 'Tool' and (text is not None or not message['tool_results'])
                or type(message.get('internal_only', False)) is not bool
                or message.get('internal_only', False) and (role != 'Assistant' or message['tool_calls'])):
            raise ValueError('invalid_exploration_checkpoint')
        batches += bool(message['tool_calls'])
        for call in message['tool_calls']:
            if (role != 'Assistant' or not isinstance(call, dict)
                    or not isinstance(call.get('id'), str) or not call['id'].strip() or call['id'] in calls
                    or not isinstance(call.get('name'), str) or not call['name'].strip()
                    or not isinstance(call.get('arguments'), dict)):
                raise ValueError('invalid_exploration_checkpoint')
            calls.add(call['id'])
            pending.add(call['id'])
        for result in message['tool_results']:
            if (role != 'Tool' or not isinstance(result, dict)
                    or not isinstance(result.get('call_id'), str) or result['call_id'] not in pending
                    or result['call_id'] in results or not isinstance(result.get('output'), dict)):
                raise ValueError('invalid_exploration_checkpoint')
            output = result['output']
            if set(output) != {'Success'}:
                failure = output.get('Failure')
                if (set(output) != {'Failure'} or not isinstance(failure, dict)
                        or set(failure) != {'code', 'message'} or not isinstance(failure['message'], str)
                        or failure['code'] not in ('UnknownTool', 'Unavailable', 'OwnerMismatch', 'Inactive',
                                                  'PermissionDenied', 'InvalidArguments')):
                    # TimedOut / Cancelled / ExecutionFailed 在 Rust 中属于未知效果，不能是 SessionEnded。
                    raise ValueError('invalid_exploration_checkpoint')
            results.add(result['call_id'])
            pending.remove(result['call_id'])
        if role == 'Tool' and pending:
            raise ValueError('invalid_exploration_checkpoint')
    if (pending or calls != results or len(calls) != used or type(checkpoint.get('tool_batches')) is not int
            or checkpoint['tool_batches'] != batches or requests < batches):
        raise ValueError('invalid_exploration_checkpoint')
    saved = checkpoint.get('result')
    remaining, request = checkpoint.get('remaining'), checkpoint.get('pending_model_request')
    if (saved is not None and (not isinstance(saved, str) or not saved.strip() or requests == 0)
            or not unsigned(checkpoint.get('continuation_checks'), 3) or type(checkpoint.get('closing')) is not bool
            or remaining is not None and (not isinstance(remaining, str) or not remaining.strip()
                                          or len(remaining.encode()) > 2048 or saved is not None and not checkpoint['closing'])
            or request is not None and (not unsigned(request, requests) or request == 0 or saved is not None)
            or checkpoint.get('pending_request_fingerprint') is not None and request is None):
        raise ValueError('invalid_exploration_session_end')
    retained = saved if saved is not None else ''
    if len(retained.encode()) > 16384:
        retained = retained.strip()
        if len(retained.encode()) > 16384:
            mark = '…（已截断）'
            retained = retained.encode()[:16384 - len(mark.encode())].decode('utf8', errors='ignore').rstrip() + mark
    reason = ('tool_budget_exhausted' if used + delegated >= spec['max_steps'] else
              'natural_stop' if saved is not None else
              'deadline_expired' if any(at is not None and at <= finished for at in (deadline, authorization)) else
              'recovery_limit_reached' if execution['recoveries'] >= 3 else 'natural_stop')
    if task['result'] != retained or execution['session_end_reason'] != reason:
        raise ValueError('invalid_exploration_session_end')

def task_documents(state_path, ledger):
    """核对任务目录与不可变冷正文；空闲门不能把目录当完整任务。"""
    if (not isinstance(ledger, dict) or set(ledger) != {'format_version', 'tasks'}
            or type(ledger['format_version']) is not int or ledger['format_version'] not in (1, 2, 3, 4)
            or not isinstance(ledger['tasks'], list) or len(ledger['tasks']) > 128):
        raise ValueError('unknown_task_ledger_version')
    version = ledger['format_version']
    seen, total = set(), 0
    for entry in ledger['tasks']:
        if version == 4:
            if not isinstance(entry, dict) or set(entry) != {'directory', 'record'}:
                raise ValueError('invalid_task_archive_index')
            record, directory = entry['record'], entry['directory']
            if (not isinstance(record, dict) or set(record) != {'hash', 'bytes'}
                    or not sha(record['hash']) or type(record['bytes']) is not int
                    or not 0 < record['bytes'] <= 16 * 1024 * 1024):
                raise ValueError('invalid_task_archive_reference')
            total += record['bytes']
            if total > 16 * 1024 * 1024:
                raise ValueError('task_archive_size_limit')
            path = state_path.parent / 'task-archive' / (record['hash'] + '.json')
            if path.is_symlink() or not path.is_file() or path.stat().st_size != record['bytes']:
                raise ValueError('invalid_task_archive_file')
            with path.open('rb') as stream:
                raw = stream.read(record['bytes'] + 1)
            if len(raw) != record['bytes'] or hashlib.sha256(raw).hexdigest() != record['hash']:
                raise ValueError('task_archive_digest_mismatch')
            task = json.loads(raw, object_pairs_hook=unique_object)
            if not isinstance(task, dict) or not isinstance(task.get('spec'), dict):
                raise ValueError('invalid_task_archive_body')
            projected = dict(task, result=None, failure=None, spec=dict(task['spec']))
            projected['spec'].pop('context', None)
            if task.get('execution') is not None:
                if not isinstance(task['execution'], dict):
                    raise ValueError('invalid_task_archive_execution')
                projected['execution'] = dict(task['execution'], checkpoint=None)
                # Rust 冷目录不复制授权原话；完整证据仍由摘要绑定的冷正文保存。
                projected['execution'].pop('review_messages', None)
            if projected != directory:
                raise ValueError('task_archive_directory_mismatch')
        else:
            task = entry
        if (not isinstance(task, dict) or not isinstance(task.get('id'), str) or not task['id']
                or task['id'] in seen or not isinstance(task.get('status'), str)
                or task['status'] not in ('Queued', 'Running', 'Completed', 'SessionEnded', 'Blocked', 'Failed', 'Cancelled', 'Interrupted')
                or version < 3 and task['status'] == 'Blocked'):
            raise ValueError('invalid_task_ledger_record')
        seen.add(task['id'])
        if task['status'] == 'SessionEnded':
            validate_ended_exploration(task, version)
        notice = task.get('notice')
        if notice is not None and (not isinstance(notice, dict)
                or notice.get('status') not in ('Pending', 'Delivering', 'Delivered', 'Expired', 'Unknown')):
            raise ValueError('invalid_task_notice')
        yield task

def runtime_idle_gate(state_path, inherited=frozenset()):
    """对话、任务、唤醒和投递空闲后切换；合法 SessionEnded 不要求完成通知或经历确认。"""
    sessions, receipts, settled = delivery_documents(state_path, inherited)
    if not settled or any(t['status']['state'] == 'Pending'
            for session in sessions['sessions'].values() for t in session_turns(state_path, session, sessions['format_version'])):
        return False
    if any(receipt_fingerprint(r) not in inherited and r['state'] in ('Processing', 'ReplyPending')
            for r in receipts['entries']):
        return False
    state = read_json(state_path, MAX_STATE_JSON)
    for owner, key, field, busy in [
        ('eve.task', 'tasks.v1', 'tasks', ('Queued', 'Running')),
        ('eve.trigger', 'triggers.v1', 'triggers', ('Ready', 'Running', 'Firing', 'Executing')),
        ('eve.reminder', 'reminders.v1', 'reminders', ('Delivering',)),
        ('eve.outreach', 'outreach.v1', 'invitations', ('Composing', 'Delivering')),
    ]:
        raw = state['entries'].get(owner, {}).get(key)
        if raw:
            ledger = json.loads(bytes(raw), object_pairs_hook=unique_object)
            rows = task_documents(state_path, ledger) if owner == 'eve.task' else ledger[field]
            # 只有遍历并核验全部冷记录后才能打开门。
            if any(isinstance(row['status'], str) and row['status'] in busy
                   or row.get('delivery') == 'Sending'
                   or (row.get('notice') or {}).get('status') == 'Delivering'
                   for row in rows):
                return False
    group = state_path.parent / 'group-observations.sqlite3'
    interests = state_path.parent / 'self-interests.sqlite3'
    if interests.with_suffix('.required').exists() and not interests.exists():
        raise ValueError('self_interests_database_missing')
    if interests.exists() and (interests.is_symlink() or not interests.is_file()):
        raise ValueError('invalid_self_interests_database')
    if group.with_suffix('.required').exists() and not group.exists():
        raise ValueError('group_database_missing')
    if group.exists():
        import sqlite3
        with contextlib.closing(sqlite3.connect(group.as_uri() + '?mode=ro', uri=True, timeout=1)) as db:
            if db.execute("SELECT 1 FROM outbound WHERE status IN ('queued','sending') LIMIT 1").fetchone():
                return False
            if db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='speech_reviews'").fetchone():
                if db.execute("SELECT 1 FROM speech_reviews WHERE state='evaluating' LIMIT 1").fetchone():
                    return False
    speech = state_path.parent / 'speech.sqlite3'
    if speech.with_suffix('.required').exists() and not speech.exists():
        raise ValueError('speech_database_missing')
    if speech.exists():
        import sqlite3
        if speech.is_symlink() or not speech.is_file():
            raise ValueError('invalid_speech_database')
        with contextlib.closing(sqlite3.connect(speech.as_uri() + '?mode=ro', uri=True, timeout=1)) as db:
            if db.execute('PRAGMA user_version').fetchone() != (1,):
                raise ValueError('unknown_speech_database_version')
            if db.execute("SELECT 1 FROM speech WHERE state IN ('evaluating','sending') LIMIT 1").fetchone():
                return False
    files = state_path.parent / 'qq-outbox' / 'ledger.json'
    if files.exists():
        ledger = read_json(files)
        if ledger['format_version'] != 1:
            raise ValueError('unknown_file_ledger_version')
        if any(row['state'] in ('Pending', 'Sending') for row in ledger['records']):
            return False
    return True

def delivery_gate(state_path, request, inherited=frozenset()):
    """确切申请轮次已完成送达且本次运行空闲；不将历史未知回执当成当前任务。"""
    sessions, receipts, settled = delivery_documents(state_path, inherited)
    if not settled:
        return False
    session = sessions['sessions'].get(request['session_id'])
    if not session or session['key']['user_id'] != request['requested_by']:
        raise ValueError('request_scope_mismatch')
    turn = None
    for candidate in session_turns(state_path, session, sessions['format_version']):
        if candidate['id'] == request['turn_id']:
            turn = candidate
    if not turn:
        raise ValueError('request_turn_missing')
    status = turn['status']['state']
    if status == 'Pending':
        return False
    if status != 'Completed':
        raise ValueError('request_turn_not_completed')
    reply = turn['status']['messages'][-1].get('text')
    matching = None
    for receipt in receipts['entries']:
        # 当前进程启动前已存在且完全未变化的未知记录：既不阻塞空闲，也不能证明本次请求已送达。
        if receipt_fingerprint(receipt) in inherited:
            continue
        msg = receipt['message']
        identity = json.dumps([receipt['app_id'], msg['scope'], msg['target_id'], msg['user_id']],
                              ensure_ascii=False, separators=(',', ':')).encode()
        scope = 'qq:' + hashlib.sha256(identity).hexdigest()
        merged_text = '\n'.join([r['message']['text'] for r in receipts['entries']
            if r.get('merged_into') == msg.get('id') and r['app_id'] == receipt['app_id']]
            + [msg['text']]) if receipts['version'] in (4, 5, 6, 7, 8, 9, 10) else msg['text']
        if receipts['version'] in (9, 10) and msg['text'] == turn['input']:
            # 轮内补充保存在同一轮 messages 中，首条输入没有被自动合成替换。
            merged_text = msg['text']
        bound_handled = (receipts['version'] in (9, 10) and receipt['state'] == 'Handled'
                         and receipt.get('control_turn_id') == request['turn_id'])
        if receipt['state'] != 'Merged' and scope == request['session_id'] and (bound_handled or merged_text == turn['input']):
            matching = receipt
        if receipt['state'] in ('Processing', 'ReplyPending'):
            return False
    handled = (matching and receipts['version'] in (9, 10) and matching['state'] == 'Handled'
               and matching.get('control_turn_id') == request['turn_id']
               and matching.get('reply') is None
               and not any(p['state'] == 'Sending' for p in matching.get('progress', [])))
    return bool(handled or matching and matching['state'] == 'Sent' and matching.get('reply') == reply) and all(t['status']['state'] != 'Pending'
                        for s in sessions['sessions'].values() for t in session_turns(state_path, s, sessions['format_version']))

def drain_stamp(state_path):
    """侧账本及 SQLite WAL 也参与门禁缓存；大状态未变化时不重复解析。"""
    paths = [state_path, state_path.parent / 'qq-outbox' / 'ledger.json']
    paths += [state_path.parent / (name + suffix)
              for name in ('group-observations.sqlite3', 'speech.sqlite3', 'self-interests.sqlite3')
              for suffix in ('', '-wal', '-journal')]
    paths += [state_path.parent / name for name in ('group-observations.required', 'speech.required', 'self-interests.required')]
    stamps = []
    for path in paths:
        try:
            stat = path.stat()
            stamps.append((stat.st_ino, stat.st_mtime_ns, stat.st_size))
        except FileNotFoundError:
            stamps.append(None)
    return tuple(stamps)

def validate_merged_receipts(receipts):
    """同分区、同身份的合并关系；格式 7 起允许后到输入并入原轮。"""
    if receipts['version'] not in (4, 5, 6, 7, 8, 9, 10):
        return
    lookup = {}
    for index, entry in enumerate(receipts['entries']):
        message = entry['message']
        identity = (entry['app_id'], message.get('id'))
        if not isinstance(identity[1], str) or not identity[1] or identity in lookup:
            raise ValueError('invalid_merged_receipt')
        lookup[identity] = (index, entry)
    for index, entry in enumerate(receipts['entries']):
        into = entry.get('merged_into')
        if entry['state'] != 'Merged':
            if into is not None:
                raise ValueError('invalid_merged_receipt')
            continue
        if not isinstance(into, str):
            raise ValueError('invalid_merged_receipt')
        target = lookup.get((entry['app_id'], into))
        if (not target or (receipts['version'] < 7 and target[0] <= index) or target[0] == index or target[1]['state'] == 'Merged'
                or any(entry['message'][key] != target[1]['message'][key]
                       for key in ('scope', 'target_id', 'user_id'))
                or entry.get('reply') is not None or entry.get('segments') is not None
                or entry.get('progress')):
            raise ValueError('invalid_merged_receipt')
    if receipts['version'] == 10:
        for entry in receipts['entries']:
            for part in entry.get('progress', []):
                anchor = part.get('reply_anchor')
                if anchor is None:
                    continue
                source = lookup.get((entry['app_id'], anchor))
                if (source is None or source[1]['state'] != 'Merged'
                        or source[1].get('merged_into') != entry['message']['id']
                        or any(source[1]['message'][key] != entry['message'][key]
                               for key in ('scope', 'target_id', 'user_id'))):
                    raise ValueError('invalid_reply_anchor_relation')

def execution_command(command):
    """Linux 便携包可选的私有动态加载器；逻辑命令和候选摘要保持原样。"""
    loader = os.environ.get('EVE_RUNTIME_LOADER')
    libraries = os.environ.get('EVE_RUNTIME_LIBRARY_PATH')
    if os.name == 'nt' or (loader is None and libraries is None):
        return command
    if (not loader or not libraries or not Path(loader).is_absolute()
            or not Path(loader).is_file()
            or any(not p or not Path(p).is_absolute() or not Path(p).is_dir()
                   for p in libraries.split(os.pathsep))):
        raise ValueError('portable_runtime_loader_invalid')
    with Path(command[0]).open('rb') as stream:
        elf = stream.read(4) == b'\x7fELF'
    return [loader, '--library-path', libraries, *command] if elf else command

def command_resources(command):
    paths = set()
    for flag in ['--agent', '--bridge-script', '--practice-mindustry-server', '--research-corpus']:
        if flag not in command:
            continue
        path = Path(argument(command, flag))
        if path.is_dir():
            paths.update(p for p in path.rglob('*') if p.is_file())
        else:
            paths.add(path)
        if flag == '--bridge-script':
            paths.update(p for p in [path.parent / name for name in
                         ['bridge-core.mjs', 'ingress-diagnostics.mjs', 'package.json', 'package-lock.json']] if p.exists())
    return paths

def snapshot_command(command, candidate_request=None):
    return dict(format_version=1, command=command, binary_sha256=digest(Path(command[0])),
                resources={str(p): digest(p) for p in sorted(command_resources(command))},
                candidate_request=candidate_request)

class Child:
    def __init__(self, command):
        self.ready = threading.Event()
        options = {'creationflags': subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == 'nt' else {}
        if sys.platform.startswith('linux'):
            # 独立进程组但继承监督器会话；内层可关闭桥接，外层仍可收拢整场会话。
            options['process_group'] = 0
        self.process = subprocess.Popen(execution_command(command), stdin=subprocess.DEVNULL, stderr=subprocess.PIPE, **options)
        def copy():
            for chunk in iter(lambda: self.process.stderr.readline(65536), b''):
                if b'EVE_QQBOT_READY' in chunk:
                    self.ready.set()
                sys.stderr.buffer.write(chunk)
                sys.stderr.buffer.flush()
        self.thread = threading.Thread(target=copy, daemon=True)
        self.thread.start()

    def close_group(self, timeout):
        if not sys.platform.startswith('linux'):
            return
        def remaining():
            result = []
            for path in Path('/proc').glob('[0-9]*'):
                try:
                    fields = (path / 'stat').read_text().rsplit(') ', 1)[1].split()
                    if fields[0] != 'Z' and int(fields[2]) == self.process.pid and int(fields[3]) == os.getsid(0):
                        result.append(int(path.name))
                except (OSError, ValueError, IndexError):
                    pass
            return result
        for sig, grace in [(signal.SIGTERM, timeout), (signal.SIGKILL, 5)]:
            if not remaining():
                return
            with contextlib.suppress(ProcessLookupError):
                os.killpg(self.process.pid, sig)
            deadline = time.monotonic() + grace
            while remaining() and time.monotonic() < deadline:
                time.sleep(0.05)
        if remaining():
            raise ValueError('runtime_process_group_not_closed')

    def stop(self, timeout):
        if self.process.poll() is None:
            self.process.send_signal(signal.CTRL_BREAK_EVENT if os.name == 'nt' else signal.SIGINT)
            self.process.wait(timeout=timeout)
        self.close_group(timeout)
        self.thread.join(timeout=1)
        if not self.thread.is_alive():
            self.process.stderr.close()
        return self.process.returncode

class Supervisor:
    def __init__(self, options):
        self.options = options
        self.root = options.directory.resolve()
        self.root.mkdir(parents=True, exist_ok=True)
        self.lock = (self.root / 'upgrade.lock').open('a+b')
        self.lock.seek(0)
        if os.name == 'nt':
            import msvcrt
            if self.lock.read(1) == b'': self.lock.write(b'0'); self.lock.flush()
            self.lock.seek(0); msvcrt.locking(self.lock.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.command = options.command
        if self.command[:1] == ['--']: self.command = self.command[1:]
        self.host_bridge = getattr(options, 'host_bridge', None)
        self.host_nonce = getattr(options, 'host_nonce', None)
        launch_path = getattr(options, 'launch_descriptor', None)
        launch = None
        if any(value is not None for value in (self.host_bridge, self.host_nonce, launch_path)):
            if (self.host_bridge is None or self.host_bridge.resolve() != self.root
                    or not sha(self.host_nonce) or launch_path is None
                    or launch_path.parent.resolve() != self.root):
                raise ValueError('invalid_host_launch')
            launch = read_json(launch_path)
            if (not isinstance(launch, dict)
                    or set(launch) != {'format_version', 'command', 'candidate_request', 'source_revision'}
                    or launch['format_version'] != 1 or not isinstance(launch['command'], list)
                    or not launch['command'] or any(not isinstance(part, str) or '\0' in part for part in launch['command'])):
                raise ValueError('invalid_host_launch_descriptor')
            self.command = launch['command']
        if '--database-config' in self.command:
            raise ValueError('file_state_supervisor_requires_file_backend')
        self.state = Path(argument(self.command, '--state-dir')) / 'state.json'
        if not self.state.is_absolute(): raise ValueError('state_directory_must_be_absolute')
        state_root = self.state.parent.resolve()
        if any(state_root == p.resolve() or state_root in p.resolve().parents
               or p.resolve() in state_root.parents for p in (self.root, options.candidates)):
            raise ValueError('state_directory_overlaps_upgrade')
        self.inherited_receipts = inherited_receipts(self.state)
        if self.inherited_receipts:
            print(f'EVE_UPGRADE_RECOVERY inherited_unknown_receipts={len(self.inherited_receipts)} preserved_no_replay', file=sys.stderr, flush=True)
        self.stopping = False
        self.handoff = False
        self.host_confirmed = False
        self.child = None
        self.status_lock = threading.RLock()
        self.status = dict(format_version=VERSION, pid=os.getpid(), heartbeat_ms=0, phase='starting',
                           active_sha256=digest(Path(self.command[0])), candidate_sha256=None, result=None, error=None,
                           restart_supported=True, host_nonce=self.host_nonce)
        for name in ([] if launch is not None else ['request.json', 'processing.json', 'auto-request.json']):
            if (self.root / name).exists():
                self.archive_request(self.root / name, 'interrupted_no_replay')
                self.status['result'] = 'interrupted_no_replay'
        pointer = self.root / 'active-command.json'
        active_request = launch['candidate_request'] if launch is not None else None
        if launch is not None and active_request is not None:
            binary, resources, _ = validate_candidate(active_request, self.options.candidates)
            if self.command[0] != str(binary) or argument(self.command, '--agent') != str(resources / 'AGENT.md'):
                raise ValueError('host_launch_candidate_mismatch')
        if launch is None and pointer.exists():
            stored = read_json(pointer)
            if set(stored) != {'command', 'binary_sha256', 'candidate_request'} or digest(Path(stored['command'][0])) != stored['binary_sha256']:
                raise ValueError('active_command_changed')
            binary, resources, _ = validate_candidate(stored['candidate_request'], self.options.candidates)
            if (self.root / 'quarantine' / (stored['candidate_request']['candidate_sha256'] + '.json')).exists():
                raise ValueError('active_candidate_quarantined')
            if stored['command'][0] != str(binary) or argument(stored['command'], '--agent') != str(resources / 'AGENT.md'):
                raise ValueError('active_command_resources_changed')
            if argument(stored['command'], '--state-dir') != argument(self.command, '--state-dir'):
                raise ValueError('state_directory_changed')
            self.command = stored['command']
            active_request = stored['candidate_request']
            self.status['active_sha256'] = stored['binary_sha256']
        self.active_descriptor = snapshot_command(self.command, active_request)
        self.status['active_source_revision'] = (
            source_revision(validate_candidate(active_request, self.options.candidates)[2])
            if active_request is not None else (launch['source_revision'] if launch is not None else getattr(options, 'source_revision', None)))
        self.status['active_candidate_sha256'] = active_request['candidate_sha256'] if active_request else None
        recovery = self.root / 'recovery.json'
        self.recovery = read_json(recovery) if recovery.exists() else dict(format_version=1, crashes=[], result=None)
        if (set(self.recovery) != {'format_version', 'crashes', 'result'}
                or self.recovery['format_version'] != 1 or not isinstance(self.recovery['crashes'], list)
                or any(not isinstance(c, dict) or set(c) != {'at_ms', 'exit_code', 'binary_sha256'}
                       or type(c['at_ms']) is not int or type(c['exit_code']) is not int
                       or not sha(c['binary_sha256']) for c in self.recovery['crashes'])):
            raise ValueError('invalid_recovery_history')
        self.rollback_used = False
        self.finished = threading.Event()
        def heartbeat():
            while not self.finished.is_set():
                self.publish()
                self.finished.wait(1)
        self.heart = threading.Thread(target=heartbeat, daemon=True)
        self.updater = None
        if getattr(options, 'auto_update_config', None):
            from source_update import SourceUpdater
            self.updater = SourceUpdater(options.auto_update_config, self.root, options.candidates)
        self.prune_backups()


    def publish(self, **changes):
        with self.status_lock:
            self.status.update(changes, heartbeat_ms=int(time.time() * 1000))
            atomic(self.root / 'supervisor.json', self.status)

    def host_activation_confirmed(self):
        """外层稳定窗口与指针提交完成后，才允许再调度升级或重启。"""
        if self.host_bridge is None:
            return True
        if not (self.root / 'host-status.json').exists():
            return False
        try:
            status = read_json(self.root / 'host-status.json')
        except FileNotFoundError:
            return False
        return (isinstance(status, dict) and type(status.get('format_version')) is int
                and status['format_version'] == VERSION and status.get('phase') == 'running'
                and status.get('child_pid') == os.getpid() and status.get('host_nonce') == self.host_nonce
                and status.get('candidate_sha256') == self.status['active_candidate_sha256'])

    def confirm_host_result(self):
        """只把宿主已提交并与本进程绑定的结果提供给状态查询。"""
        if self.host_bridge is None or self.host_confirmed:
            return
        path = self.root / 'host-handoff.json'
        if path.exists():
            receipt = read_json(path)
            if (not isinstance(receipt, dict)
                    or set(receipt) != {'format_version', 'phase', 'previous', 'next', 'error', 'updated_at_ms'}
                    or type(receipt['format_version']) is not int or receipt['format_version'] != VERSION
                    or receipt['phase'] not in ('committed', 'rolled_back')
                    or type(receipt['updated_at_ms']) is not int):
                raise ValueError('invalid_host_handoff_receipt')
            applied = receipt['phase'] == 'committed'
            descriptor = receipt['next' if applied else 'previous']['descriptor']
            request = receipt['next']['descriptor']['candidate_request']
            if (descriptor['command'] != self.command
                    or descriptor['candidate_request'] != self.active_descriptor['candidate_request']
                    or descriptor['source_revision'] != self.status['active_source_revision']
                    or not isinstance(request, dict) or not sha(request.get('candidate_sha256'))):
                raise ValueError('host_handoff_receipt_mismatch')
            self.publish(result='applied' if applied else 'rolled_back_runtime_state_preserved',
                         candidate_sha256=request['candidate_sha256'], error=receipt['error'])
        self.host_confirmed = True

    def archive_request(self, path, result):
        history = self.root / 'history'
        history.mkdir(exist_ok=True)
        path.rename(history / f'{time.time_ns()}-{result}.json')

    def failure_evidence(self, error, request=None):
        """仅保存错误码、摘要与阶段；不复制会话、密钥、完整日志或状态。"""
        code = str(error) if type(error) is ValueError and str(error).replace('_', '').isalnum() else type(error).__name__
        atomic(self.root / 'last-failure.json', dict(format_version=1, at_ms=int(time.time() * 1000),
               phase=self.status['phase'], error=code[:100], active_sha256=self.status['active_sha256'],
               candidate_sha256=(request or {}).get('candidate_sha256')))

    def quarantine_candidate(self, request, error):
        directory = self.root / 'quarantine'
        directory.mkdir(exist_ok=True)
        code = str(error) if type(error) is ValueError and str(error).replace('_', '').isalnum() else type(error).__name__
        path = directory / (request['candidate_sha256'] + '.json')
        if not path.exists():
            atomic(path, dict(format_version=1, candidate_sha256=request['candidate_sha256'],
                             failed_at_ms=int(time.time() * 1000), error=code[:100]))

    def prune_backups(self):
        # 快照不用于自动还原；只保留最近两份现场，避免每次切换复制永久增长。
        directory = self.root / 'backups'
        if directory.exists():
            backups = sorted((p for p in directory.iterdir() if p.is_dir() and not p.is_symlink()
                              and p.name.isdecimal()), key=lambda p: int(p.name), reverse=True)
            for path in backups[2:]:
                shutil.rmtree(path)

    def wait_ready(self, child):
        deadline = time.monotonic() + self.options.ready_timeout
        while not child.ready.wait(0.1):
            if self.stopping or child.process.poll() is not None or time.monotonic() >= deadline:
                raise ValueError('candidate_not_ready')
        stable = time.monotonic() + self.options.health_seconds
        while time.monotonic() < stable:
            if self.stopping or child.process.poll() is not None:
                raise ValueError('candidate_not_stable')
            time.sleep(0.05)
        # 调度暂停可能跨过整个稳定窗口；提交指针前仍须核对进程尚存。
        if self.stopping or child.process.poll() is not None:
            raise ValueError('candidate_not_stable')

    def candidate_command(self, request):
        if (self.root / 'quarantine' / (request['candidate_sha256'] + '.json')).exists():
            raise ValueError('candidate_quarantined')
        binary, resources, manifest = validate_candidate(request, self.options.candidates)
        validate_lease(request, binary.parent, manifest)
        if manifest.get('runtime') is not None and self.host_bridge is None:
            raise ValueError('runtime_handoff_requires_host_bridge')
        subprocess.run(execution_command([str(binary), '--help']), check=True, timeout=30, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        frozen_bridge = resources / 'connectors/qqbot'
        # Node 依赖属于运行缓存，不能 npm ci 改写内容寻址的候选目录。
        bridge = self.root / 'runtime-resources' / request['candidate_sha256'] / 'qqbot'
        if not bridge.exists():
            bridge.parent.mkdir(parents=True, exist_ok=True)
            with tempfile.TemporaryDirectory(dir=bridge.parent) as temporary:
                staged = Path(temporary) / 'qqbot'
                staged.mkdir()
                for name in manifest['resources']:
                    if name.startswith('connectors/qqbot/'):
                        relative = name.removeprefix('connectors/qqbot/')
                        target = staged / relative
                        target.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copy2(frozen_bridge / relative, target)
                staged.rename(bridge)
        for name, expected in manifest['resources'].items():
            if name.startswith('connectors/qqbot/'):
                if digest(bridge / name.removeprefix('connectors/qqbot/')) != expected:
                    raise ValueError('runtime_resource_changed')
        modules = bridge / 'node_modules'
        original_bridge = Path(argument(self.command, '--bridge-script'))
        bundled_bridge = original_bridge.name == 'bridge.mjs' and original_bridge.parent.name == 'qqbot'
        ready_marker = bridge.parent / 'dependencies-ready.json'
        binding = dict(format_version=1, package_lock_sha256=digest(bridge / 'package-lock.json'))
        complete = ready_marker.exists() and read_json(ready_marker) == binding
        if bundled_bridge and modules.is_symlink(): raise ValueError('runtime_dependency_symlink')
        if bundled_bridge and (not modules.exists() or not complete):
            # npm ci/复制中断留下的目录不能冒充完整安装；仅重建可再生运行缓存。
            if modules.exists(): shutil.rmtree(modules)
            old_bridge = original_bridge.parent
            if (old_bridge / 'node_modules').exists() and digest(bridge / 'package-lock.json') == digest(old_bridge / 'package-lock.json'):
                shutil.copytree(old_bridge / 'node_modules', modules, symlinks=True)
            else:
                subprocess.run(['npm', 'ci', '--ignore-scripts', '--no-audit', '--no-fund'], cwd=bridge,
                               check=True, timeout=60, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            atomic(ready_marker, binding)
        result = [str(binary), *without_removed_flags(self.command[1:])]
        replacements = [('--agent', resources / 'AGENT.md')]
        if bundled_bridge: replacements.append(('--bridge-script', bridge / 'bridge.mjs'))
        for flag, value in replacements:
            result = replace_argument(result, flag, value)
        if '--research-corpus' in result:
            result = replace_argument(result, '--research-corpus', resources / 'vendor/eve-portable-knowledge')
        for index, value in enumerate(result[:-1]):
            if value == '--reference' and result[index+1].startswith('eve-portable='):
                result[index+1] = 'eve-portable=' + str(resources / 'vendor/eve-portable-knowledge')
        binary, _, checked = validate_candidate(request, self.options.candidates)
        validate_lease(request, binary.parent, checked)
        return result, manifest

    def switch(self, request, command, manifest, request_path=None):
        if self.stopping: raise ValueError('shutdown_requested')
        # 再次核对磁盘上的请求、资源与执行文件，不能切换一份预检之后发生变化的候选。
        request_path = Path(request_path or self.root / 'request.json')
        # Windows TEMP 的 8.3 短名与 resolve 后的长名必须指向同一请求类型。
        # 只规范化父目录，保留请求文件本身供 read_json 拒绝符号链接。
        request_path = request_path.parent.resolve() / request_path.name
        if request_path not in (self.root / 'request.json', self.root / 'auto-request.json'):
            raise ValueError('request_path_outside_supervisor')
        if request_path == self.root / 'request.json' and request.get('action') == 'restart':
            if (set(request) != {'format_version', 'action', 'session_id', 'requested_by', 'turn_id'}
                    or request['format_version'] != VERSION or type(request['turn_id']) is not int
                    or request['turn_id'] <= 0 or not all(isinstance(request[k], str) and request[k]
                        for k in ('session_id', 'requested_by'))):
                raise ValueError('invalid_restart_request')
            if read_json(request_path) != request:
                raise ValueError('request_changed')
            if not delivery_gate(self.state, request, self.inherited_receipts):
                raise ValueError('reply_not_delivered')
            if not runtime_idle_gate(self.state, self.inherited_receipts):
                raise ValueError('runtime_not_idle')
            self.validate_descriptor(self.active_descriptor)
            self.publish(phase='restarting', result=None, error=None)
            processing = self.root / 'processing.json'
            request_path.rename(processing)
            if self.child.stop(self.options.stop_timeout) != 0:
                raise ValueError('old_process_did_not_close_cleanly')
            if not runtime_idle_gate(self.state, self.inherited_receipts):
                raise ValueError('runtime_drain_incomplete')
            self.child = Child(self.command)
            try:
                self.wait_ready(self.child)
            except (ValueError, OSError, subprocess.SubprocessError) as error:
                self.failure_evidence(error)
                self.child.stop(self.options.stop_timeout)
                if self.stopping:
                    raise ValueError('shutdown_during_restart')
                self.child = Child(self.command)
                self.wait_ready(self.child)
                self.publish(phase='running', result='restart_failed_recovered', error='restart_not_ready')
            else:
                self.publish(phase='running', result='restarted', error=None)
            self.archive_request(processing, self.status['result'])
            return
        if read_json(request_path) != request or manifest['candidate_sha256'] != request['candidate_sha256']:
            raise ValueError('request_changed')
        binary, _, checked = validate_candidate(request, self.options.candidates)
        validate_lease(request, binary.parent, checked)
        # 文件投递和群消息数据库可能在 state.json 未变时进入繁忙；不能只缓存主文件戳。
        if not runtime_idle_gate(self.state, self.inherited_receipts):
            raise ValueError('runtime_not_idle')
        if request_path != self.root / 'auto-request.json' and not delivery_gate(self.state, request, self.inherited_receipts):
            raise ValueError('reply_not_delivered')
        active = self.active_descriptor['candidate_request']
        if active is not None and active['candidate_sha256'] == request['candidate_sha256']:
            self.validate_descriptor(self.active_descriptor)
            self.publish(phase='running', candidate_sha256=request['candidate_sha256'],
                         result='applied', error=None)
            self.archive_request(request_path, 'already_active')
            return
        if self.host_bridge is not None:
            if checked.get('runtime') != RUNTIME_CONTRACT:
                raise ValueError('runtime_contract_required')
            if checked['source_diff']['base_revision'] != self.status['active_source_revision']:
                raise ValueError('source_diff_baseline_changed')
            self.validate_descriptor(self.active_descriptor)
            if self.updater:
                self.updater.stop()
                if self.updater.thread and self.updater.thread.is_alive():
                    raise ValueError('update_worker_did_not_close')
            processing = self.root / 'processing.json'
            request_path.rename(processing)
            self.publish(phase='handing_off', result=None, error=None)
            if self.child.stop(self.options.stop_timeout) != 0:
                raise ValueError('old_process_did_not_close_cleanly')
            if not runtime_idle_gate(self.state, self.inherited_receipts):
                raise ValueError('runtime_drain_incomplete')
            atomic(self.root / 'handoff-request.json', dict(format_version=1, nonce=self.host_nonce,
                   base_candidate_sha256=active['candidate_sha256'] if active else None,
                   request=request, command=command))
            self.handoff = True
            self.stopping = True
            self.publish(phase='handed_off', result='handed_off')
            return
        state_bytes = sum(p.stat().st_size for p in self.state.parent.rglob('*')
                          if p.is_file() and not p.is_symlink())
        if shutil.disk_usage(self.root).free < state_bytes * 2 + 256 * 1024**2:
            raise ValueError('insufficient_space_for_backup')
        self.validate_descriptor(self.active_descriptor)
        previous_path = self.root / 'previous-command.json'
        previous = read_json(previous_path) if previous_path.exists() else None
        if previous is not None:
            self.validate_descriptor(previous)
        atomic(previous_path, self.active_descriptor)
        pending = request_path
        processing = self.root / 'processing.json'
        pending.rename(processing)
        self.publish(phase='switching', candidate_sha256=request['candidate_sha256'], result=None, error=None)
        old_command = self.command
        old_descriptor = self.active_descriptor
        if self.child.stop(self.options.stop_timeout) != 0:
            raise ValueError('old_process_did_not_close_cleanly')
        if not runtime_idle_gate(self.state, self.inherited_receipts):
            raise ValueError('runtime_drain_incomplete')
        try:
            backup = self.root / 'backups' / str(time.time_ns())
            shutil.copytree(self.state.parent, backup)
            self.child = Child(command)
            self.wait_ready(self.child)
            descriptor = snapshot_command(command, request)
            # 状态指针只在候选真正连通后提交；不恢复旧状态快照。
            atomic(self.root / 'active-command.json', dict(command=command, binary_sha256=manifest['binary_sha256'], candidate_request=request))
            self.command = command
            self.active_descriptor = descriptor
            self.rollback_used = False
            self.publish(phase='running', active_sha256=manifest['binary_sha256'], result='applied',
                         active_source_revision=source_revision(manifest), active_candidate_sha256=request['candidate_sha256'])
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            self.failure_evidence(error, request)
            self.child.stop(self.options.stop_timeout)
            if self.stopping: raise ValueError('shutdown_during_upgrade')
            self.quarantine_candidate(request, error)
            # 任何提交后异常都必须先恢复磁盘与内存的同一版本，再启动旧程序。
            # 恢复指针失败时停止并保留现场，不能留下“运行旧版、重启新版”的分裂状态。
            pointer = self.root / 'active-command.json'
            if old_descriptor['candidate_request'] is None:
                pointer.unlink(missing_ok=True)
            else:
                atomic(pointer, {k: old_descriptor[k] for k in ('command', 'binary_sha256', 'candidate_request')})
            self.command = old_command
            self.active_descriptor = old_descriptor
            if previous is None:
                previous_path.unlink(missing_ok=True)
            else:
                atomic(previous_path, previous)
            self.validate_descriptor(self.active_descriptor)
            self.child = Child(old_command)
            self.wait_ready(self.child)
            self.publish(phase='running', active_sha256=old_descriptor['binary_sha256'],
                         active_source_revision=(source_revision(validate_candidate(old_descriptor['candidate_request'], self.options.candidates)[2])
                             if old_descriptor['candidate_request'] else getattr(self.options, 'source_revision', None)),
                         active_candidate_sha256=old_descriptor['candidate_request']['candidate_sha256'] if old_descriptor['candidate_request'] else None,
                         result='rolled_back_binary_state_preserved')
        self.archive_request(processing, self.status['result'])
        self.prune_backups()

    def validate_descriptor(self, descriptor):
        if (not isinstance(descriptor, dict)
                or set(descriptor) != {'format_version', 'command', 'binary_sha256', 'resources', 'candidate_request'}
                or descriptor['format_version'] != 1
                or not isinstance(descriptor['command'], list) or not descriptor['command']
                or len(descriptor['command']) > 512
                or any(not isinstance(s, str) or '\0' in s for s in descriptor['command'])):
            raise ValueError('invalid_previous_command')
        command = descriptor['command']
        if (not Path(command[0]).is_absolute()
                or Path(argument(command, '--state-dir')) != self.state.parent
                or snapshot_command(command, descriptor['candidate_request']) != descriptor):
            raise ValueError('previous_command_changed')
        if descriptor['candidate_request'] is not None:
            binary, resources, _ = validate_candidate(descriptor['candidate_request'], self.options.candidates)
            if str(binary) != command[0] or argument(command, '--agent') != str(resources / 'AGENT.md'):
                raise ValueError('previous_candidate_changed')

    def recover(self):
        """有限恢复；仅重启程序，绝不还原状态、重放旧请求或重试未知投递。"""
        if self.stopping:
            return
        if self.child.process.poll() is None:
            raise ValueError('old_process_still_running')
        if self.host_bridge is not None:
            # 脚本与业务版本由外层作为一个整体恢复，不在旧脚本内单独回退业务程序。
            raise ValueError('managed_runtime_process_exited')
        self.child.stop(self.options.stop_timeout)
        now = int(time.time() * 1000)
        crashes = [c for c in self.recovery['crashes']
                   if now - c['at_ms'] <= self.options.restart_window_seconds * 1000]
        crashes.append(dict(at_ms=now, exit_code=self.child.process.returncode,
                            binary_sha256=self.status['active_sha256']))
        self.recovery.update(crashes=crashes, result='recovering')
        atomic(self.root / 'recovery.json', self.recovery)
        if len(crashes) > self.options.restart_limit:
            self.recovery['result'] = 'restart_limit_exhausted'
            atomic(self.root / 'recovery.json', self.recovery)
            raise ValueError('restart_limit_exhausted')
        if not self.state.is_file():
            raise ValueError('runtime_state_missing')
        # 必须在旧进程真正退出后重新捕获，保留中断轮次与未知回执的真实字节。
        self.inherited_receipts = inherited_receipts(self.state)
        pending = self.root / 'request.json'
        if pending.exists():
            self.archive_request(pending, 'runtime_interrupted_no_replay')
        previous = self.root / 'previous-command.json'
        candidate = self.active_descriptor['candidate_request']
        result = 'runtime_restarted_state_preserved'
        if candidate is not None and previous.exists() and not self.rollback_used:
            descriptor = read_json(previous)
            self.validate_descriptor(descriptor)
            if descriptor != self.active_descriptor:
                quarantine = self.root / 'quarantine'
                quarantine.mkdir(exist_ok=True)
                atomic(quarantine / (candidate['candidate_sha256'] + '.json'),
                       dict(format_version=1, candidate_sha256=candidate['candidate_sha256'],
                            failed_at_ms=now, exit_code=self.child.process.returncode))
                pointer = self.root / 'active-command.json'
                if pointer.exists():
                    self.archive_request(pointer, 'runtime_failed_active_command')
                if descriptor['candidate_request'] is not None:
                    atomic(pointer, {k: descriptor[k] for k in ['command', 'binary_sha256', 'candidate_request']})
                self.active_descriptor = descriptor
                self.command = descriptor['command']
                self.rollback_used = True
                result = 'runtime_rolled_back_binary_state_preserved'
        self.validate_descriptor(self.active_descriptor)
        self.publish(phase='recovering', result=result, error='active_process_exited',
                     active_sha256=self.active_descriptor['binary_sha256'],
                     active_source_revision=(source_revision(validate_candidate(self.active_descriptor['candidate_request'], self.options.candidates)[2])
                         if self.active_descriptor['candidate_request'] else getattr(self.options, 'source_revision', None)),
                     active_candidate_sha256=self.active_descriptor['candidate_request']['candidate_sha256'] if self.active_descriptor['candidate_request'] else None)
        until = time.monotonic() + min(60, self.options.restart_delay * 2 ** (len(crashes) - 1))
        while not self.stopping and time.monotonic() < until:
            time.sleep(0.05)
        if self.stopping:
            return
        self.child = Child(self.command)
        try:
            self.wait_ready(self.child)
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            self.failure_evidence(error)
            self.child.stop(self.options.stop_timeout)
            if not self.stopping:
                # 每个未就绪的真实进程同样消耗恢复预算，不能一次失败就绕过 restart_limit。
                return self.recover()
            return
        self.recovery['result'] = result
        atomic(self.root / 'recovery.json', self.recovery)
        self.publish(phase='running', result=result, error=None)

    def run(self):
        def stop(_signum, _frame): self.stopping = True
        signals = [signal.SIGINT, signal.SIGTERM]
        if os.name == 'nt': signals.append(signal.SIGBREAK)
        for sig in signals: signal.signal(sig, stop)
        self.heart.start()
        try:
            self.child = Child(self.command)
            try:
                self.wait_ready(self.child)
            except (ValueError, OSError, subprocess.SubprocessError) as error:
                self.failure_evidence(error)
                self.child.stop(self.options.stop_timeout)
                if not self.stopping:
                    self.recover()
            self.publish(phase='running')
            waiting_since = None
            quiet_since = None
            prepared = None
            state_stamp = None
            gate = False
            last_gate_check = 0.0
            while not self.stopping:
                if self.child.process.poll() is not None:
                    self.recover()
                    prepared = waiting_since = quiet_since = state_stamp = None
                    gate = False
                    continue
                if not self.host_activation_confirmed():
                    time.sleep(0.1)
                    continue
                self.confirm_host_result()
                if self.updater and not (self.root / 'request.json').exists() and prepared is None:
                    self.updater.tick(self)
                pending = self.root / 'request.json'
                if pending.exists():
                    request = None
                    try:
                        request = read_json(pending)
                        if not isinstance(request, dict) or not sha(request.get('candidate_sha256')):
                            if not isinstance(request, dict) or request.get('action') != 'restart':
                                raise ValueError('invalid_request')
                        if prepared is not None and prepared[0] != request:
                            raise ValueError('request_changed')
                        if prepared is None:
                            if request.get('action') == 'restart':
                                if set(request) != {'format_version', 'action', 'session_id', 'requested_by', 'turn_id'}:
                                    raise ValueError('invalid_restart_request')
                                prepared = (request, self.command, None)
                                waiting_since = time.monotonic()
                                self.publish(phase='waiting', candidate_sha256=None, error=None)
                                continue
                            self.publish(phase='preparing', candidate_sha256=request.get('candidate_sha256'), result=None, error=None)
                            prepared = (request, *self.candidate_command(request))
                            waiting_since = time.monotonic()
                            self.publish(phase='waiting')
                        if time.monotonic() - waiting_since > self.options.drain_timeout:
                            raise ValueError('reply_or_idle_timeout')
                        stamp = drain_stamp(self.state)
                        # 文件戳只是缓存提示；粗粒度时间戳或同长度 WAL 更新不能永久卡住门禁。
                        if state_stamp != stamp or time.monotonic() - last_gate_check >= 1.0:
                            if state_stamp != stamp:
                                quiet_since = None
                            gate = (delivery_gate(self.state, request, self.inherited_receipts)
                                    and runtime_idle_gate(self.state, self.inherited_receipts))
                            state_stamp = stamp
                            last_gate_check = time.monotonic()
                        if gate:
                            quiet_since = quiet_since or time.monotonic()
                            if time.monotonic() - quiet_since >= self.options.quiet_seconds:
                                if prepared[0] != request: raise ValueError('request_changed')
                                if not (delivery_gate(self.state, request, self.inherited_receipts)
                                        and runtime_idle_gate(self.state, self.inherited_receipts)):
                                    quiet_since = None
                                    continue
                                self.switch(*prepared)
                                prepared = waiting_since = quiet_since = None
                                state_stamp = None
                        else:
                            quiet_since = None
                    except (ValueError, OSError, KeyError, TypeError, AttributeError, subprocess.SubprocessError) as error:
                        self.failure_evidence(error, request if isinstance(request, dict) else None)
                        # 拒绝候选或不能确认停机时，不宣称回退/应用成功。
                        if pending.exists(): self.archive_request(pending, 'rejected')
                        if (self.root / 'processing.json').exists():
                            self.publish(phase='blocked', result='interrupted_state_preserved')
                            raise
                        code = str(error) if type(error) is ValueError and str(error).replace('_','').isalnum() else type(error).__name__
                        self.publish(phase='running', result='rejected', error=code[:100])
                        prepared = waiting_since = quiet_since = None
                        state_stamp = None
                time.sleep(0.1)
            return HANDOFF_EXIT if self.handoff else self.child.stop(self.options.stop_timeout)
        except Exception as error:
            code = str(error) if type(error) is ValueError and str(error).replace('_', '').isalnum() else type(error).__name__
            with contextlib.suppress(Exception):
                self.failure_evidence(error)
                self.recovery['result'] = code[:100]
                atomic(self.root / 'recovery.json', self.recovery)
                self.publish(result='stopped_state_preserved', error=code[:100])
            raise
        finally:
            self.finished.set()
            if self.updater:
                self.updater.stop()
            if self.child and self.child.process.poll() is None:
                with contextlib.suppress(subprocess.TimeoutExpired): self.child.stop(self.options.stop_timeout)
                if self.child.process.poll() is None:
                    self.child.process.kill()
                    self.child.process.wait()
            if self.child:
                self.child.close_group(self.options.stop_timeout)
            self.publish(phase='stopped')
            self.lock.close()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('--candidates', type=Path, required=True)
    parser.add_argument('--auto-update-config', type=Path)
    parser.add_argument('--source-revision')
    parser.add_argument('--host-bridge', type=Path)
    parser.add_argument('--host-nonce')
    parser.add_argument('--launch-descriptor', type=Path)
    parser.add_argument('--ready-timeout', type=float, default=60)
    parser.add_argument('--stop-timeout', type=float, default=90)
    parser.add_argument('--drain-timeout', type=float, default=300)
    parser.add_argument('--quiet-seconds', type=float, default=1)
    parser.add_argument('--health-seconds', type=float, default=2)
    parser.add_argument('--restart-limit', type=int, default=3)
    parser.add_argument('--restart-window-seconds', type=float, default=600)
    parser.add_argument('--restart-delay', type=float, default=1)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    options = parser.parse_args()
    if options.source_revision is not None and (len(options.source_revision) != 40
            or any(c not in '0123456789abcdef' for c in options.source_revision)):
        parser.error('源码基线必须是宿主核对的40位提交摘要')
    if any(not 0 < v <= 600 for v in [options.ready_timeout, options.stop_timeout, options.drain_timeout, options.quiet_seconds, options.health_seconds]):
        parser.error('期限必须大于 0 且不超过 600 秒')
    if not 0 <= options.restart_limit <= 10 or not 0 < options.restart_window_seconds <= 86400 or not 0 < options.restart_delay <= 60:
        parser.error('恢复次数为 0 至 10，窗口为 0 至 86400 秒，退避为 0 至 60 秒；期限必须大于零')
    try:
        return Supervisor(options).run()
    except Exception as error:
        # 不输出子进程参数、原文、状态正文或凭据。
        print('EVE_SELF_UPGRADE_STOPPED ' + type(error).__name__, file=sys.stderr, flush=True)
        if type(error) is ValueError and str(error) == 'managed_runtime_process_exited':
            return MANAGED_FAILURE_EXIT
        return RECOVERY_STOP

if __name__ == '__main__':
    raise SystemExit(main())
