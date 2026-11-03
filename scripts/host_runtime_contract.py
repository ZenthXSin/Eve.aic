"""固定宿主运行时协议：只解析数据、核对字节，不导入候选代码。"""
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import tempfile
import time

RUNTIME_SCRIPTS = ['scripts/build_resources.py', 'scripts/self_upgrade_runtime.py', 'scripts/source_update.py']
RUNTIME = {'host_protocol': 1, 'scripts': RUNTIME_SCRIPTS}
MAX_JSON = 1024 * 1024
MAX_DIFF = 128 * 1024
RECOVERY_STOP = 78
HANDOFF = 75
RUNTIME_FAILED = 76


def value_digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def sha(value):
    return isinstance(value, str) and re.fullmatch('[a-f0-9]{64}', value) is not None


def revision(value):
    return isinstance(value, str) and re.fullmatch('[a-f0-9]{40}', value) is not None


def regular(path):
    metadata = Path(path).lstat()
    if not stat.S_ISREG(metadata.st_mode):
        raise ValueError('host_nonregular_file')
    return metadata


def read_bytes(path, limit):
    path = Path(path)
    before = regular(path)
    if before.st_size > limit:
        raise ValueError('host_file_too_large')
    flags = os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0)
    with os.fdopen(os.open(path, flags), 'rb') as stream:
        current = os.fstat(stream.fileno())
        if not stat.S_ISREG(current.st_mode) or (before.st_dev, before.st_ino) != (current.st_dev, current.st_ino):
            raise ValueError('host_file_changed')
        data = stream.read(limit + 1)
        if len(data) > limit or len(data) != before.st_size:
            raise ValueError('host_file_changed')
    return data


def digest(path, limit=2 * 1024**3):
    path = Path(path)
    before = regular(path)
    if before.st_size > limit:
        raise ValueError('host_file_too_large')
    result, count = hashlib.sha256(), 0
    with os.fdopen(os.open(path, os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0)), 'rb') as stream:
        current = os.fstat(stream.fileno())
        if not stat.S_ISREG(current.st_mode) or (before.st_dev, before.st_ino) != (current.st_dev, current.st_ino):
            raise ValueError('host_file_changed')
        while True:
            data = stream.read(min(65536, before.st_size + 1 - count))
            if not data:
                break
            count += len(data)
            if count > before.st_size:
                raise ValueError('host_file_changed')
            result.update(data)
    if count != before.st_size:
        raise ValueError('host_file_changed')
    return result.hexdigest()


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('host_duplicate_json_field')
        result[key] = value
    return result


def read_json(path, limit=MAX_JSON):
    return json.loads(read_bytes(path, limit), object_pairs_hook=unique_object)


def atomic(path, value):
    path = Path(path)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode='w', encoding='utf8', dir=path.parent, delete=False) as stream:
            temporary = Path(stream.name)
            json.dump(value, stream, ensure_ascii=False, sort_keys=True)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        temporary = None
        sync_directory(path.parent)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0))
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def directory(path):
    if not stat.S_ISDIR(Path(path).lstat().st_mode):
        raise ValueError('host_nonregular_directory')


def argument(command, name):
    if command.count(name) != 1:
        raise ValueError('host_invalid_command_argument')
    index = command.index(name)
    if index + 1 >= len(command):
        raise ValueError('host_invalid_command_argument')
    return command[index + 1]


