"""用实际 CLI 进程和本地 HTTP 替身评估受限内生执行；不评估模型语义质量。"""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time


ARTIFACT = {
    "summary": "这是固定的模拟反思：现有输入不足以确认现实任务完成。",
    "next_step": "请用户确认任务范围与验收条件。",
    "needs_user_input": True,
}
FIXTURE_KEY = "public-cognition-evaluation-fixture-key"


class EvaluationFailure(Exception):
    """只含公开检查名称，不带子进程日志、状态正文或请求正文。"""


def require(condition, check):
    if not condition:
        raise EvaluationFailure(check)


def response(text):
    return {
        "status": "completed", "error": None,
        "output": [{
            "type": "message", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }],
    }


class MockProvider:
    """每个场景独立端口；空回复队列也记录意外 HTTP 请求并返回错误。"""

    def __init__(self):
        self.requests = []
        self.errors = []
        self.replies = []
        self.lock = threading.Lock()
        self.received = threading.Event()
        self.release = threading.Event()
        fixture = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                self.connection.settimeout(5)
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    if not 0 < length <= 1024 * 1024:
                        raise ValueError("request-size")
                    body = json.loads(self.rfile.read(length))
                    with fixture.lock:
                        fixture.requests.append({
                            "path": self.path,
                            "authorized": self.headers.get("Authorization")
                            == "Bearer " + FIXTURE_KEY,
                            "body": body,
                        })
                        reply = fixture.replies.pop(0) if fixture.replies else None
                    fixture.received.set()
                    if reply is None:
                        code, data = 503, {"error": {"message": "unexpected fixture request"}}
                    else:
                        data, gated = reply
                        if gated:
                            fixture.release.wait(35)
                        code = 200
                    encoded = json.dumps(data, ensure_ascii=False).encode("utf-8")
                    self.send_response(code)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(encoded)))
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.wfile.write(encoded)
                except (BrokenPipeError, ConnectionResetError):
                    # 强制退出场景有意关闭仍在等待的客户端。
                    pass
                except Exception:
                    with fixture.lock:
                        fixture.errors.append("invalid-http-request")
                    fixture.received.set()

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_port}/v1/responses"
        self.thread = threading.Thread(
            target=self.server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True,
        )
        self.thread.start()

    def enqueue(self, text=None, *, body=None, gated=False):
        if body is None:
            body = response(text if text is not None else json.dumps(ARTIFACT, ensure_ascii=False))
        with self.lock:
            self.replies.append((body, gated))

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)


