#!/usr/bin/env python3
"""真实离线 Cargo 验收；仅创建并清理本次零根文件系统镜像，不拉取镜像或执行宿主依赖脚本。"""
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import uuid

ROOT = Path(__file__).resolve().parents[1]
cli = shutil.which('docker')
if sys.platform != 'linux' or not cli:
    raise SystemExit('需要 Linux、本地 Docker 与已安装的 Rust/bubblewrap')
environment = os.environ.copy()
for name in ('DOCKER_HOST', 'DOCKER_CONTEXT', 'DOCKER_TLS', 'DOCKER_TLS_VERIFY', 'DOCKER_CERT_PATH'):
    environment.pop(name, None)
docker = [cli, '--host=unix:///var/run/docker.sock']
subprocess.run(docker + ['info', '--format', '{{.CgroupVersion}}'], env=environment, check=True, timeout=15)
lock = tomllib.loads((ROOT / 'Cargo.lock').read_text())
version, = [p['version'] for p in lock['package'] if p['name'] == 'serde_json']
toolchain = subprocess.check_output(['rustc', '--print', 'sysroot'], text=True, env=environment, timeout=15).strip()
tag = 'eve-cargo-acceptance:' + uuid.uuid4().hex
created = False
with tempfile.TemporaryDirectory(prefix='eve-cargo-acceptance-') as directory:
    try:
        bundle = json.loads(subprocess.check_output([sys.executable, str(ROOT / 'scripts/prepare-cargo-fixture.py'),
            '--out', str(Path(directory) / 'vendor')], text=True, env=environment, timeout=30))
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode='w'):
            pass
        image = subprocess.check_output(docker + ['import', '-', tag], input=archive.getvalue(), env=environment, timeout=15).decode().strip()
        created = True
        environment.update(EVE_TEST_CARGO_CLI=cli, EVE_TEST_CARGO_IMAGE=image,
            EVE_TEST_CARGO_TOOLCHAIN=toolchain, EVE_TEST_CARGO_VENDOR=str(Path(directory) / 'vendor'),
            EVE_TEST_CARGO_BUNDLE_SHA256=bundle['bundle_sha256'], EVE_TEST_CARGO_SERDE_JSON=version)
        print(json.dumps({'bundle':bundle, 'image':image, 'network':False}), flush=True)
        for arguments in [
            [sys.executable, 'crates/script-runtime/tests/cargo_container.py', '-v'],
            ['cargo', 'test', '-p', 'eve-script-runtime', '--test', 'cargo_api', 'actual_cargo_', '--', '--ignored', '--nocapture', '--test-threads=1'],
            [sys.executable, 'connectors/qqbot/test/script_runtime_test.py', 'ScriptRepairAcceptance.test_cargo_dependency_plugin_is_saved_executed_and_restored_in_real_host', '-v'],
        ]:
            subprocess.run(arguments, cwd=ROOT, env=environment, check=True, timeout=180)
    finally:
        if created:
            subprocess.run(docker + ['image', 'rm', tag], env=environment, check=True, timeout=15)
