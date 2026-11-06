#!/usr/bin/env python3
"""验证生产包拒绝缺证据、错二进制与篡改日志；不运行 Cargo 或生产程序。"""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest


SPEC = importlib.util.spec_from_file_location("debian12_release", Path(__file__).with_name("debian12-release.py"))
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleaseEvidenceAcceptance(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="eve-release-evidence-")
        self.work = Path(self.temporary.name)
        self.repo = self.work / "source"
        self.repo.mkdir()
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        subprocess.run(["git", "-C", str(self.repo), "config", "user.email", "fixture@invalid"], check=True)
        subprocess.run(["git", "-C", str(self.repo), "config", "user.name", "release fixture"], check=True)
        (self.repo / "Cargo.lock").write_text("version = 4\n")
        manifest = self.repo / "vendor/codex-0.162.0/UPSTREAM_FILES.json"
        manifest.parent.mkdir(parents=True)
        manifest.write_text('{"synthetic": true}\n')
        subprocess.run(["git", "-C", str(self.repo), "add", "."], check=True)
        subprocess.run(["git", "-C", str(self.repo), "commit", "-qm", "合成来源"], check=True)
        self.old_root = release.ROOT
        release.ROOT = self.repo
        self.revision = release.read("git", "rev-parse", "HEAD")
        self.out = self.work / "out"
        self.evidence = self.out / "evidence"
        self.evidence.mkdir(parents=True)
        self.payload = self.out / "payload"
        self.payload.mkdir()
        binaries = {}
        for name in release.BINARIES:
            path = self.payload / name
            path.write_text("#!/bin/sh\n# " + name + "\nexit 0\n")
            path.chmod(0o700)
            binaries[name] = {"bytes": path.stat().st_size, "sha256": release.digest(path)}
        self.built = {"source": release.source_identity(self.revision),
                      "environment": {"synthetic_only": True}, "binaries": binaries}
        release.save(self.evidence / "build.json", self.built)
        names = [name + "-help" for name in release.BINARIES] + [name for name, _ in release.SUITES]
        results = []
        for name in names:
            log = self.evidence / (name + ".log")
            log.write_text("synthetic successful receipt: " + name + "\n")
            results.append({"name": name, "exit_code": 0, "timeout": False,
                            "log": log.name, "sha256": release.digest(log)})
        self.verification = {"success": True, "source": self.built["source"],
                             "binary_sha256": {name: value["sha256"] for name, value in binaries.items()},
                             "results": results}
        self.save_verification()

    def tearDown(self):
        release.ROOT = self.old_root
        self.temporary.cleanup()

    def save_verification(self):
        release.save(self.evidence / "verification.json", self.verification)

    def package(self):
        with contextlib.redirect_stdout(io.StringIO()):
            release.package(self.out, self.evidence, self.revision)

    def test_successful_evidence_produces_only_exact_payload_and_bound_manifest(self):
        self.package()
        artifact = self.out / "artifact"
        provenance = json.loads((artifact / "provenance.json").read_text())
        archive = artifact / provenance["archive"]["name"]
        self.assertEqual(release.digest(archive), provenance["archive"]["sha256"])
        self.assertEqual(provenance["source"], self.built["source"])
        with tarfile.open(archive, "r:gz") as tar:
            self.assertEqual(tar.getnames(), list(release.BINARIES))
            for member in tar.getmembers():
                self.assertTrue(member.isfile())
                self.assertEqual(tar.extractfile(member).read(), (self.payload / member.name).read_bytes())
        for line in (artifact / "SHA256SUMS").read_text().splitlines():
            expected, name = line.split("  ")
            self.assertEqual(release.digest(artifact / name), expected)

    def test_missing_binary_record_is_rejected_even_when_names_exist(self):
        for names in ((), release.BINARIES[:2]):
            with self.subTest(names=names):
                malformed = dict(self.built, binaries={name: self.built["binaries"][name] for name in names})
                with self.assertRaisesRegex(RuntimeError, "产物记录范围改变"):
                    release.require_payload(self.out, malformed)

    def test_modified_binary_and_symbolic_link_are_rejected(self):
        path = self.payload / release.BINARIES[0]
        original = path.read_bytes()
        path.write_bytes(original + b"altered")
        with self.assertRaisesRegex(RuntimeError, "产物已改变"):
            release.require_payload(self.out, self.built)
        path.unlink()
        target = self.work / "replacement"
        target.write_bytes(original)
        target.chmod(0o700)
        path.symlink_to(target)
        with self.assertRaisesRegex(RuntimeError, "产物已改变"):
            release.require_payload(self.out, self.built)

    def test_missing_or_duplicate_suite_cannot_be_packaged(self):
        results = list(self.verification["results"])
        for invalid in (results[:-1], results + [results[-1]]):
            with self.subTest(length=len(invalid)):
                self.verification["results"] = invalid
                self.save_verification()
                with self.assertRaisesRegex(RuntimeError, "验收步骤缺失或重复"):
                    self.package()
                self.assertFalse((self.out / "artifact").exists())

    def test_failed_or_changed_log_is_not_success_evidence(self):
        record = self.verification["results"][0]
        record["exit_code"] = 1
        self.save_verification()
        with self.assertRaisesRegex(RuntimeError, "验收原日志不符"):
            self.package()
        record["exit_code"] = 0
        self.save_verification()
        (self.evidence / record["log"]).write_text("changed\n")
        with self.assertRaisesRegex(RuntimeError, "验收原日志不符"):
            self.package()
        self.assertFalse((self.out / "artifact").exists())

    def test_mismatched_source_and_lock_change_are_rejected(self):
        self.verification["source"] = dict(self.built["source"], tree="0" * 40)
        self.save_verification()
        with self.assertRaisesRegex(RuntimeError, "打包来源不一致"):
            self.package()
        (self.repo / "Cargo.lock").write_text("version = 3\n")
        with self.assertRaisesRegex(RuntimeError, "工作树不干净"):
            release.source_identity(self.revision)

    def test_real_command_exit_code_and_log_digest_are_bound(self):
        with contextlib.redirect_stdout(io.StringIO()):
            receipt = release.command(self.evidence, "real-failure", [sys.executable, "-c", "print('fixture');raise SystemExit(3)"])
        self.assertEqual(receipt["exit_code"], 3)
        self.assertEqual(release.digest(self.evidence / receipt["log"]), receipt["sha256"])
        self.assertFalse(receipt["timeout"])

    def test_timeout_stops_before_next_suite_even_when_child_survives(self):
        # 探针故意生成独立 session 的子进程，验证父超时不会被误报成全部已清理。
        # 测试结束按本测试持有的 PID 清理；真实 job 由整个容器退出收尾。
        pidfile = self.work / "owned-child.pid"
        marker = self.work / "owned-child.marker"
        child_code = "import pathlib,time;time.sleep(.3);pathlib.Path(" + repr(str(marker)) + ").write_text('alive');time.sleep(10)"
        parent_code = ("import subprocess,sys,pathlib,time;p=subprocess.Popen([sys.executable,'-c',"
                       + repr(child_code) + "],start_new_session=True);pathlib.Path(" + repr(str(pidfile))
                       + ").write_text(str(p.pid));time.sleep(10)")
        reached_next = False
        try:
            with self.assertRaises(TimeoutError), contextlib.redirect_stdout(io.StringIO()):
                release.command(self.evidence, "owned-timeout", [sys.executable, "-c", parent_code], timeout=.2)
                reached_next = True
            self.assertFalse(reached_next)
            self.assertTrue(json.loads((self.evidence / "owned-timeout-timeout.json").read_text())["timeout"])
            deadline = time.monotonic() + 2
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(.02)
            self.assertTrue(marker.exists(), "探针没有复现独立 session 子进程存活")
        finally:
            if pidfile.exists():
                with contextlib.suppress(ProcessLookupError):
                    os.kill(int(pidfile.read_text()), signal.SIGKILL)


if __name__ == "__main__":
    unittest.main()
