"""Release Eve + production QQ bridge performance; all inputs and peers are local."""
import argparse
import json
import math
import os
import pathlib
import platform
import resource
import statistics
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[3]
HERE = pathlib.Path(__file__).resolve().parent


def require(condition, diagnostic):
    if not condition:
        raise RuntimeError(diagnostic)


def percentile(values, fraction):
    ordered = sorted(values)
    require(bool(ordered), "empty_latency_sample")
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def aggregate(runs):
    latencies = [value for run in runs for value in run["latency_ms"]]
    report = {
        "runs": len(runs),
        "messages": sum(run["messages"] for run in runs),
        "median_messages_per_second": statistics.median(run["messages_per_second"] for run in runs),
        "median_elapsed_ms": statistics.median(run["elapsed_ms"] for run in runs),
        "p50_ms": percentile(latencies, 0.50),
        "p95_ms": percentile(latencies, 0.95),
        "p99_ms": percentile(latencies, 0.99),
        "max_ms": max(latencies),
        "peak_rss_mib": max(run["rss_mib"] for run in runs),
        "median_cpu_ms": statistics.median(run["cpu_ms"] for run in runs),
    }
    if "startup_to_first_request_ms" in runs[0]:
        report["median_startup_to_first_request_ms"] = statistics.median(
            run["startup_to_first_request_ms"] for run in runs)
        report["node_peak_rss_mib"] = max(run["node_peak_rss_mib"] for run in runs)
        report["median_state_bytes"] = statistics.median(run["state_bytes"] for run in runs)
        report["provider_requests"] = sum(run["provider_requests"] for run in runs)
        report["tool_roundtrips"] = sum(run["tool_roundtrips"] for run in runs)
    return {key: round(value, 4) if isinstance(value, float) else value for key, value in report.items()}


class LocalProvider:
    def __init__(self):
        self.requests = []
        self.errors = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            disable_nagle_algorithm = True

            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    require(self.path == "/v1/chat/completions", "wrong_endpoint")
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    require(body["model"] == "deepseek-v4.1-flash", "wrong_model")
                    require(body["reasoning_effort"] == "none", "wrong_reasoning")
                    messages = body["messages"]
                    outer.requests.append({
                        "at": time.perf_counter(),
                        "user_count": sum(message["role"] == "user" for message in messages),
                        "tool": messages[-1]["role"] == "tool",
                    })
                    latest = messages[-1]
                    reason = "stop"
                    if latest["role"] == "tool":
                        message = {"role": "assistant", "content": json.loads(latest["content"])["echo"]}
                    elif latest["content"].startswith("echo:"):
                        marker = latest["content"][5:]
                        reason = "tool_calls"
                        message = {"role": "assistant", "content": None, "tool_calls": [{
                            "id": "perf-call-" + str(len(outer.requests)), "type": "function",
                            "function": {"name": "echo", "arguments": json.dumps({"text": marker})},
                        }]}
                    else:
                        message = {"role": "assistant", "content": latest["content"]}
                    encoded = json.dumps({"choices": [{
                        "index": 0, "finish_reason": reason, "message": message,
                    }]}).encode()
                    self.send_response(200)
                except Exception:
                    outer.errors.append("invalid_local_request")
                    encoded = b'{"error":"invalid_local_request"}'
                    self.send_response(500)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


