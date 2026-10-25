"""宿主源码自动同步：独立检出、完整验收、冻结候选，由升级监督器在空闲时切换。"""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import shutil
import signal
import subprocess
import tempfile
import threading
import time

from build_resources import BuildPlanner
import self_upgrade_runtime as runtime

EMPTY = hashlib.sha256(b'').hexdigest()


def configuration(path):
    value = runtime.read_json(path)
    allowed = {'version', 'enabled', 'repository', 'branch', 'data_directory', 'interval_seconds',
               'cargo_program', 'cargo_arguments', 'jobs', 'timeout_seconds', 'minimum_free_bytes'}
    if not isinstance(value, dict) or set(value) - allowed or value.get('version') != 1:
        raise ValueError('invalid_auto_update_configuration')
    result = dict(enabled=True, repository='ZenthXSin/Eve.aic', branch='main', interval_seconds=3600,
                  cargo_program='cargo', cargo_arguments=[], jobs='auto', timeout_seconds=600,
                  minimum_free_bytes=1024**3)
    result.update({k: v for k, v in value.items() if k != 'version'})
    if (type(result['enabled']) is not bool
            or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*', result['repository'])
            or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_./-]*', result['branch'])
            or '..' in result['branch'] or result['branch'].endswith(('/', '.'))
            or type(result['interval_seconds']) is not int or not 60 <= result['interval_seconds'] <= 604800
            or not (result['jobs'] == 'auto' or type(result['jobs']) is int and 1 <= result['jobs'] <= 256)
            or type(result['timeout_seconds']) is not int or not 30 <= result['timeout_seconds'] <= 3600
            or type(result['minimum_free_bytes']) is not int or result['minimum_free_bytes'] < 256 * 1024**2
            or not isinstance(result['cargo_program'], str) or not result['cargo_program']
            or not isinstance(result['cargo_arguments'], list)
            or any(not isinstance(a, str) or '\0' in a for a in result['cargo_arguments'])):
        raise ValueError('invalid_auto_update_configuration')
    directory = Path(result['data_directory'])
    if not directory.is_absolute() or directory.is_symlink():
        raise ValueError('auto_update_directory_requires_absolute_real_path')
    result['data_directory'] = str(directory.resolve())
    return result


def build_environment(settings):
    names = {'PATH', 'SystemRoot', 'SYSTEMROOT', 'TEMP', 'TMP', 'HOME', 'USERPROFILE',
             'CARGO_HOME', 'RUSTUP_HOME', 'RUSTUP_TOOLCHAIN', 'JAVA_HOME', 'LD_LIBRARY_PATH',
             'PYTHONHOME', 'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'NO_PROXY',
             'http_proxy', 'https_proxy', 'all_proxy', 'no_proxy', 'SSL_CERT_FILE',
             'SSL_CERT_DIR', 'REQUESTS_CA_BUNDLE', 'CURL_CA_BUNDLE', 'NODE_EXTRA_CA_CERTS'}
    env = {k: v for k, v in os.environ.items() if k in names}
    env.update(CARGO_INCREMENTAL='0', CARGO_PROFILE_RELEASE_DEBUG='0',
               CARGO_BUILD_JOBS=str(1 if settings['jobs'] == 'auto' else settings['jobs']), GIT_TERMINAL_PROMPT='0')
    return env


