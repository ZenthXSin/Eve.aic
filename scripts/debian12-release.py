#!/usr/bin/env python3
"""远端固定 Debian 12 发布构建：来源、真实进程验收与可核验的小型产物。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import time


ROOT = Path(__file__).resolve().parents[1]
IMAGE = "rust:1.95.0-bookworm@sha256:4c2fd73ef19c5ef9d54bee03b06b2839a392604fbfcd578ed948b71b37c1d7fb"
TARGET = "x86_64-unknown-linux-gnu"
BINARIES = ("eve-qqbot", "eve-web", "eve-memory")
NODE_SHA256 = "9aa8e9d2298ab68c600bd6fb86a6c13bce11a4eca1ba9b39d79fa021755d7c37"
QQ = "connectors/qqbot/test/"
SUITES = (
    ("qq-adapter", QQ + "model_adapter_test.py"),
    ("web-adapter", "scripts/test-web-model-adapter.py"),
    ("free-exploration", QQ + "free_exploration_test.py"),
    ("live-upgrade", QQ + "live_upgrade_test.py"),
    ("deferred-reply", QQ + "deferred_reply_test.py"),
    ("compaction-stream", QQ + "compaction_stream_test.py"),
    ("cache-metrics", QQ + "cache_metrics_test.py"),
    ("request-timing", QQ + "request_timing_test.py"),
    ("owner-account", QQ + "owner_account_test.py"),
    ("memory-capacity", QQ + "memory_capacity_test.py"),
    ("admin", QQ + "admin_test.py"),
    ("trusted-originals", QQ + "trusted_review_originals_test.py"),
    ("receipt-capacity", QQ + "receipt_capacity_test.py"),
    ("source-lookup", QQ + "source_lookup_test.py"),
    ("business-restrictions", QQ + "business_restriction_ingress_test.py"),
    ("live-dialogue", QQ + "live_dialogue_test.py"),
    ("natural-progress", QQ + "natural_progress_test.py"),
    ("subagent-records", QQ + "subagent_profiles_test.py"),
    ("conversation-learning", QQ + "conversation_learning_test.py"),
    ("conversation-history-maintenance", QQ + "conversation_history_maintenance_test.py"),
    ("message-learning-flow", QQ + "message_learning_flow_test.py"),
    ("speech-event-context", QQ + "speech_event_context_test.py"),
    ("health-watch", QQ + "health_watch_test.py"),
    ("wakeup-silence", QQ + "wakeup_silence_test.py"),
    ("startup-checkpoint", QQ + "startup_checkpoint_test.py"),
    ("group-live-anchor", QQ + "group_live_anchor_test.py"),
    ("phrases", QQ + "phrases_test.py"),
    ("background-recovery", QQ + "background_recovery_test.py"),
    ("group-wakeup", QQ + "group_wakeup_test.py"),
    ("runtime-recovery", "scripts/test-self-upgrade-runtime.py"),
    ("host-handoff", "scripts/test-host-runtime-bridge.py"),
)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def read(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True, timeout=30).strip()


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf8")


def source_identity(revision):
    require(re.fullmatch(r"[0-9a-f]{40}", revision), "必须显式提供完整源码提交")
    require(read("git", "rev-parse", "HEAD") == revision, "源码提交不符")
    require(not read("git", "status", "--porcelain", "--untracked-files=all"), "源码工作树不干净")
    return {
        "revision": revision,
        "tree": read("git", "rev-parse", "HEAD^{tree}"),
        "cargo_lock_sha256": digest(ROOT / "Cargo.lock"),
        "codex_manifest_sha256": digest(ROOT / "vendor/codex-0.162.0/UPSTREAM_FILES.json"),
    }


def environment():
    require(os.environ.get("GITHUB_ACTIONS") == "true" and Path("/.dockerenv").is_file(),
            "此入口仅用于远端容器，不允许在生产宿主机编译")
    require(os.environ.get("EVE_RELEASE_IMAGE") == IMAGE, "构建镜像未固定")
    os_release = dict(line.split("=", 1) for line in Path("/etc/os-release").read_text().splitlines() if "=" in line)
    require(os_release.get("ID") == "debian" and os_release.get("VERSION_ID", "").strip('"') == "12",
            "构建系统必须为 Debian 12")
    rustc, cargo = read("rustc", "-Vv"), read("cargo", "-Vv")
    require("release: 1.95.0\n" in rustc + "\n" and "host: " + TARGET in rustc,
            "Rust 版本或架构不符")
    require(cargo.startswith("cargo 1.95.0 "), "Cargo 版本不符")
    require(read("node", "--version") == "v22.22.0", "Node 版本不符")
    require(read("getconf", "GNU_LIBC_VERSION") == "glibc 2.36", "glibc 版本不符")
    require(os.environ.get("CARGO_INCREMENTAL") == "0"
            and os.environ.get("CARGO_PROFILE_RELEASE_DEBUG") == "0", "构建配置不符")
    require(not any(os.environ.get(key) for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER")),
            "不允许隐藏的编译选项")
    return {"image": IMAGE, "target": TARGET, "rustc": rustc, "cargo": cargo,
            "glibc": "2.36", "node": "22.22.0", "node_archive_sha256": NODE_SHA256,
            "cargo_incremental": False, "release_debug": False}


def command(evidence, name, args, *, env=None, timeout=600):
    path = evidence / (name + ".log")
    started = time.monotonic()
    print("EVE_RELEASE_BEGIN " + name, flush=True)
    with path.open("w", encoding="utf8") as stream:
        try:
            result = subprocess.run(args, cwd=ROOT, env=env, stdout=stream, stderr=subprocess.STDOUT,
                                    timeout=timeout, check=False)
            code, timed_out = result.returncode, False
        except subprocess.TimeoutExpired:
            code, timed_out = None, True
    record = {"name": name, "command": args, "exit_code": code, "timeout": timed_out,
              "seconds": round(time.monotonic() - started, 3), "log": path.name, "sha256": digest(path)}
    print("EVE_RELEASE_RESULT " + json.dumps(record, ensure_ascii=False), flush=True)
    # 原日志完整保留；只把尾部送到 Action 界面，避免重复输出大段构建日志。
    with path.open("rb") as stream:
        stream.seek(max(0, path.stat().st_size - 5000))
        print(stream.read().decode("utf8", errors="replace"), flush=True)
    if timed_out:
        # subprocess.run 只保证终止直接子进程；监督器可创建独立 session。
        # 不在可能仍有孤儿进程时继续其它组，由本次失败作业的容器退出统一收尾。
        save(evidence / (name + "-timeout.json"), record)
        raise TimeoutError("验收超时，停止后续执行并保留容器日志：" + name)
    return record


def build(out, evidence, revision):
    initial = source_identity(revision)
    env = environment()
    save(evidence / "source-before.json", initial)
    save(evidence / "build-environment.json", env)
    upstream = command(evidence, "codex-original-files", [sys.executable, "-B", "scripts/verify-codex-upstream.py"])
    require(upstream["exit_code"] == 0, "Codex 原文件来源校验失败")
    args = ["cargo", "build", "-p", "eve-app", "--release", "--locked", "--target", TARGET]
    for name in BINARIES:
        args.extend(["--bin", name])
    result = command(evidence, "build", args, timeout=2700)
    save(evidence / "build-result.json", result)
    require(result["exit_code"] == 0, "生产入口构建失败")
    require(source_identity(revision) == initial, "构建改写了源码或依赖锁")
    payload = out / "payload"
    payload.mkdir()
    binaries = {}
    for name in BINARIES:
        src = ROOT / "target" / TARGET / "release" / name
        dest = payload / name
        shutil.copy2(src, dest)
        require(not src.is_symlink() and digest(src) == digest(dest), "构建产物复制不一致")
        binaries[name] = {"bytes": dest.stat().st_size, "sha256": digest(dest)}
    save(evidence / "build.json", {"source": initial, "environment": env, "binaries": binaries})


def require_payload(out, build_record):
    files = out / "payload"
    require(set(build_record["binaries"]) == set(BINARIES), "产物记录范围改变")
    require(set(path.name for path in files.iterdir()) == set(BINARIES), "产物文件范围改变")
    for name, info in build_record["binaries"].items():
        path = files / name
        require(not path.is_symlink() and path.is_file() and os.access(path, os.X_OK)
                and path.stat().st_size == info["bytes"] and digest(path) == info["sha256"],
                "产物已改变：" + name)


def verify(out, evidence, revision):
    built = json.loads((evidence / "build.json").read_text())
    require(source_identity(revision) == built["source"] and environment() == built["environment"],
            "构建和验收的来源或环境不一致")
    require_payload(out, built)
    results, elf = [], {}
    for name in BINARIES:
        path = str(out / "payload" / name)
        header = read("readelf", "-h", path)
        require("ELF64" in header and "Advanced Micro Devices X86-64" in header, "ELF 架构不符")
        dependencies = read("ldd", path)
        require("not found" not in dependencies, "动态依赖缺失：" + name)
        versions = read("readelf", "--version-info", path)
        glibc = sorted({tuple(map(int, item.split("."))) for item in re.findall(r"\bGLIBC_([0-9.]+)", versions)})
        require(glibc and glibc[-1] <= (2, 36), "二进制需要更新的 glibc：" + name)
        elf[name] = {"file": read("file", path), "header": header, "dependencies": dependencies,
                     "dynamic": read("readelf", "-d", path), "version_info": versions,
                     "required_glibc_versions": [".".join(map(str, value)) for value in glibc]}
        results.append(command(evidence, name + "-help", [path, "--help"], timeout=30))
    save(evidence / "elf.json", elf)
    # memory_capacity_test 按 QQ 二进制的同目录寻找 eve-memory。
    env = dict(os.environ, EVE_QQBOT_BINARY=str(out / "payload/eve-qqbot"),
               EVE_WEB_BINARY=str(out / "payload/eve-web"))
    for name, script in SUITES:
        # 用例内部期限不变；某组失败后继续收集其它独立组，最终整体失败且不打包。
        # 自然进度原整组已实测 575.6 秒，新增待消费输入用例还需真实等待约 70 秒。
        # 只增加该组总预算，不缩短平台窗口/冷却或放宽其他组和单个用例的期限。
        results.append(command(evidence, name, [sys.executable, "-B", script], env=env,
                               timeout=900 if name == "natural-progress" else 600))
    final_source = source_identity(revision)
    require(final_source == built["source"], "验收改写了源码或依赖锁")
    require_payload(out, built)
    success = all(result["exit_code"] == 0 and not result["timeout"] for result in results)
    save(evidence / "verification.json", {"success": success, "source": final_source,
                                         "binary_sha256": {name: info["sha256"] for name, info in built["binaries"].items()},
                                         "results": results, "real_model_or_qq": False,
                                         "process_suites": [name for name, _ in SUITES if name not in
                                                            ("runtime-recovery", "host-handoff")],
                                         "python_host_fixture_suites": ["runtime-recovery", "host-handoff"]})
    require(success, "生产二进制验收失败；日志保留，不生成可发布产物")


def package(out, evidence, revision):
    built = json.loads((evidence / "build.json").read_text())
    verified = json.loads((evidence / "verification.json").read_text())
    require(source_identity(revision) == built["source"] == verified["source"], "打包来源不一致")
    require(verified["success"] is True, "缺少成功的实际进程验收")
    require(verified["binary_sha256"] == {name: info["sha256"] for name, info in built["binaries"].items()},
            "验收的二进制与构建不一致")
    expected = [name + "-help" for name in BINARIES] + [name for name, _ in SUITES]
    require([item["name"] for item in verified["results"]] == expected, "验收步骤缺失或重复")
    for item in verified["results"]:
        require(item["exit_code"] == 0 and not item["timeout"]
                and digest(evidence / item["log"]) == item["sha256"], "验收原日志不符")
    require_payload(out, built)
    artifact = out / "artifact"
    artifact.mkdir()
    archive = artifact / ("eve-debian12-x64-" + revision + ".tar.gz")
    with tarfile.open(archive, "w:gz", compresslevel=6) as tar:
        for name in BINARIES:
            tar.add(out / "payload" / name, arcname=name, recursive=False)
    manifest = {"format_version": 1, **built, "verification": verified,
                "archive": {"name": archive.name, "bytes": archive.stat().st_size, "sha256": digest(archive)},
                "evidence_sha256": {path.name: digest(path) for path in sorted(evidence.iterdir()) if path.is_file()},
                "github": {key: os.environ.get(key) for key in
                           ("GITHUB_REPOSITORY", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_WORKFLOW_REF")}}
    save(artifact / "provenance.json", manifest)
    (artifact / "SHA256SUMS").write_text(
        "".join(digest(path) + "  " + path.name + "\n" for path in (archive, artifact / "provenance.json")))
    print(json.dumps({"packaged": True, "revision": revision, "archive": manifest["archive"]}, ensure_ascii=False))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("build", "verify", "package"))
    args = parser.parse_args()
    revision = os.environ.get("EVE_RELEASE_REVISION", "")
    output = os.environ.get("EVE_RELEASE_OUTPUT", "")
    require(output and Path(output).is_absolute(), "需要独立的绝对路径输出目录")
    out = Path(output).resolve()
    require(not out.is_relative_to(ROOT) and out != Path("/"), "输出目录不能位于源码树")
    evidence = out / "evidence"
    evidence.mkdir(parents=True, exist_ok=True)
    {"build": build, "verify": verify, "package": package}[args.action](out, evidence, revision)


if __name__ == "__main__":
    main()