def validate_candidate(request, candidates, require_runtime=False):
    fields = {'format_version', 'candidate_sha256', 'candidate_directory', 'session_id', 'requested_by', 'turn_id'}
    if (not isinstance(request, dict) or set(request) not in (fields, fields | {'lease'})
            or type(request.get('format_version')) is not int or request['format_version'] != 1
            or not sha(request.get('candidate_sha256')) or type(request.get('turn_id')) is not int or request['turn_id'] <= 0
            or any(not isinstance(request.get(k), str) or not request[k] for k in ('session_id', 'requested_by'))):
        raise ValueError('host_invalid_candidate_request')
    root = Path(candidates).resolve()
    candidate = Path(request['candidate_directory'])
    if not candidate.is_absolute() or candidate.is_symlink() or candidate.resolve() != root / request['candidate_sha256']:
        raise ValueError('host_candidate_outside_directory')
    directory(candidate)
    manifest = read_json(candidate / 'manifest.json')
    if (not isinstance(manifest, dict) or type(manifest.get('format_version')) is not int or manifest['format_version'] != 1
            or manifest.get('prepared') is not True or manifest.get('success') is not True
            or manifest.get('candidate_sha256') != request['candidate_sha256']
            or not isinstance(manifest.get('checks'), list) or len(manifest['checks']) != 4
            or any(not isinstance(c, dict) or c.get('success') is not True for c in manifest['checks'])):
        raise ValueError('host_candidate_not_verified')
    binary = candidate / 'eve-qqbot'
    declared = Path(manifest.get('candidate_path', ''))
    if not declared.is_absolute() or declared.is_symlink() or (declared.name != binary.name or declared.parent.resolve() != binary.parent.resolve()) or digest(binary) != manifest.get('binary_sha256'):
        raise ValueError('host_candidate_binary_changed')
    source = manifest.get('source')
    if (not isinstance(source, dict) or set(source) != {'head', 'diff', 'untracked'}
            or not revision(source.get('head')) or not sha(source.get('diff')) or not sha(source.get('untracked'))):
        raise ValueError('host_invalid_source_identity')
    resources = manifest.get('resources')
    required = {'AGENT.md', 'connectors/qqbot/bridge.mjs', 'connectors/qqbot/bridge-core.mjs',
                'connectors/qqbot/package.json', 'connectors/qqbot/package-lock.json'}
    if not isinstance(resources, dict) or len(resources) > 10000 or not required <= set(resources):
        raise ValueError('host_missing_resources')
    metadata = manifest.get('runtime')
    if (metadata is not None and (not isinstance(metadata, dict) or metadata != RUNTIME or type(metadata.get('host_protocol')) is not int)) or (require_runtime and metadata != RUNTIME):
        raise ValueError('host_unsupported_runtime')
    if require_runtime and (source_revision(manifest) is None or not revision(source_revision(manifest))):
        raise ValueError('host_clean_source_required')
    if metadata is not None and 'source_diff' not in manifest:
        raise ValueError('host_source_diff_required')
    if metadata is not None and not set(RUNTIME_SCRIPTS) <= set(resources):
        raise ValueError('host_missing_runtime_scripts')
    resource_root = candidate / 'resources'
    directory(resource_root)
    total = 0
    for name, expected in resources.items():
        relative = PurePosixPath(name)
        if (not sha(expected) or str(relative) != name or relative.is_absolute() or '..' in relative.parts or '\\' in name
                or not (name == 'AGENT.md' or name.startswith(('connectors/qqbot/', 'vendor/'))
                        or metadata is not None and name in RUNTIME_SCRIPTS)):
            raise ValueError('host_invalid_resource_path')
        path = resource_root
        for part in relative.parts:
            path /= part
            if path.is_symlink():
                raise ValueError('host_resource_symlink')
        if digest(path, 8 * 1024**2) != expected:
            raise ValueError('host_resource_changed')
        total += path.stat().st_size
        if total > 64 * 1024**2:
            raise ValueError('host_resources_too_large')
    if metadata is not None:
        scripts = resource_root / 'scripts'
        directory(scripts)
        if {p.name for p in scripts.iterdir()} != {Path(p).name for p in RUNTIME_SCRIPTS}:
            raise ValueError('host_unlisted_runtime_file')
    identity = {key: manifest[key] for key in ('source', 'binary_sha256', 'resources')}
    if metadata is not None:
        identity['runtime'] = metadata
    if 'source_diff' in manifest:
        diff = manifest['source_diff']
        if (not isinstance(diff, dict) or set(diff) != {'path', 'sha256', 'base_revision', 'target_revision'}
                or diff['path'] != 'source-diff.patch' or not sha(diff['sha256'])
                or not revision(diff['base_revision']) or not revision(diff['target_revision'])
                or diff['target_revision'] != manifest.get('source', {}).get('head')
                or digest(candidate / diff['path'], MAX_DIFF) != diff['sha256']):
            raise ValueError('host_invalid_source_diff')
        patch = read_bytes(candidate / diff['path'], MAX_DIFF).decode('utf8')
        if any(line == 'GIT binary patch' or line.startswith(('Binary files ', 'Submodule '))
               or line.startswith('index ') and line.endswith(' 160000')
               or line.startswith(('old mode ', 'new mode ', 'new file mode ', 'deleted file mode ')) and line.endswith('160000')
               for line in patch.splitlines()):
            raise ValueError('host_unreviewable_source_diff')
        identity['source_diff'] = diff
    if value_digest(identity) != request['candidate_sha256']:
        raise ValueError('host_candidate_identity_changed')
    return binary, resource_root, manifest