class Scenario:
    def __init__(self, binary, root):
        self.binary = binary
        self.root = root
        self.root.mkdir()
        self.agent = root / "AGENT.md"
        self.agent.write_text("你是 Eve。区分事实、反思草稿与现实任务完成。\n", encoding="utf-8")
        self.provider = MockProvider()
        self.live = set()
        self.processes = 0
        self.runs = []
        self.observations = {}

    def spawn(self, *args, credentials=True):
        # 覆盖模型配置并移除继承的 EVE 配置及代理，绝不读取现有凭据或真实状态目录。
        environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith("EVE_") and key.lower() not in {
                "http_proxy", "https_proxy", "all_proxy", "no_proxy",
            }
        }
        environment.update({
            "EVE_OPENAI_MODEL": "mock-reflection-evaluation",
            "EVE_OPENAI_PROTOCOL": "responses",
            "EVE_OPENAI_BASE_URL": self.provider.url,
            "EVE_OPENAI_TIMEOUT_SECONDS": "30",
            "EVE_OPENAI_MAX_OUTPUT_TOKENS": "256",
            "EVE_OPENAI_REASONING_EFFORT": "none",
            "EVE_LLM_RESPONSE_MODE": "complete",
            "NO_PROXY": "127.0.0.1,localhost",
        })
        if credentials:
            environment["EVE_OPENAI_API_KEY"] = FIXTURE_KEY
        process = subprocess.Popen(
            [str(self.binary), "--state-dir", str(self.root / "state"),
             "--agent", str(self.agent), *args],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=environment, cwd=self.root,
        )
        self.live.add(process)
        self.processes += 1
        return process

    def finish(self, process, command, *, timeout=15):
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate()
            raise EvaluationFailure(f"{command}:process-timeout") from None
        finally:
            self.live.discard(process)
        require(process.returncode == 0, f"{command}:process-exit")
        require(FIXTURE_KEY.encode() not in stdout + stderr, f"{command}:credential-redaction")
        try:
            report = json.loads(stdout)
        except (ValueError, UnicodeDecodeError):
            raise EvaluationFailure(f"{command}:json-report") from None
        require(isinstance(report, dict), f"{command}:object-report")
        return report

    def command(self, *args, credentials=False):
        return self.finish(self.spawn(*args, credentials=credentials), args[0])

    def add(self, identifier, *, user="owner", text="请为本地模拟待办整理验收条件。"):
        report = self.command("add", "--id", identifier, "--text", text, "--user", user)
        require(report["command"] == "add", "add:command")
        require(report["goal_id"] == identifier, "add:durable-identifier")

    def run(self, *, credentials=True):
        before = len(self.provider.requests)
        report = self.command(
            "run", "--seconds", "1", "--max-executions", "1", credentials=credentials,
        )
        self.record_run(report, before)
        return report

    def record_run(self, report, before):
        stats = report["loop"]
        observed = len(self.provider.requests) - before
        self.runs.append({
            "model_requests": stats["model_requests"],
            "observed_http_requests": observed,
            "submitted": stats["submitted"], "completed": stats["completed"],
            "blocked": stats["blocked"],
            "admitted_tool_calls": stats["admitted_tool_calls"],
            "started_tools": stats["started_tools"],
        })
        require(stats["model_requests"] == observed, "run:observed-http-count")
        require(observed <= 1 and stats["submitted"] <= 1, "run:startup-budget")
        require(stats["admitted_tool_calls"] == 0, "run:no-admitted-tools")
        require(stats["started_tools"] == 0, "run:no-started-tools")

    def show(self, identifier):
        return self.command("show", "--id", identifier)

    def state_bytes(self):
        return (self.root / "state/state.json").read_bytes()

    def snapshot(self):
        outer = json.loads(self.state_bytes())
        return json.loads(bytes(outer["entries"]["eve.cognition"]["cognition.v1"]))

    def check_requests(self):
        require(not self.provider.errors, "fixture:valid-http")
        for request in self.provider.requests:
            require(request["path"] == "/v1/responses", "fixture:responses-route")
            require(request["authorized"], "fixture:local-test-credential")
            body = request["body"]
            require(body["model"] == "mock-reflection-evaluation", "fixture:mock-model")
            require(body["tools"] == [], "fixture:no-advertised-tools")
            require(body["stream"] is False, "fixture:complete-response")
            require(body["max_output_tokens"] == 256, "fixture:output-budget")
        # FileStateStore 的值是字节数组，不能只在外层 JSON 文本中查密钥。
        outer = json.loads(self.state_bytes())
        for entries in outer["entries"].values():
            for value in entries.values():
                require(FIXTURE_KEY.encode() not in bytes(value), "state:no-credential")

    def close(self):
        for process in self.live:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=5)
        self.live.clear()
        self.provider.close()


def expect_idle(report):
    require(report["loop"]["submitted"] == 0, "idle:no-submission")
    require(report["loop"]["model_requests"] == 0, "idle:no-model-request")


def expect_parent(view, child_status):
    require(view["goal"]["status"] == "Waiting", "parent:still-waiting")
    require(view["goal"]["feedback"] is None, "parent:no-real-world-success-claim")
    reflections = view["reflections"]
    require(len(reflections) == 1, "parent:one-derived-reflection")
    child = reflections[0]["goal"]
    require(child["status"] == child_status, "child:expected-status")
    require(child["visibility"] == view["goal"]["visibility"], "child:visibility-inherited")
    require(child["budget"]["max_attempts"] == 1, "child:single-attempt")
    require(child["budget"]["max_model_requests"] == 1, "child:single-model-request")
    require(child["budget"]["max_tool_calls"] == 0, "child:no-tools")
    expected = ARTIFACT if child_status == "Completed" else None
    require(reflections[0]["artifact"] == expected, "child:verified-artifact-only")


def idle_without_credentials(case):
    report = case.run(credentials=False)
    expect_idle(report)
    require(report["goals"]["total"] == 0, "idle:no-goals-created")
    require(case.command("status")["goals"]["total"] == 0, "idle:status-empty")
    require(not case.provider.requests, "idle:no-http")
    case.observations["credentials_required"] = False


def durable_reflection(case):
    case.provider.enqueue()
    case.add("durable-parent")
    require(not case.provider.requests, "add:no-http")
    first = case.run()
    require(first["loop"]["completed"] == 1, "reflection:completed-draft")
    view = case.show("durable-parent")
    expect_parent(view, "Completed")
    persisted = case.state_bytes()
    expect_idle(case.run())
    require(case.show("durable-parent") == view, "restart:same-persisted-artifact")
    require(case.state_bytes() == persisted, "restart:no-state-rewrite")
    require(case.command("status")["goals"]["waiting"] == 1, "parent:waiting-count")
    case.observations.update({"parent_status": "Waiting", "child_status": "Completed",
                              "restart_replayed_requests": 0, "state_unchanged_after_restart": True})


