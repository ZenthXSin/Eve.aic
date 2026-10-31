"""受限 Cargo 控制器真实进程验收；依赖由操作者提供的离线公共 vendor 包。"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import uuid

CONTROL = Path(__file__).resolve().parents[1] / 'src/cargo_controller.py'
IMAGE = os.environ.get('EVE_TEST_CARGO_IMAGE')
VENDOR = os.environ.get('EVE_TEST_CARGO_VENDOR')
TOOLCHAIN = os.environ.get('EVE_TEST_CARGO_TOOLCHAIN')
VERSION = os.environ.get('EVE_TEST_CARGO_SERDE_JSON', '1.0.151')


@unittest.skipUnless(sys.platform.startswith('linux'), '控制器仅支持 Linux')
class ControllerSafety(unittest.TestCase):
    def test_host_execution_fails_before_cleanup_or_compilation(self):
        result = subprocess.run([sys.executable, str(CONTROL)], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout)['code'], 'trusted_controller_requires_private_pid_namespace')

    def test_invalid_utf8_diagnostics_report_actual_truncation(self):
        spec = importlib.util.spec_from_file_location('cargo_control_under_test', CONTROL)
        controller = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(controller)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'diagnostic'
            path.write_bytes(b'\xff' * 4096)
            text, truncated = controller.diagnostic(path)
            self.assertLessEqual(len(text.encode()), 8192)
            self.assertTrue(truncated)


@unittest.skipUnless(IMAGE and VENDOR and TOOLCHAIN, '需显式配置本地镜像、toolchain 和公共 vendor')
class CargoIsolation(unittest.TestCase):
    def setUp(self):
        self.assertRegex(IMAGE, r'^sha256:[0-9a-f]{64}$')
        self.directory = tempfile.TemporaryDirectory(prefix='eve-cargo-container-test-')
        self.root = Path(self.directory.name)
        self.environment = os.environ.copy()
        for key in ['DOCKER_HOST', 'DOCKER_CONTEXT', 'DOCKER_TLS', 'DOCKER_TLS_VERIFY', 'DOCKER_CERT_PATH']:
            self.environment.pop(key, None)
        self.environment['EVE_CARGO_TEST_SECRET'] = 'HOST_PRIVATE_FIXTURE'
        self.docker = ['docker', '--host=unix:///var/run/docker.sock']
        size = subprocess.check_output(self.docker + ['image', 'inspect', IMAGE, '--format', '{{.Size}}'],
            env=self.environment, text=True)
        self.assertLessEqual(int(size), 64 * 1024**2, '编译镜像根层超过64MiB，拒绝在复制型存储驱动上扩大宿主磁盘压力')

    def tearDown(self):
        self.directory.cleanup()

    def build(self, source, build_source=None, pids=64, readonly=True):
        work = self.root / uuid.uuid4().hex
        work.mkdir()
        inputs = work / 'input'
        inputs.mkdir(mode=0o755)
        inputs.chmod(0o755)
        (inputs / 'main.rs').write_text(source)
        (inputs / 'Cargo.toml').write_text('[package]\nname="eve_plugin"\nversion="0.1.0"\nedition="2021"\n'
            f'[dependencies]\nserde_json="={VERSION}"\n[profile.dev]\ndebug=0\nincremental=false\ncodegen-units=1\npanic="abort"\n')
        if build_source:
            (inputs / 'build.rs').write_text(build_source)
        control = work / 'control.py'
        shutil.copyfile(CONTROL, control)
        for path in [*inputs.iterdir(), control]:
            path.chmod(0o644)
        name = 'eve-cargo-test-' + uuid.uuid4().hex
        command = self.docker + ['run', '--rm', '--pull=never', '--name', name, '-i', '--network=none',
            '--user=0:0', '--cap-drop=ALL', '--cap-add=KILL', '--cap-add=CHOWN', '--cap-add=FOWNER',
            '--cap-add=SETUID', '--cap-add=SETGID', '--security-opt=no-new-privileges',
            f'--pids-limit={pids}', '--memory=2g', '--memory-swap=2g', '--cpus=1',
            '--ulimit=nofile=64:64', '--ulimit=core=0:0']
        if readonly:
            command.append('--read-only')
        for path, size, execution in [('tmp', 16, 'noexec'), ('project', 8, 'noexec'),
                                      ('target', 256, 'exec'), ('cargo', 16, 'noexec'), ('out', 16, 'noexec')]:
            command += ['--tmpfs', f'/{path}:rw,{execution},nosuid,size={size}m']
        mounts = [(inputs, '/input'), (Path(VENDOR), '/vendor'), (control, '/control.py'),
                  (Path(TOOLCHAIN), '/toolchain')]
        for path in ['/usr/bin', '/usr/lib', '/usr/libexec', '/lib', '/lib64']:
            if Path(path).exists():
                mounts.append((Path(path), path))
        for host, guest in mounts:
            command += ['--mount', f'type=bind,source={host},target={guest},readonly']
        command += ['--entrypoint=/usr/bin/env', IMAGE, '-i', 'PATH=/usr/bin', 'HOME=/tmp',
                    '/usr/bin/python3', '-I', '-B', '/control.py']
        process = None
        try:
            process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, env=self.environment)
            line = process.stdout.readline()
            if not line:
                self.fail(process.stderr.read()[-2000:])
            proof = json.loads(line)
            binary = None
            if proof.get('ok'):
                binary = work / 'plugin'
                extracted = subprocess.run(self.docker + ['exec', '--user=0:0', name, '/usr/bin/cat', '/out/plugin'],
                    env=self.environment, capture_output=True, check=True, timeout=5)
                self.assertLessEqual(len(extracted.stdout), 16 * 1024**2)
                with binary.open('xb') as output:
                    output.write(extracted.stdout)
                self.assertEqual(binary.stat().st_size, proof['artifact_bytes'])
                self.assertEqual(hashlib.sha256(binary.read_bytes()).hexdigest(), proof['artifact_sha256'])
                binary.chmod(0o755)
            process.stdin.close()
            process.wait(timeout=5)
            return proof, binary
        finally:
            if process and process.poll() is None:
                process.kill()
                process.wait()
            subprocess.run(self.docker + ['rm', '-f', name], env=self.environment, capture_output=True, timeout=10)
            if process:
                process.stdout.close()
                process.stderr.close()

    def test_real_offline_serde_compilation_and_immutable_artifact(self):
        proof, binary = self.build('fn main(){println!("{}",serde_json::json!({"compiled":true,"value":42}));}')
        self.assertTrue(proof['ok'], proof)
        self.assertIsNotNone(binary)
        self.assertLessEqual(proof['limits']['memory_bytes'], 2 * 1024**3)
        self.assertLessEqual(proof['limits']['pids'], 64)
        self.assertEqual(binary.read_bytes()[:4], b'\x7fELF')

    def test_weakened_limits_and_writable_root_fail_before_user_compilation(self):
        for options, reason in [({'pids': 128}, 'resource_limits_unverified'),
                                ({'readonly': False}, 'root_filesystem_writable')]:
            proof, binary = self.build('compile_error!("must not compile");', **options)
            self.assertFalse(proof['ok'])
            self.assertEqual(proof['code'], reason)
            self.assertIsNone(binary)

    def test_build_script_cannot_read_host_files_credentials_or_network(self):
        secret = self.root / 'host-secret.txt'
        secret.write_text('HOST_PRIVATE_FILE')
        build = '''fn main(){
assert!(std::fs::read_to_string(HOST_PATH).is_err());
assert!(std::env::var("EVE_CARGO_TEST_SECRET").is_err());
let status=std::fs::read_to_string("/proc/self/status").unwrap();
assert!(status.lines().any(|line|line.starts_with("Uid:")&&line.split_whitespace().skip(1).all(|id|id=="EXPECTED_UID")));
assert!(status.contains("NoNewPrivs:\\t1"));
assert!(std::net::TcpStream::connect_timeout(&"1.1.1.1:80".parse().unwrap(),std::time::Duration::from_millis(200)).is_err());
println!("cargo:rustc-env=SANDBOX_VERIFIED=ok");
}'''.replace('HOST_PATH', json.dumps(str(secret))).replace('EXPECTED_UID', str(Path(TOOLCHAIN).stat().st_uid or 65534))
        proof, binary = self.build('fn main(){assert_eq!(env!("SANDBOX_VERIFIED"),"ok");}', build)
        self.assertTrue(proof['ok'], proof)
        self.assertIsNotNone(binary)
        self.assertEqual(secret.read_text(), 'HOST_PRIVATE_FILE')


if __name__ == '__main__':
    unittest.main()