def prepare_lease(candidate, manifest):
    plan = dict(operation='switch_binary_preserve_state', candidate_sha256=manifest['candidate_sha256'],
                state_schema_version=1, validation_policy='format-clippy-tests-release-build-v1')
    if manifest.get('runtime') is not None:
        plan.update(operation='switch_runtime_preserve_state', runtime=manifest['runtime'])
    now = int(time.time() * 1000)
    return dict(format_version=1, candidate_sha256=manifest['candidate_sha256'],
                manifest_sha256=digest(Path(candidate) / 'manifest.json'), plan_sha256=value_digest(plan),
                validation_sha256=value_digest(manifest['checks']), state_schema_version=1,
                created_at_ms=now, expires_at_ms=now + 24 * 3600 * 1000)


def validate_lease(request, candidate, manifest):
    lease = request.get('lease')
    expected = prepare_lease(candidate, manifest)
    if (not isinstance(lease, dict) or set(lease) != set(expected)
            or any(type(lease[k]) is not int for k in ('format_version', 'state_schema_version', 'created_at_ms', 'expires_at_ms'))):
        raise ValueError('host_invalid_lease')
    if any(lease[k] != expected[k] for k in set(expected) - {'created_at_ms', 'expires_at_ms'}):
        raise ValueError('host_lease_changed')
    now = int(time.time() * 1000)
    if (any(type(lease[k]) is not int for k in ('created_at_ms', 'expires_at_ms'))
            or lease['created_at_ms'] > now + 1000 or lease['expires_at_ms'] <= now
            or not 0 < lease['expires_at_ms'] - lease['created_at_ms'] <= 24 * 3600 * 1000):
        raise ValueError('host_lease_expired')


def source_revision(manifest):
    source = manifest.get('source', {})
    empty = hashlib.sha256(b'').hexdigest()
    return source.get('head') if source.get('diff') == empty and source.get('untracked') == empty else None


def validate_descriptor(value, candidates, state=None):
    if (not isinstance(value, dict) or set(value) != {'format_version', 'command', 'candidate_request', 'source_revision'}
            or type(value.get('format_version')) is not int or value['format_version'] != 1
            or value['source_revision'] is not None and not revision(value['source_revision'])):
        raise ValueError('host_invalid_descriptor')
    command = value['command']
    if (not isinstance(command, list) or not 1 <= len(command) <= 512
            or any(not isinstance(s, str) or '\0' in s or len(s) > 8192 for s in command)
            or not Path(command[0]).is_absolute() or not Path(argument(command, '--state-dir')).is_absolute()):
        raise ValueError('host_invalid_command')
    if state is not None and argument(command, '--state-dir') != str(state):
        raise ValueError('host_state_directory_changed')
    digest(Path(command[0]))
    if value['candidate_request'] is not None:
        binary, resources, manifest = validate_candidate(value['candidate_request'], candidates)
        if (command[0] != str(binary) or argument(command, '--agent') != str(resources / 'AGENT.md')
                or value['source_revision'] != source_revision(manifest)):
            raise ValueError('host_descriptor_candidate_changed')
    return value