def budget_and_user_scope(case):
    markers = ["EVAL_ALICE_ONLY", "EVAL_BOB_ONLY", "EVAL_CAROL_ONLY"]
    for index, user in enumerate(["alice", "bob", "carol"]):
        case.provider.enqueue()
        case.add(f"goal-{index}", user=user, text=f"{markers[index]}：列出验收前的问题。")
    for index in range(3):
        report = case.run()
        require(report["loop"]["completed"] == 1, "budget:one-completion-per-start")
        require(report["goals"]["completed"] == index + 1, "budget:cumulative-completions")
        require(report["goals"]["waiting"] == 3, "budget:parents-still-waiting")
    expect_idle(case.run())
    seen = []
    for request in case.provider.requests:
        text = json.dumps(request["body"], ensure_ascii=False)
        included = [marker for marker in markers if marker in text]
        require(len(included) == 1, "scope:one-user-goal-per-request")
        seen.extend(included)
    require(sorted(seen) == sorted(markers), "scope:all-authorized-goals-covered")
    for index in range(3):
        expect_parent(case.show(f"goal-{index}"), "Completed")
    case.observations.update({"waiting_parents": 3, "startup_execution_limit": 1,
                              "max_observed_requests_per_start": 1, "cross_user_marker_leaks": 0})


def malformed_artifacts(case):
    invalid = [
        "这不是 JSON",
        '{"summary":" ","next_step":"","needs_user_input":true}',
        '{"summary":"摘要","summary":"覆盖","next_step":"询问","needs_user_input":true}',
        '{"summary":"摘要","next_step":"询问","needs_user_input":true,"execute":true}',
    ]
    for index, text in enumerate(invalid):
        identifier = f"invalid-{index}"
        case.provider.enqueue(text)
        case.add(identifier)
        report = case.run()
        require(report["loop"]["blocked"] == 1, "invalid:blocked")
        require(report["loop"]["completed"] == 0, "invalid:no-completion")
        view = case.show(identifier)
        expect_parent(view, "Blocked")
        persisted = case.state_bytes()
        expect_idle(case.run())
        require(case.show(identifier) == view, "invalid:no-retry-after-restart")
        require(case.state_bytes() == persisted, "invalid:stable-restart-state")
    case.observations.update({"malformed_variants": len(invalid), "blocked_children": 4,
                              "restart_replayed_requests": 0})


def unauthorized_sources(case):
    case.add("wrong-channel")
    case.add("wrong-kind")
    # CLI 只创建已授权来源；此场景在停机的临时文件中预置其他来源。
    # 这是持久化恢复/执行过滤夹具，不代表磁盘篡改者不能伪造授权。
    outer = json.loads(case.state_bytes())
    inner = case.snapshot()
    goals = inner["state"]["goals"]
    goals["wrong-channel"]["source"]["channel"] = "evaluation.untrusted"
    goals["wrong-kind"]["source"]["kind"] = "Tool"
    for event in inner["state"]["events"]:
        if event["goal_id"] in goals:
            event["source"] = goals[event["goal_id"]]["source"].copy()
    outer["entries"]["eve.cognition"]["cognition.v1"] = list(
        json.dumps(inner, ensure_ascii=False).encode("utf-8"),
    )
    (case.root / "state/state.json").write_text(json.dumps(outer), encoding="utf-8")
    persisted = case.state_bytes()
    report = case.run(credentials=False)
    expect_idle(report)
    require(report["goals"]["total"] == 2, "source:no-derived-goals")
    require(case.state_bytes() == persisted, "source:no-state-rewrite")
    for identifier in goals:
        view = case.show(identifier)
        require(view["goal"]["status"] == "Waiting", "source:parent-retained")
        require(view["reflections"] == [], "source:no-reflection")
    case.observations.update({"unauthorized_parents": 2, "derived_goals": 0,
                              "observed_model_requests": 0})


def unsolicited_tool_call(case):
    case.provider.enqueue(body={
        "status": "completed", "error": None,
        "output": [{"type": "function_call", "call_id": "evaluation-forbidden-tool",
                    "name": "echo", "arguments": '{"text":"不得执行"}'}],
    })
    case.add("tool-parent")
    report = case.run()
    require(report["loop"]["blocked"] == 1, "tool:blocked")
    require(report["loop"]["completed"] == 0, "tool:no-completion")
    expect_parent(case.show("tool-parent"), "Blocked")
    expect_idle(case.run())
    require(len(case.provider.requests) == 1, "tool:no-follow-up-request")
    case.observations.update({"model_requested_tool_calls": 1, "started_tools": 0,
                              "follow_up_model_requests": 0, "child_status": "Blocked"})