def run_wave(binary, provider, directory, count, initial_turns, payload_bytes):
    phase = "fresh" if initial_turns == 0 else "restored"
    messages = []
    for index in range(initial_turns, initial_turns + count):
        marker = ("perf." + str(index) + ".").ljust(payload_bytes, "x")
        text = "echo:" + marker if index % 10 == 0 else marker
        messages.append({"id": "ROBOT1.0_perf." + str(index) + "!", "text": text, "expected": marker})
    metrics_path = directory / (phase + "-metrics.json")
    scenario = directory / (phase + "-scenario.json")
    scenario.write_text(json.dumps({"messages": messages, "metrics_path": str(metrics_path)}), encoding="utf8")
    env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
    env.update(
        QQBOT_APP_SECRET="test-app-secret",
        EVE_OPENAI_API_KEY="test-model-secret",
        EVE_OPENAI_BASE_URL="http://127.0.0.1:" + str(provider.server.server_port),
        EVE_OPENAI_MODEL="deepseek-v4.1-flash",
        EVE_OPENAI_PROTOCOL="chat",
        EVE_OPENAI_REASONING_EFFORT="none",
        EVE_LLM_RESPONSE_MODE="complete",
    )
    command = [str(binary), "--state-dir", str(directory / "state"),
               "--agent", str(ROOT / "AGENT.md"), "--bridge-script", str(HERE / "performance-bridge.mjs"),
               "--bridge-arg", str(scenario)]
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    request_start = len(provider.requests)
    peak = [0.0]
    stopped = threading.Event()
    started = time.perf_counter()
    child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def sample_memory():
        while not stopped.is_set():
            try:
                for line in pathlib.Path("/proc/" + str(child.pid) + "/status").read_text().splitlines():
                    if line.startswith(("VmRSS:", "VmHWM:")):
                        peak[0] = max(peak[0], int(line.split()[1]) / 1024)
            except FileNotFoundError:
                pass
            stopped.wait(0.005)

    sampler = threading.Thread(target=sample_memory, daemon=True)
    sampler.start()
    try:
        stdout, stderr = child.communicate(timeout=120)
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        stopped.set()
        sampler.join()
    elapsed_ms = (time.perf_counter() - started) * 1000
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    require(child.returncode == 0, "eve_process_failed")
    require("test-app-secret" not in stdout + stderr and "test-model-secret" not in stdout + stderr,
            "test_credentials_leaked")
    summary = json.loads(stdout)
    require(summary["ready"] and summary["closed"] and not summary["terminal_error"], "invalid_lifecycle")
    require((summary["received"], summary["completed"], summary["sent"], summary["failed"]) ==
            (count, count, count, 0), "lost_or_failed_messages")
    requests = provider.requests[request_start:]
    tool_count = sum(message["text"].startswith("echo:") for message in messages)
    require(not provider.errors and len(requests) == count + tool_count, "wrong_model_request_count")
    require(sum(request["tool"] for request in requests) == tool_count, "wrong_tool_result_count")
    require(requests[0]["user_count"] == initial_turns + 1, "history_not_restored")
    metrics = json.loads(metrics_path.read_text())
    latencies = metrics["latency_ms"]
    require(len(latencies) == count and all(math.isfinite(value) and value >= 0 for value in latencies),
            "invalid_latency_metrics")
    state_path = directory / "state/state.json"
    state = json.loads(state_path.read_text())
    ledger = json.loads(bytes(state["entries"]["eve.channel.qqbot"]["receipts.v1"]))
    require(len(ledger["entries"]) == initial_turns + count, "wrong_receipt_count")
    require(all(entry["state"] == "Sent" for entry in ledger["entries"]), "uncommitted_delivery")
    by_id = {entry["message"]["id"]: entry["reply"] for entry in ledger["entries"]}
    require(all(by_id[message["id"]] == message["expected"] for message in messages), "wrong_saved_reply")
    sessions = json.loads(bytes(state["entries"]["eve.session"]["sessions.v1"]))["sessions"]
    require(len(sessions) == 1, "unexpected_session_split")
    session = next(iter(sessions.values()))
    require(session["revision"] == 2 * (initial_turns + count), "wrong_session_revision")
    require(len(session["turns"]) == initial_turns + count and
            all(turn["status"]["state"] == "Completed" for turn in session["turns"]),
            "uncommitted_session")
    require(peak[0] > 0, "memory_sampling_failed")
    return {
        "messages": count, "latency_ms": latencies, "elapsed_ms": elapsed_ms,
        "messages_per_second": count * 1000 / elapsed_ms,
        "cpu_ms": ((after.ru_utime + after.ru_stime) - (before.ru_utime + before.ru_stime)) * 1000,
        "rss_mib": peak[0], "node_peak_rss_mib": metrics["node_peak_rss_mib"],
        "startup_to_first_request_ms": (requests[0]["at"] - started) * 1000,
        "state_bytes": state_path.stat().st_size, "provider_requests": len(requests), "tool_roundtrips": tool_count,
    }