def script_binding(descriptor, candidates, initial):
    if descriptor['candidate_request'] is not None:
        _, resources, manifest = validate_candidate(descriptor['candidate_request'], candidates)
        if manifest.get('runtime') is not None:
            return {str(resources / name): manifest['resources'][name] for name in RUNTIME_SCRIPTS}
    root = Path(initial).resolve().parent
    return {str(root / Path(name).name): digest(root / Path(name).name, 8 * 1024**2) for name in RUNTIME_SCRIPTS}


def validate_record(record, candidates, state, initial=None):
    if (not isinstance(record, dict) or set(record) != {'format_version', 'descriptor', 'scripts', 'binary_sha256'}
            or type(record['format_version']) is not int or record['format_version'] != 1):
        raise ValueError('host_invalid_active_record')
    validate_descriptor(record['descriptor'], candidates, state)
    if not sha(record['binary_sha256']) or digest(Path(record['descriptor']['command'][0])) != record['binary_sha256']:
        raise ValueError('host_active_binary_changed')
    scripts = record['scripts']
    if (not isinstance(scripts, dict) or len(scripts) != 3
            or {Path(p).name for p in scripts} != {Path(p).name for p in RUNTIME_SCRIPTS}
            or len({str(Path(p).parent) for p in scripts}) != 1):
        raise ValueError('host_invalid_script_binding')
    for path, expected in scripts.items():
        if not Path(path).is_absolute() or not sha(expected) or digest(Path(path), 8 * 1024**2) != expected:
            raise ValueError('host_script_changed')
    request = record['descriptor']['candidate_request']
    if request is not None:
        _, resources, manifest = validate_candidate(request, candidates)
        if manifest.get('runtime') is not None and scripts != {str(resources / n): manifest['resources'][n] for n in RUNTIME_SCRIPTS}:
            raise ValueError('host_script_binding_changed')
    return record


def candidate_command(current, requested, request, control, candidates):
    binary, resources, manifest = validate_candidate(request, candidates, True)
    validate_lease(request, binary.parent, manifest)
    result = list(current)
    for flag in ('--practice-mindustry-json-rules', '--practice-python'):
        while flag in result:
            index = result.index(flag)
            del result[index:index + 2]
    result[0] = str(binary)
    def replace(flag, value):
        argument(result, flag)
        result[result.index(flag) + 1] = str(value)
    replace('--agent', resources / 'AGENT.md')
    if '--bridge-script' in result:
        old = Path(argument(result, '--bridge-script'))
        if old.name == 'bridge.mjs' and old.parent.name == 'qqbot':
            target = Path(argument(requested, '--bridge-script'))
            allowed = {resources / 'connectors/qqbot/bridge.mjs',
                       Path(control) / 'runtime-resources' / request['candidate_sha256'] / 'qqbot/bridge.mjs'}
            if target not in allowed:
                raise ValueError('host_unbound_bridge_command')
            base = resources if target == resources / 'connectors/qqbot/bridge.mjs' else Path(control)
            current_path = base
            directory(base)
            for part in target.parent.relative_to(base).parts:
                current_path /= part
                directory(current_path)
            for name, expected in manifest['resources'].items():
                if name.startswith('connectors/qqbot/'):
                    path = target.parent
                    for part in PurePosixPath(name.removeprefix('connectors/qqbot/')).parts:
                        path /= part
                        if path.is_symlink():
                            raise ValueError('host_bridge_resource_symlink')
                    if digest(path, 8 * 1024**2) != expected:
                        raise ValueError('host_bridge_resource_changed')
            replace('--bridge-script', target)
    if '--research-corpus' in result:
        replace('--research-corpus', resources / 'vendor/eve-portable-knowledge')
    for index, value in enumerate(result[:-1]):
        if value == '--reference' and result[index + 1].startswith('eve-portable='):
            result[index + 1] = 'eve-portable=' + str(resources / 'vendor/eve-portable-knowledge')
    if result != requested:
        raise ValueError('host_candidate_command_changed')
    return dict(format_version=1, command=result, candidate_request=request, source_revision=source_revision(manifest))