class GitBuilder:
    """可替换的源码取得与候选准备实现；测试只在代码注入本地 Git 源，不开放配置 URL。"""
    def __init__(self, settings, candidates, stopped, publish, remote=None, planner=None):
        self.settings, self.candidates = settings, Path(candidates).resolve()
        self.root = Path(settings['data_directory'])
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.stopped, self.publish = stopped, publish
        self.remote = remote or 'https://github.com/' + settings['repository'] + '.git'
        self.env = build_environment(settings)
        self.planner = planner or BuildPlanner()

    def command(self, args, cwd, logfile, seconds=None, plan=None, stdout_file=None):
        self.root.mkdir(parents=True, exist_ok=True)
        if self.stopped.is_set():
            raise ValueError('auto_update_cancelled')
        with contextlib.ExitStack() as stack:
            output = stack.enter_context(logfile.open('wb'))
            stdout = stack.enter_context(stdout_file.open('wb')) if stdout_file else output
            if plan:
                output.write(('BUILD_RESOURCES ' + json.dumps(plan, sort_keys=True) + '\n').encode()); output.flush()
            env = dict(self.env)
            if plan: env.update(CARGO_BUILD_JOBS=str(plan['jobs']), RUST_TEST_THREADS=str(plan['jobs']))
            flags = subprocess.BELOW_NORMAL_PRIORITY_CLASS if os.name == 'nt' and plan else 0
            child = subprocess.Popen(args, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                     stdout=stdout, stderr=output if stdout_file else subprocess.STDOUT,
                                     creationflags=flags, start_new_session=(os.name != 'nt'))
            if plan and hasattr(os, 'setpriority'):
                with contextlib.suppress(OSError): os.setpriority(os.PRIO_PROCESS, child.pid, 10)
            next_probe = time.monotonic() + 1
            deadline = time.monotonic() + (seconds or self.settings['timeout_seconds'])
            try:
                while child.poll() is None:
                    if self.stopped.wait(.2):
                        raise ValueError('auto_update_cancelled')
                    if time.monotonic() > deadline:
                        raise ValueError('auto_update_command_timeout')
                    if shutil.disk_usage(self.root).free < 128 * 1024**2:
                        raise ValueError('auto_update_disk_pressure')
                    if plan and time.monotonic() >= next_probe:
                        next_probe = time.monotonic() + 1
                        if self.planner.critical(): raise ValueError('auto_update_resources_busy')
                if child.returncode:
                    raise ValueError('auto_update_check_failed')
            finally:
                if child.poll() is None:
                    if os.name == 'nt':
                        with contextlib.suppress(OSError, subprocess.TimeoutExpired):
                            subprocess.run(['taskkill', '/PID', str(child.pid), '/T', '/F'],
                                           env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
                        if child.poll() is None: child.terminate()
                    else:
                        with contextlib.suppress(ProcessLookupError): os.killpg(child.pid, signal.SIGTERM)
                    try:
                        child.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        if os.name == 'nt': child.kill()
                        else:
                            with contextlib.suppress(ProcessLookupError): os.killpg(child.pid, signal.SIGKILL)
                        child.wait(timeout=5)
        return {'success': True, 'exit_code': 0, 'stdout': logfile.read_bytes()[-16384:].decode(errors='replace'),
                'stderr': '', 'stdout_truncated': logfile.stat().st_size > 16384, 'stderr_truncated': False}

    def git(self, args, cwd, log):
        self.command(['git', '-c', 'credential.helper=', '-c', 'core.hooksPath=/dev/null', *args], cwd, log, 120)
        return log.read_text().strip()

    def prepare(self, active_head, forced=False, failed_head=None):
        source = self.root / 'source'
        logs = self.root / 'logs'; logs.mkdir(exist_ok=True)
        git_log = logs / 'git.log'
        source.mkdir(exist_ok=True)
        if source.is_symlink(): raise ValueError('auto_update_source_symlink')
        if not (source / '.git').exists(): self.git(['init'], source, git_log)
        if self.git(['status', '--porcelain', '--untracked-files=all'], source, git_log):
            raise ValueError('auto_update_checkout_has_local_changes')
        self.git(['fetch', '--no-tags', '--depth=200', self.remote,
                  'refs/heads/' + self.settings['branch']], source, git_log)
        head = self.git(['rev-parse', 'FETCH_HEAD'], source, git_log)
        if not re.fullmatch('[a-f0-9]{40}', head): raise ValueError('invalid_upstream_revision')
        self.publish(phase='checking', target_head=head)
        if head == active_head: return {'head': head, 'up_to_date': True}
        if head == failed_head and not forced: return {'head': head, 'previous_failure': True}
        if active_head:
            # 不自动降级、切换到不相关历史或丢弃 Eve 已激活的分支功能。
            self.git(['fetch', '--no-tags', '--depth=200', self.remote, active_head], source, git_log)
            try: self.git(['merge-base', '--is-ancestor', active_head, head], source, git_log)
            except ValueError: raise ValueError('active_source_not_upstream') from None
        if shutil.disk_usage(self.root).free < self.settings['minimum_free_bytes']:
            raise ValueError('auto_update_insufficient_space')
        self.git(['checkout', '--detach', head], source, git_log)
        self.publish(phase='verifying', target_head=head)
        target = self.root / 'target'
        cargo = [self.settings['cargo_program'], *self.settings['cargo_arguments']]
        metadata = logs / (head + '-metadata.json')
        self.command(cargo + ['metadata', '--format-version', '1', '--no-deps', '--locked'],
                     source, logs / (head + '-metadata.log'), stdout_file=metadata)
        try:
            packages = runtime.read_json(metadata, 16 * 1024**2)['packages']
            app = [p for p in packages if p['name'] == 'eve-app']
            binaries = sorted(t['name'] for p in app for t in p['targets'] if t['kind'] == ['bin'])
            if (len(app) != 1 or not 1 <= len(binaries) <= 32 or len(set(binaries)) != len(binaries)
                    or 'eve-qqbot' not in binaries or any(not re.fullmatch('[A-Za-z0-9_-]+', b) for b in binaries)):
                raise ValueError()
        except (KeyError, TypeError, ValueError):
            raise ValueError('auto_update_invalid_build_metadata') from None
        common = ['--release', '--locked', '--target-dir', str(target)]
        stages = [
            (1, 'format', ['fmt', '--all', '--', '--check']),
            (2, 'libraries', ['build', '--workspace', '--lib', *common]),
            (2, 'clippy', ['clippy', '--workspace', '--all-targets', *common, '--', '-D', 'warnings']),
            (3, 'test_compile', ['test', '--workspace', '--no-run', *common]),
            (3, 'tests', ['test', '--workspace', *common]),
            *[(4, 'binary', ['build', '-p', 'eve-app', '--bin', binary, *common]) for binary in binaries],
        ]
        groups = [[], [], [], []]
        for index, (check, stage, arguments) in enumerate(stages):
            plan = self.planner.choose(stage, self.settings['jobs'])
            self.publish(phase='verifying', target_head=head, check=check, stage=stage,
                         stage_index=index + 1, stage_count=len(stages), effective_jobs=plan['jobs'],
                         resources=plan, binary=arguments[arguments.index('--bin') + 1] if '--bin' in arguments else None)
            if not plan['jobs']: raise ValueError('auto_update_resources_busy')
            groups[check - 1].append(self.command(cargo + arguments, source,
                logs / (head + '-' + str(check) + '-' + str(index + 1) + '-' + stage + '.log'), plan=plan))
        checks = []
        for group in groups:
            output = '\n'.join(r['stdout'] for r in group)
            checks.append(dict(success=True, exit_code=0, stdout=output[-16384:], stderr='',
                stdout_truncated=len(output) > 16384 or any(r['stdout_truncated'] for r in group), stderr_truncated=False,
                steps=[dict(success=r['success'], exit_code=r['exit_code']) for r in group]))
        if self.git(['rev-parse', 'HEAD'], source, git_log) != head or self.git(['status', '--porcelain', '--untracked-files=all'], source, git_log):
            raise ValueError('auto_update_source_changed_during_validation')
        return self.freeze(source, head, target / 'release' / ('eve-qqbot.exe' if os.name == 'nt' else 'eve-qqbot'), checks, git_log)

    def freeze(self, source, head, binary, checks, git_log):
        self.candidates.mkdir(parents=True, exist_ok=True)
        resources = {}
        names = self.git(['ls-files', '-z', '--', 'AGENT.md', 'connectors/qqbot', 'vendor'], source, git_log).split('\0')
        total = 0
        for name in filter(None, names):
            path = source / name
            if path.is_symlink() or not path.is_file(): raise ValueError('invalid_auto_update_resource')
            total += path.stat().st_size
            if total > 64 * 1024**2 or len(resources) >= 10000: raise ValueError('auto_update_resources_too_large')
            resources[name] = runtime.digest(path)
        source_id = {'head': head, 'diff': EMPTY, 'untracked': EMPTY}
        binary_sha = runtime.digest(binary)
        identity = {'source': source_id, 'binary_sha256': binary_sha, 'resources': resources}
        key = hashlib.sha256(json.dumps(identity, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        directory = self.candidates / key
        request = dict(format_version=1, candidate_sha256=key, candidate_directory=str(directory),
                       session_id='host:auto-update', requested_by='host:auto-update', turn_id=1)
        if not directory.exists():
            if shutil.disk_usage(self.candidates).free < binary.stat().st_size + total + 128 * 1024**2:
                raise ValueError('auto_update_insufficient_space_for_candidate')
            with tempfile.TemporaryDirectory(dir=self.candidates) as temporary:
                stage = Path(temporary)
                shutil.copy2(binary, stage / binary.name); (stage / binary.name).chmod(0o700)
                for name in resources:
                    dest = stage / 'resources' / name; dest.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(source / name, dest)
                runtime.atomic(stage / 'manifest.json', dict(format_version=1, prepared=True, success=True, applied=False,
                    source=source_id, binary_sha256=binary_sha, candidate_sha256=key,
                    candidate_path=str(directory / binary.name), resources=resources, checks=checks))
                stage.rename(directory)
        runtime.validate_candidate(request, self.candidates)
        return {'head': head, 'request': request}


class SourceUpdater:
    def __init__(self, config, control, candidates, builder_factory=GitBuilder):
        self.config_path, self.control, self.candidates = Path(config), Path(control), Path(candidates)
        self.settings = configuration(self.config_path)
        root = Path(self.settings['data_directory'])
        if any(root == p.resolve() or root in p.resolve().parents or p.resolve() in root.parents for p in (self.control, self.candidates)):
            raise ValueError('auto_update_directory_overlaps_runtime')
        self.builder_factory = builder_factory
        self.stopped, self.thread, self.pending = threading.Event(), None, None
        self.results, self.mutex = queue.Queue(), threading.RLock()
        self.next_check, self.quiet, self.last_tick = 0, None, 0
        path = self.control / 'auto-update.json'
        self.status = runtime.read_json(path) if path.exists() else {'format_version': 1}
        if self.status.get('format_version') != 1: raise ValueError('unknown_auto_update_status')
        self.publish(phase='idle', enabled=self.settings['enabled'], interrupted=self.status.get('phase') in ('checking', 'verifying', 'waiting'))

    def publish(self, **changes):
        with self.mutex:
            self.status.update(changes, updated_at_ms=int(time.time() * 1000))
            runtime.atomic(self.control / 'auto-update.json', self.status)

    def active(self, supervisor):
        descriptor = supervisor.active_descriptor
        if not descriptor['candidate_request']: return None
        _, _, manifest = runtime.validate_candidate(descriptor['candidate_request'], self.candidates)
        source = manifest['source']
        if source['diff'] != EMPTY or source['untracked'] != EMPTY:
            raise ValueError('active_source_has_local_changes')
        return source['head']

    def tick(self, supervisor):
        if time.monotonic() - self.last_tick < .5: return
        self.last_tick = time.monotonic()
        try:
            self.settings = configuration(self.config_path)
            root = Path(self.settings['data_directory'])
            if any(root == p.resolve() or root in p.resolve().parents or p.resolve() in root.parents
                   for p in (self.control, self.candidates, supervisor.state.parent)):
                raise ValueError('auto_update_directory_overlaps_runtime')
            signature = hashlib.sha256(json.dumps(self.settings, sort_keys=True).encode()).hexdigest()
            if not self.settings['enabled']:
                self.stopped.set(); self.pending = None; self.quiet = None
                if self.thread and not self.thread.is_alive():
                    self.thread.join(); self.thread = None; self.results.get_nowait()
                if self.status.get('phase') != 'disabled': self.publish(phase='disabled', enabled=False)
                return
            if self.status.get('enabled') is False:
                if self.thread:
                    if self.thread.is_alive(): return
                    self.thread.join(); self.thread = None; self.results.get_nowait()
                self.next_check = 0
                self.publish(phase='idle', enabled=True)
            if self.pending:
                if self.pending['configuration'] != signature:
                    self.pending = None; self.next_check = 0; self.publish(phase='idle', result='configuration_changed'); return
                if self.active(supervisor) != self.pending['base_head']:
                    self.pending = None; self.next_check = 0; self.publish(phase='idle', result='active_version_changed'); return
                if not runtime.runtime_idle_gate(supervisor.state, supervisor.inherited_receipts):
                    self.quiet = None; self.publish(phase='waiting', result='waiting_for_idle'); return
                self.quiet = self.quiet or time.monotonic()
                if time.monotonic() - self.quiet < supervisor.options.quiet_seconds: return
                # 快照及运行时下一次原子状态提交都需要空间；不能先停旧进程再发现放不下备份。
                state_bytes = sum(p.stat().st_size for p in supervisor.state.parent.rglob('*')
                                  if p.is_file() and not p.is_symlink())
                if shutil.disk_usage(self.control).free < state_bytes * 2 + 256 * 1024**2:
                    raise ValueError('auto_update_insufficient_space_for_backup')
                request = self.pending['request']
                path = self.control / 'auto-request.json'
                runtime.atomic(path, request)
                command, manifest = supervisor.candidate_command(request)
                if not runtime.runtime_idle_gate(supervisor.state, supervisor.inherited_receipts):
                    path.unlink(); self.quiet = None; return
                supervisor.switch(request, command, manifest, request_path=path)
                result = supervisor.status['result']
                self.publish(phase='idle', result=result, current_head=self.pending['head'] if result == 'applied' else self.pending['base_head'], applied_head=self.pending['head'] if result == 'applied' else None,
                             failed_head=None if result == 'applied' else self.pending['head'])
                self.pending = None; self.quiet = None; return
            if self.thread:
                if self.thread.is_alive(): return
                self.thread.join(); self.thread = None
                result, base_head, job_signature = self.results.get_nowait()
                if job_signature != signature:
                    self.next_check = 0; self.publish(phase='idle', result='configuration_changed'); return
                if 'request' in result:
                    self.pending = dict(result, base_head=base_head, configuration=signature); self.quiet = None
                    self.publish(phase='waiting', prepared_head=result['head']); return
                if result.get('result') == 'deferred': self.next_check = min(self.next_check, time.monotonic() + 60)
                self.publish(phase='idle', **result, **({} if 'result' in result else {'result': 'up_to_date' if result.get('up_to_date') else 'previous_failure'})); return
            force = self.control / 'check-updates.json'
            forced = force.exists()
            if time.monotonic() < self.next_check and not forced: return
            if forced:
                value = runtime.read_json(force)
                if set(value) != {'format_version', 'nonce'} or value['format_version'] != 1 or not isinstance(value['nonce'], str):
                    raise ValueError('invalid_auto_update_trigger')
                force.unlink()
            base = self.active(supervisor)
            self.next_check = time.monotonic() + self.settings['interval_seconds']
            self.publish(phase='checking', enabled=True, current_head=base, error=None, check=0, stage=None, stage_index=0, stage_count=None, effective_jobs=None, resources=None, binary=None)
            settings = dict(self.settings)
            if self.stopped.is_set(): self.stopped = threading.Event()
            def work():
                try:
                    builder = self.builder_factory(settings, self.candidates, self.stopped, self.publish)
                    result = builder.prepare(base, forced, self.status.get('failed_head'))
                except Exception as error:
                    code = str(error) if type(error) is ValueError and str(error).replace('_', '').isalnum() else type(error).__name__
                    result = {'result': 'deferred' if code == 'auto_update_resources_busy' else 'failed', 'error': code[:100], 'failed_head': None if code == 'auto_update_resources_busy' else self.status.get('target_head')}
                self.results.put((result, base, signature))
            self.thread = threading.Thread(target=work, daemon=True); self.thread.start()
        except Exception as error:
            if (self.control / 'processing.json').exists(): raise
            code = str(error) if type(error) is ValueError and str(error).replace('_', '').isalnum() else type(error).__name__
            self.pending = None; self.quiet = None
            self.next_check = time.monotonic() + self.settings['interval_seconds']
            self.publish(phase='idle', result='failed', error=code[:100])

    def stop(self):
        self.stopped.set()
        if self.thread: self.thread.join(timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=Path, required=True)
    parser.add_argument('action', choices=['check', 'status'])
    args = parser.parse_args()
    path = args.directory / 'auto-update.json'
    status = runtime.read_json(path)
    if args.action == 'status': print(json.dumps(status, ensure_ascii=False)); return
    if not status.get('enabled'): raise ValueError('auto_update_not_enabled')
    import uuid
    target = args.directory / 'check-updates.json'
    if not target.exists(): runtime.atomic(target, {'format_version': 1, 'nonce': uuid.uuid4().hex})
    print('已提交立即检查；请用 status 查看验证与切换结果')


if __name__ == '__main__':
    main()