def check_budgets(metrics, limits):
    warnings = []
    checks = [
        ("node_bridge", "p95_ms", "node_p95_ms", "max"),
        ("node_bridge", "median_messages_per_second", "node_min_messages_per_second", "min"),
        ("node_bridge", "peak_rss_mib", "node_peak_rss_mib", "max"),
    ]
    for name in ("eve_fresh", "eve_restored"):
        checks.extend([
            (name, "p95_ms", "eve_p95_ms", "max"),
            (name, "median_messages_per_second", "eve_min_messages_per_second", "min"),
            (name, "peak_rss_mib", "eve_peak_rss_mib", "max"),
            (name, "node_peak_rss_mib", "node_peak_rss_mib", "max"),
            (name, "median_startup_to_first_request_ms", "eve_startup_to_first_request_ms", "max"),
        ])
    for name, key, limit_key, direction in checks:
        value, limit = metrics[name][key], limits[limit_key]
        if (direction == "max" and value > limit) or (direction == "min" and value < limit):
            warnings.append({"case": name, "metric": key, "value": value, "budget": limit, "direction": direction})
    return warnings


def publish(report, output):
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf8")
    print(json.dumps(report))
    for warning in report.get("warnings", []):
        print("::warning title=性能预算超限::" + warning["case"] + "." + warning["metric"] +
              " = " + str(warning["value"]) + "; budget = " + str(warning["budget"]))
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        rows = ["## Eve 性能测试", "", "模型与 QQ 为本地替身；预热后测量 Release 版运行时及持久化开销。",
                "", "| 场景 | 消息数 | 中位吞吐 msg/s | p50 ms | p95 ms | p99 ms | 峰值 RSS MiB |",
                "| --- | ---: | ---: | ---: | ---: | ---: | ---: |"]
        for name, case in report.get("metrics", {}).items():
            rows.append("| " + name + " | " + " | ".join(str(case[key]) for key in
                        ("messages", "median_messages_per_second", "p50_ms", "p95_ms", "p99_ms", "peak_rss_mib")) + " |")
        rows.extend(["", "功能正确性：" + str(report["functional_ok"]) +
                     "；性能预算：" + report["performance_status"] + "。", ""])
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf8") as summary:
            summary.write("\n".join(rows))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=pathlib.Path, default=ROOT / "target/release/eve-qqbot")
    parser.add_argument("--budget", type=pathlib.Path, default=HERE / "performance-budget.json")
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    report = {"format_version": 1, "functional_ok": False, "performance_status": "failed",
              "provider": "loopback_chat_stub", "provider_tcp_nodelay": True, "qq_transport": "production_bridge_with_sdk_stub",
              "build_profile": "release", "platform": platform.platform(),
              "python_version": platform.python_version(), "logical_cpus": os.cpu_count(),
              "source_sha": os.environ.get("GITHUB_SHA", "local")}
    provider = None
    try:
        budget = json.loads(args.budget.read_text())
        require(budget["format_version"] == 1, "wrong_budget_version")
        node = subprocess.run(["node", str(HERE / "bridge_performance.mjs"), str(args.budget.resolve())],
                              check=True, capture_output=True, text=True, timeout=60)
        node_report = json.loads(node.stdout)
        report["node_version"] = node_report["node_version"]
        provider = LocalProvider()
        fresh, restored = [], []
        with tempfile.TemporaryDirectory(prefix="eve-performance-") as temporary:
            work = pathlib.Path(temporary)
            warmup = work / "warmup"
            warmup.mkdir()
            run_wave(args.binary.resolve(), provider, warmup, budget["eve"]["warmup_messages"], 0,
                     budget["payload_bytes"])
            for index in range(budget["eve"]["samples"]):
                directory = work / ("sample-" + str(index))
                directory.mkdir()
                count = budget["eve"]["messages"]
                fresh.append(run_wave(args.binary.resolve(), provider, directory, count, 0, budget["payload_bytes"]))
                restored.append(run_wave(args.binary.resolve(), provider, directory, count, count, budget["payload_bytes"]))
        report["workload"] = budget
        report["metrics"] = {"node_bridge": aggregate(node_report["samples"]),
                             "eve_fresh": aggregate(fresh), "eve_restored": aggregate(restored)}
        report["warnings"] = check_budgets(report["metrics"], budget["budgets"])
        report["functional_ok"] = True
        report["performance_status"] = "warning" if report["warnings"] else "passed"
    except Exception:
        report["diagnostic"] = "performance_validation_failed"
        publish(report, args.output)
        raise SystemExit("性能测试的消息、工具、状态或生命周期校验失败；未通过验收。")
    finally:
        if provider:
            provider.close()
    publish(report, args.output)


if __name__ == "__main__":
    main()