def forced_exit_recovery(case):
    case.provider.enqueue(gated=True)
    case.add("interrupted-parent")
    process = case.spawn("run", "--seconds", "30", "--max-executions", "1")
    require(case.provider.received.wait(10), "crash:http-admission-gate")
    require(not case.provider.errors, "crash:valid-request")
    require(len(case.provider.requests) == 1, "crash:one-admitted-request")
    active = [goal for goal in case.snapshot()["state"]["goals"].values()
              if goal["status"] == "Executing"]
    require(len(active) == 1, "crash:durable-executing-before-kill")
    process.kill()
    process.communicate(timeout=5)
    case.live.discard(process)
    require(process.returncode != 0, "crash:forced-process-exit")
    case.provider.release.set()
    expect_idle(case.run())
    view = case.show("interrupted-parent")
    expect_parent(view, "Blocked")
    require(view["reflections"][0]["goal"]["block_reason"] == "Interrupted",
            "crash:interrupted-recovery-reason")
    persisted = case.state_bytes()
    expect_idle(case.run())
    require(case.state_bytes() == persisted, "crash:stable-recovered-state")
    require(len(case.provider.requests) == 1, "crash:no-replay")
    case.observations.update({"before_exit": "Executing", "recovered_child": "Blocked",
                              "block_reason": "Interrupted", "restart_replayed_requests": 0,
                              "forced_exit_returncode": process.returncode})


SCENARIOS = [
    ("idle_without_credentials", idle_without_credentials),
    ("durable_reflection", durable_reflection),
    ("startup_budget_and_user_scope", budget_and_user_scope),
    ("malformed_artifacts", malformed_artifacts),
    ("unauthorized_sources", unauthorized_sources),
    ("unsolicited_tool_call", unsolicited_tool_call),
    ("forced_exit_recovery", forced_exit_recovery),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path, help="已编译的 eve-cognition")
    parser.add_argument("--output", required=True, type=Path, help="结构化 JSON 结果路径")
    parser.add_argument("--source-sha", default="", help="构建者声明的源码版本，不自动推断二进制版本")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("--binary 必须指向可执行的 eve-cognition 文件")
    began = time.monotonic()
    results = []
    with tempfile.TemporaryDirectory(prefix="eve-cognition-evaluation-") as directory:
        for name, scenario in SCENARIOS:
            started = time.monotonic()
            case = Scenario(binary, Path(directory) / name)
            result = {"name": name, "status": "passed"}
            try:
                scenario(case)
                # 空闲实例可能从未写过 state.json。
                if (case.root / "state/state.json").exists():
                    case.check_requests()
                else:
                    require(not case.provider.requests, "empty-state:no-http")
                    require(not case.provider.errors, "fixture:valid-http")
            except EvaluationFailure as error:
                result.update(status="failed", failed_check=str(error))
            except Exception as error:
                result.update(status="failed", failed_check="harness-or-report-error",
                              error_type=type(error).__name__)
            finally:
                case.close()
            result.update({
                "elapsed_ms": round((time.monotonic() - started) * 1000, 1),
                "processes": case.processes,
                "observed_http_requests": len(case.provider.requests),
                "run_metrics": case.runs, "observations": case.observations,
            })
            results.append(result)
    passed = sum(item["status"] == "passed" for item in results)
    with binary.open("rb") as stream:
        binary_sha256 = hashlib.file_digest(stream, "sha256").hexdigest()
    report = {
        "format_version": 1,
        "evaluation": "bounded-endogenous-process-behavior",
        "provider": "mock-loopback-http-responses",
        "real_model_evaluated": False,
        "semantic_quality_evaluated": False,
        "declared_source_sha": args.source_sha,
        "binary_sha256": binary_sha256,
        "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "functional_ok": passed == len(results),
        "summary": {
            "scenarios": len(results), "passed": passed, "failed": len(results) - passed,
            "processes": sum(item["processes"] for item in results),
            "observed_http_requests": sum(item["observed_http_requests"] for item in results),
            "elapsed_ms": round((time.monotonic() - began) * 1000, 1),
        },
        "scenarios": results,
        "not_measured": [
            "反思建议的事实正确性、规划质量和跨任务泛化",
            "自创目标的有效性与多步现实任务完成能力",
            "真实模型、QQ、外部工具、PostgreSQL 和学习效果",
        ],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"functional_ok": report["functional_ok"], **report["summary"]},
                     ensure_ascii=False))
    return 0 if report["functional_ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
