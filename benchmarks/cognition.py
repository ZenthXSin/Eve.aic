"""认知循环的实际 Release 子进程正确性与性能检查，不需要外部凭据。"""
import argparse
import json
import math
import os
from pathlib import Path
import resource
import subprocess
import tempfile
import time


def run(binary, directory, mode):
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.perf_counter()
    process = subprocess.Popen(
        [binary, str(directory), mode],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8",
    )
    peak_rss_kib = 0
    while process.poll() is None:
        if time.perf_counter() - started > 15:
            process.kill()
            process.communicate()
            raise RuntimeError("认知进程超时，正确性检查失败")
        try:
            for line in Path(f"/proc/{process.pid}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    peak_rss_kib = max(peak_rss_kib, int(line.split()[1]))
        except FileNotFoundError:
            pass
        time.sleep(0.005)
    stdout, stderr = process.communicate()
    if process.returncode:
        raise RuntimeError(f"认知子进程失败：{mode}，退出码 {process.returncode}")
    report = json.loads(stdout.strip())
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    report["elapsed_ms"] = (time.perf_counter() - started) * 1000
    report["cpu_ms"] = ((after.ru_utime + after.ru_stime) -
                        (before.ru_utime + before.ru_stime)) * 1000
    report["peak_rss_mib"] = peak_rss_kib / 1024
    if not report["old_services_closed"] or not report["directory_reopened"]:
        raise RuntimeError("认知句柄或目录锁没有收尾")
    return report


def percentile(values, quantile):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * quantile) - 1)]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    active, idle, cancelled = [], [], []
    with tempfile.TemporaryDirectory(prefix="eve-cognition-performance-") as temporary:
        root = Path(temporary)
        for index in range(5):
            directory = root / f"cycle-{index}"
            seed = run(binary, directory, "seed")
            if (seed["revision"], seed["status"], seed["model_requests"], seed["tool_executions"]) != (1, "Ready", 0, 0):
                raise RuntimeError("Ready 目标没有独立持久化")
            result = run(binary, directory, "run")
            if (result["restored_revision"], result["revision"], result["status"],
                    result["model_requests"], result["tool_executions"],
                    result["admitted_tool_calls"], result["completed"]) != (1, 3, "Completed", 2, 1, 1, 1):
                raise RuntimeError("无输入执行或反馈验证失败")
            original = (directory / "state.json").read_bytes()
            resumed = run(binary, directory, "run")
            if (resumed["restored_revision"], resumed["revision"], resumed["status"],
                    resumed["model_requests"], resumed["tool_executions"]) != (3, 3, "Completed", 0, 0):
                raise RuntimeError("完成目标发生重放")
            if resumed["idle_ticks"] < 1 or original != (directory / "state.json").read_bytes():
                raise RuntimeError("空闲恢复产生请求或改写状态")
            cancellation = run(binary, root / f"cancel-{index}", "cancel")
            if (cancellation["status"], cancellation["model_requests"],
                    cancellation["tool_executions"], cancellation["tool_drops"],
                    cancellation["cancelled"]) != ("Cancelled", 1, 1, 1, 1):
                raise RuntimeError("取消没有等待实际工具和会话收尾")
            active.append(result)
            idle.append(resumed)
            cancelled.append(cancellation)
    metrics = {
        "samples": 5,
        "wakeup_to_admission_p95_ms": percentile([r["wakeup_to_admission_us"] / 1000 for r in active], 0.95),
        "cancel_settle_p95_ms": percentile([r["cancel_settle_us"] / 1000 for r in cancelled], 0.95),
        "restore_p95_ms": percentile([r["restore_us"] / 1000 for r in idle], 0.95),
        "active_process_p95_ms": percentile([r["elapsed_ms"] for r in active], 0.95),
        "idle_process_cpu_p95_ms": percentile([r["cpu_ms"] for r in idle], 0.95),
        "peak_rss_mib": max(r["peak_rss_mib"] for r in active + idle + cancelled),
        "idle_model_requests": sum(r["model_requests"] for r in idle),
        "idle_tool_executions": sum(r["tool_executions"] for r in idle),
    }
    budgets = {
        "wakeup_to_admission_p95_ms": 500,
        "cancel_settle_p95_ms": 1000,
        "restore_p95_ms": 5000,
        "peak_rss_mib": 512,
    }
    warnings = [f"{name}={metrics[name]:.3f} 超过预算 {limit}"
                for name, limit in budgets.items() if metrics[name] > limit]
    report = {
        "format_version": 1, "functional_ok": True,
        "performance_status": "warning" if warnings else "passed",
        "source_sha": os.environ.get("GITHUB_SHA", ""),
        "provider": "loopback_chat", "build_profile": "release",
        "metrics": metrics, "budgets": budgets, "warnings": warnings,
    }
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    for warning in warnings:
        print("::warning::" + warning)
    print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    main()
