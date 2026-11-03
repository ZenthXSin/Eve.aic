"""升级候选的真实隔离进程验收；任一失败即阻断候选发布。"""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import subprocess
import shutil
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, help='对实际构建产物运行隔离的完整重启/运行包交接')
    args = parser.parse_args()
    directory = Path(__file__).resolve().parent
    # 测试仅创建自己的临时状态和本地传输替身，不连接正式 QQ 或模型。
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE='1', PYTHONUTF8='1')
    frozen = contextlib.ExitStack()
    if args.binary:
        binary = args.binary.resolve(strict=True)
        temporary = Path(frozen.enter_context(tempfile.TemporaryDirectory(prefix='eve-binary-rehearsal-')))
        with binary.open('rb') as stream:
            binary_digest = hashlib.file_digest(stream, 'sha256').hexdigest()
        target = temporary / binary.name
        shutil.copy2(binary, target)
        with target.open('rb') as stream:
            if hashlib.file_digest(stream, 'sha256').hexdigest() != binary_digest:
                frozen.close()
                raise ValueError('binary_changed_during_rehearsal_freeze')
        binary = target
        env['EVE_QQBOT_BINARY'] = str(binary)
        # 测试自行生成凭据、监听回环地址及临时状态；不继承正式业务配置。
        env = {key: value for key, value in env.items()
               if not key.startswith(('EVE_', 'QQBOT_')) or key == 'EVE_QQBOT_BINARY'}
        suites = [directory.parent / 'connectors/qqbot/test/live_upgrade_test.py']
        identity = dict(kind='isolated_binary_rehearsal', binary_sha256=binary_digest,
                        production_switched=False)
    else:
        suites = [directory / name for name in ('test-self-upgrade-runtime.py', 'test-source-update.py',
                  'test-service-guard.py', 'test-host-runtime-bridge.py')]
        identity = dict(kind='isolated_protocol_rehearsal', production_switched=False)
    # 各套件有独立临时目录与子进程；并行保留完整验收，避免串行等待耗尽单命令期限。
    processes = []
    try:
        for suite in suites:
            print('EVE_UPGRADE_VERIFY ' + suite.name, flush=True)
            processes.append(subprocess.Popen([sys.executable, '-B', str(suite)], env=env))
        codes = [process.wait() for process in processes]
        code = next((code if code > 0 else 1 for code in codes if code), 0)
        print('EVE_UPGRADE_EVIDENCE ' + json.dumps(dict(identity, success=code == 0,
              suites=[dict(name=path.name, exit_code=code) for path, code in zip(suites, codes)]),
              ensure_ascii=False, sort_keys=True), flush=True)
        return code
    finally:
        for process in processes:
            if process.poll() is None:
                process.terminate()
        for process in processes:
            if process.poll() is None:
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        frozen.close()


if __name__ == '__main__':
    raise SystemExit(main())
