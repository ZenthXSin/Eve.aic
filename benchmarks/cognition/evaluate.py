"""用实际 CLI 进程和本地 HTTP 替身评估受限内生执行；不评估模型语义质量。"""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import signal
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

    def run(self, *, credentials=True, expected_terminal=None):
        before = len(self.provider.requests)
        if expected_terminal is None:
            # 空闲场景没有模型完成门槛，窗口结束报告用于检查零提交。
            report = self.command(
                "run", "--seconds", "1", "--max-executions", "1", credentials=credentials,
            )
        else:
            previous = {
                identifier for identifier, goal in self.snapshot()["state"]["goals"].items()
                if goal["status"] == expected_terminal
            }
            process = self.spawn(
                "run", "--seconds", "30", "--max-executions", "1", credentials=credentials,
            )
            deadline = time.monotonic() + 12
            while True:
                require(process.poll() is None, "gate:process-alive-until-durable-terminal")
                completed = [goal for identifier, goal in self.snapshot()["state"]["goals"].items()
                             if identifier not in previous and goal["status"] == expected_terminal]
                require(len(completed) <= 1, "gate:no-extra-terminal-goals")
                if completed:
                    break
                require(time.monotonic() < deadline, "gate:durable-terminal-timeout")
                time.sleep(0.01)
            process.send_signal(signal.SIGINT)
            report = self.finish(process, "run")
        self.record_run(report, before)
        return report

    def record_run(self, report, before, *, execution_limit=1):
        stats = report["loop"]
        observed = len(self.provider.requests) - before
        metrics = {
            "model_requests": stats["model_requests"],
            "observed_http_requests": observed,
            "submitted": stats["submitted"], "completed": stats["completed"],
            "blocked": stats["blocked"],
            "admitted_tool_calls": stats["admitted_tool_calls"],
            "started_tools": stats["started_tools"],
        }
        if report.get("observation") is not None:
            metrics["file_observation"] = {
                key: report["observation"][key] for key in ("reads", "saved", "duplicates")
            }
        self.runs.append(metrics)
        require(stats["model_requests"] == observed, "run:observed-http-count")
        require(observed <= execution_limit and stats["submitted"] <= execution_limit,
                "run:startup-budget")
        require(stats["admitted_tool_calls"] == 0, "run:no-admitted-tools")
        require(stats["started_tools"] == 0, "run:no-started-tools")

    def show(self, identifier):
        return self.command("show", "--id", identifier)

    def state_bytes(self):
        return (self.root / "state/state.json").read_bytes()

    def snapshot(self):
        outer = json.loads(self.state_bytes())
        return json.loads(bytes(outer["entries"]["eve.cognition"]["cognition.v1"]))

    def feedback(self, identifier, revision, feedback_id, text, *, user="owner"):
        before = len(self.provider.requests)
        report = self.command(
            "feedback", "--id", identifier, "--revision", str(revision),
            "--feedback-id", feedback_id, "--text", text, "--user", user,
        )
        require(report["command"] == "feedback", "feedback:command")
        require(len(self.provider.requests) == before, "feedback:no-http")
        return report

    def observed_arguments(self, identifier, path, *, user="owner", limit=1, seconds=30):
        return (
            "run", "--seconds", str(seconds), "--max-executions", str(limit),
            "--observe-goal", identifier, "--observe-file", str(path),
            "--observe-user", user,
        )

    def start_observed(self, identifier, path, *, user="owner", limit=1):
        before = len(self.provider.requests)
        process = self.spawn(*self.observed_arguments(identifier, path, user=user, limit=limit))
        return process, before

    def wait_completed(self, process, identifier, expected, revision):
        # 只读取本场景临时目录中的状态。时间是故障看门狗，Completed 落盘才是通过门槛。
        deadline = time.monotonic() + 12
        while True:
            require(process.poll() is None, "gate:process-alive-until-durable-completion")
            snapshot = self.snapshot()
            goals = snapshot["state"]["goals"]
            children = [goal for goal in goals.values()
                        if goal["source"]["kind"] == "Inference"
                        and goal["source"]["reference"] == identifier
                        and goal["status"] == "Completed"
                        and goal["feedback"]["verification_met"] is True]
            require(len(children) <= expected, "gate:no-extra-completions")
            if len(children) == expected and goals[identifier]["revision"] == revision:
                return snapshot
            require(time.monotonic() < deadline, "gate:durable-completion-timeout")
            time.sleep(0.01)

    def stop_observed(self, process, before, *, limit=1):
        # Popen 是本场景直接创建的唯一进程句柄；不使用 PID 文件或进程组。
        require(process.poll() is None, "stop:owned-process-alive")
        process.send_signal(signal.SIGINT)
        report = self.finish(process, "run")
        self.record_run(report, before, execution_limit=limit)
        return report

    def idle_observed(self, identifier, path, *, user="owner"):
        before = len(self.provider.requests)
        # 无需模型完成的有限观察窗口；报告必须证明实际执行了至少一次读取。
        report = self.command(
            *self.observed_arguments(identifier, path, user=user, seconds=1),
            credentials=False,
        )
        self.record_run(report, before)
        expect_idle(report)
        observation = report["observation"]
        require(observation["reads"] >= 1, "unchanged:actually-read-file")
        require(observation["saved"] == 0, "unchanged:no-new-observation")
        require(observation["duplicates"] == observation["reads"], "unchanged:all-reads-deduplicated")
        return report

    def rejected_owner(self, identifier, path, *, user):
        persisted = self.state_bytes()
        before = len(self.provider.requests)
        process = self.spawn(
            *self.observed_arguments(identifier, path, user=user), credentials=False,
        )
        try:
            stdout, stderr = process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate()
            raise EvaluationFailure("owner:rejection-timeout") from None
        finally:
            self.live.discard(process)
        require(process.returncode != 0, "owner:unauthorized-run-rejected")
        require("观察目标不存在或不属于指定用户的本地目标".encode() in stderr,
                "owner:rejected-before-file-binding")
        require(FIXTURE_KEY.encode() not in stdout + stderr, "owner:credential-redaction")
        require(str(path).encode() not in stdout + stderr, "owner:path-redaction")
        require(self.state_bytes() == persisted, "owner:no-state-mutation")
        require(len(self.provider.requests) == before, "owner:no-http")

    def check_file_path_redaction(self, paths, *values):
        encoded = json.dumps(values, ensure_ascii=False)
        require(str(self.root) not in encoded, "observation:no-private-directory")
        for path in paths:
            require(path.name not in encoded, "observation:no-private-filename")

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
    first = case.run(expected_terminal="Completed")
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
        report = case.run(expected_terminal="Completed")
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
        report = case.run(expected_terminal="Blocked")
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
    report = case.run(expected_terminal="Blocked")
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


def prompt_data(value):
    """读取真实 HTTP 请求中末行的规划信封，正文仅用于内存中的断言。"""
    if isinstance(value, str):
        for line in reversed(value.splitlines()):
            try:
                candidate = json.loads(line)
            except ValueError:
                continue
            if isinstance(candidate, dict) and "unverified_waiting_input" in candidate:
                return candidate
    elif isinstance(value, list):
        for item in value:
            candidate = prompt_data(item)
            if candidate is not None:
                return candidate
    elif isinstance(value, dict):
        for item in value.values():
            candidate = prompt_data(item)
            if candidate is not None:
                return candidate
    return None


def observed_prompt(case, index, text, revision, *, current=True):
    require(len(case.provider.requests) > index, "observation:actual-model-request")
    data = prompt_data(case.provider.requests[index]["body"])
    require(data is not None, "observation:structured-planner-envelope")
    observation = data.get("untrusted_file_observation")
    require(isinstance(observation, dict), "observation:environment-provenance-present")
    require(observation["text_excerpt"] == text, "observation:actual-file-text")
    require(observation["sha256"] == hashlib.sha256(text.encode()).hexdigest(),
            "observation:full-file-digest")
    require(observation["byte_count"] == len(text.encode()), "observation:full-file-byte-count")
    require(observation["read_verified"] is True, "observation:read-evidence")
    require(observation["content_untrusted"] is True, "observation:no-instruction-authority")
    require(observation["observed_goal_revision"] == revision, "observation:observed-revision")
    require(observation["is_current_goal_revision"] is current,
            "observation:current-revision-provenance")
    return data


def expect_current_reflection(view, *, revision, completed):
    require(view["goal"]["revision"] == revision, "parent:expected-revision")
    require(view["goal"]["status"] == "Waiting", "parent:still-waiting")
    require(view["goal"]["feedback"] is None, "parent:no-real-world-success-claim")
    reflections = view["reflections"]
    require(len(reflections) == completed, "reflection:expected-durable-count")
    require(all(item["goal"]["status"] == "Completed" for item in reflections),
            "reflection:all-drafts-completed")
    require(sum(item["current"] is True for item in reflections) == 1,
            "reflection:exactly-one-current")
    require(sum(item["stale"] is True for item in reflections) == completed - 1,
            "reflection:old-drafts-stale")
    require(all(item["current"] is not item["stale"] for item in reflections),
            "reflection:current-stale-complement")
    for item in reflections:
        require(item["artifact"] == ARTIFACT, "reflection:verified-fixture-artifact")


def file_changes_replan_and_restart_deduplicates(case):
    identifier = "observed-parent"
    path = case.root / "private-progress-fixture.txt"
    first_text = "EVAL_PROGRESS_A：目前已收集两份材料。"
    second_text = "EVAL_PROGRESS_B：目前已收集三份材料。"
    path.write_text(first_text, encoding="utf-8")
    case.add(identifier)
    case.provider.enqueue()
    case.provider.enqueue()
    process, before = case.start_observed(identifier, path, limit=2)
    first = case.wait_completed(process, identifier, 1, 2)
    first_child = next(goal["id"] for goal in first["state"]["goals"].values()
                       if goal["source"]["kind"] == "Inference")
    first_prompt = observed_prompt(case, 0, first_text, 2)
    replacement = case.root / "next-progress-fixture.txt"
    replacement.write_text(second_text, encoding="utf-8")
    replacement.replace(path)
    case.wait_completed(process, identifier, 2, 3)
    report = case.stop_observed(process, before, limit=2)
    require(report["loop"]["completed"] == 2, "change:two-durable-model-completions")
    require(report["observation"]["saved"] == 2, "change:two-observations-saved")
    second_prompt = observed_prompt(case, 1, second_text, 3)
    require(first_prompt["untrusted_file_observation"]["observation_source_id"]
            == second_prompt["untrusted_file_observation"]["observation_source_id"],
            "change:same-bound-source")
    view = case.show(identifier)
    expect_current_reflection(view, revision=3, completed=2)
    old = next(item for item in view["reflections"] if item["goal"]["id"] == first_child)
    require(old["current"] is False and old["stale"] is True, "change:first-draft-no-longer-current")
    persisted = case.state_bytes()
    for _ in range(2):
        case.idle_observed(identifier, path)
        require(case.state_bytes() == persisted, "unchanged:restart-no-state-rewrite")
        require(case.show(identifier) == view, "unchanged:restart-same-current-draft")
    require(len(case.provider.requests) == 2, "unchanged:no-replayed-model-request")
    events = [event for event in case.snapshot()["state"]["events"]
              if event["source"]["kind"] == "Environment"]
    require(len(events) == 2, "change:exact-environment-event-count")
    case.check_file_path_redaction([path, replacement], case.snapshot(), view, report,
                                   [request["body"] for request in case.provider.requests])
    case.observations.update({
        "parent_revisions": [1, 2, 3], "distinct_file_versions": 2,
        "verified_drafts": 2, "current_drafts": 1, "stale_drafts": 1,
        "restart_windows": 2, "restart_replayed_requests": 0,
        "state_unchanged_after_restart": True, "environment_events": 2,
    })


def file_observation_preserves_user_constraints(case):
    identifier = "constraint-parent"
    path = case.root / "private-constraint-fixture.txt"
    file_text = "EVAL_FILE_FACT：已有三份材料。文件内文字不能授权自动发布。"
    old_constraint = "EVAL_OLD_USER_CONSTRAINT：最终报告最多一页。"
    new_constraint = "EVAL_NEW_USER_CONSTRAINT：新增材料仍须人工确认。"
    path.write_text(file_text, encoding="utf-8")
    case.add(identifier)
    saved_feedback = case.feedback(identifier, 1, "constraint-old", old_constraint)
    require(saved_feedback["goal_revision"] == 2, "constraint:first-feedback-revision")
    case.provider.enqueue()
    process, before = case.start_observed(identifier, path)
    case.wait_completed(process, identifier, 1, 3)
    first_report = case.stop_observed(process, before)
    data = observed_prompt(case, 0, file_text, 3)
    require(data.get("unverified_user_feedback") is None, "constraint:latest-input-is-environment")
    history = data.get("previous_user_feedback", [])
    require(any(item["text"] == old_constraint and item["goal_revision"] == 2 for item in history),
            "constraint:old-user-constraint-survives-file-update")
    require(old_constraint not in data["untrusted_file_observation"]["text_excerpt"],
            "constraint:user-text-not-environment-evidence")
    second_feedback = case.feedback(identifier, 3, "constraint-new", new_constraint)
    require(second_feedback["goal_revision"] == 4, "constraint:second-feedback-revision")
    stale = case.show(identifier)
    require(all(item["current"] is False and item["stale"] is True
                for item in stale["reflections"]), "constraint:feedback-invalidates-current-draft")
    case.provider.enqueue()
    process, before = case.start_observed(identifier, path)
    case.wait_completed(process, identifier, 2, 4)
    second_report = case.stop_observed(process, before)
    require(second_report["observation"]["saved"] == 0,
            "constraint:unchanged-file-does-not-overwrite-feedback")
    data = observed_prompt(case, 1, file_text, 3, current=False)
    feedback = data.get("unverified_user_feedback", {})
    require(feedback.get("text") == new_constraint and feedback.get("goal_revision") == 4
            and feedback.get("independently_verified") is False,
            "constraint:new-user-feedback-has-separate-provenance")
    require(any(item["text"] == old_constraint for item in data.get("previous_user_feedback", [])),
            "constraint:old-constraint-retained-after-second-feedback")
    snapshot = case.snapshot()
    events = snapshot["state"]["events"]
    environment = [event for event in events if event["source"]["kind"] == "Environment"]
    user_feedback = [event for event in events
                     if event["source"]["kind"] == "User"
                     and event["source"]["channel"] == "cognition.feedback"]
    require(len(environment) == 1 and len(user_feedback) == 2, "constraint:separate-durable-event-provenance")
    require(json.loads(environment[0]["summary"])["text_excerpt"] == file_text,
            "constraint:environment-event-contains-file-evidence")
    require({json.loads(event["summary"])["text"] for event in user_feedback}
            == {old_constraint, new_constraint}, "constraint:user-events-preserve-both-constraints")
    view = case.show(identifier)
    expect_current_reflection(view, revision=4, completed=2)
    persisted = case.state_bytes()
    case.idle_observed(identifier, path)
    require(case.state_bytes() == persisted, "constraint:restart-preserves-all-evidence")
    require(len(case.provider.requests) == 2, "constraint:one-model-request-per-replanned-revision")
    case.check_file_path_redaction([path], snapshot, view, first_report, second_report,
                                   [request["body"] for request in case.provider.requests])
    case.observations.update({
        "parent_revisions": [1, 2, 3, 4], "user_feedback_events": 2,
        "environment_events": 1, "old_constraints_retained": 1,
        "latest_feedback_independently_verified": False,
        "file_read_verified": True, "file_content_untrusted": True,
        "stale_file_evidence_retained_with_revision": True, "restart_replayed_requests": 0,
    })


def file_sources_and_owner_isolation(case):
    identifier = "source-parent"
    first_path = case.root / "private-source-a-fixture.txt"
    second_path = case.root / "private-source-b-fixture.txt"
    missing_path = case.root / "private-missing-fixture.txt"
    # 两个文件字节相同：摘要相同也不能掩盖显式切换来源。
    text = "EVAL_SHARED_BYTES：整理当前显式绑定的材料。"
    first_path.write_text(text, encoding="utf-8")
    second_path.write_text(text, encoding="utf-8")
    case.add(identifier, user="alice")
    case.rejected_owner(identifier, missing_path, user="bob")
    sources = []
    reports = []
    for index, path in enumerate([first_path, second_path, first_path]):
        case.provider.enqueue()
        process, before = case.start_observed(identifier, path, user="alice")
        case.wait_completed(process, identifier, index + 1, index + 2)
        reports.append(case.stop_observed(process, before))
        data = observed_prompt(case, index, text, index + 2)
        sources.append(data["untrusted_file_observation"]["observation_source_id"])
        require(reports[-1]["observation"]["saved"] == 1, "source:rebinding-saves-new-evidence")
        if index == 0:
            case.rejected_owner(identifier, second_path, user="bob")
    require(sources[0] != sources[1], "source:equal-bytes-distinct-source-identities")
    require(sources[0] == sources[2], "source:return-to-original-source-identity")
    snapshot = case.snapshot()
    environment = [event for event in snapshot["state"]["events"]
                   if event["source"]["kind"] == "Environment"]
    require(len(environment) == 3, "source:three-binding-events")
    require(all(event["source"]["channel"] == "cognition.file-observation"
                and event["visibility"] == {"User": "alice"} for event in environment),
            "source:environment-events-retain-owner-and-channel")
    view = case.show(identifier)
    expect_current_reflection(view, revision=4, completed=3)
    persisted = case.state_bytes()
    case.idle_observed(identifier, first_path, user="alice")
    require(case.state_bytes() == persisted, "source:same-final-source-deduplicates")
    require(len(case.provider.requests) == 3, "source:no-owner-or-restart-http")
    case.check_file_path_redaction([first_path, second_path, missing_path], snapshot, view,
                                   reports, [request["body"] for request in case.provider.requests])
    case.observations.update({
        "explicit_bindings": 3, "distinct_sources": 2, "distinct_content_digests": 1,
        "parent_revisions": [1, 2, 3, 4], "rejected_owner_attempts": 2,
        "unauthorized_observations_saved": 0, "unauthorized_model_requests": 0,
        "source_return_created_new_revision": True, "restart_replayed_requests": 0,
    })


SCENARIOS = [
    ("idle_without_credentials", idle_without_credentials),
    ("durable_reflection", durable_reflection),
    ("startup_budget_and_user_scope", budget_and_user_scope),
    ("malformed_artifacts", malformed_artifacts),
    ("unauthorized_sources", unauthorized_sources),
    ("unsolicited_tool_call", unsolicited_tool_call),
    ("forced_exit_recovery", forced_exit_recovery),
    ("file_changes_replan_and_restart_deduplicates", file_changes_replan_and_restart_deduplicates),
    ("file_observation_preserves_user_constraints", file_observation_preserves_user_constraints),
    ("file_sources_and_owner_isolation", file_sources_and_owner_isolation),
]

DIMENSIONS = {
    "idle_without_credentials": ["idle-without-model"],
    "durable_reflection": ["durable-draft", "restart-deduplication"],
    "startup_budget_and_user_scope": ["startup-budget", "user-scope"],
    "malformed_artifacts": ["artifact-validation", "restart-no-retry"],
    "unauthorized_sources": ["source-admission"],
    "unsolicited_tool_call": ["tool-admission"],
    "forced_exit_recovery": ["durable-recovery", "restart-no-replay"],
    "file_changes_replan_and_restart_deduplicates": [
        "environment-driven-replanning", "current-revision", "restart-deduplication",
    ],
    "file_observation_preserves_user_constraints": [
        "user-constraint-retention", "evidence-provenance", "feedback-replanning",
    ],
    "file_sources_and_owner_isolation": ["source-rebinding", "owner-admission", "evidence-provenance"],
}


def source_revision(value):
    if value and re.fullmatch(r"[0-9a-fA-F]{7,64}", value) is None:
        raise argparse.ArgumentTypeError("源码版本必须为空或 7 至 64 位十六进制提交摘要")
    return value.lower()


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path, help="已编译的 eve-cognition")
    parser.add_argument("--output", required=True, type=Path, help="结构化 JSON 结果路径")
    parser.add_argument("--source-sha", default="", type=source_revision,
                        help="构建者声明的 7 至 64 位提交摘要，不自动推断二进制版本")
    parser.add_argument("--scenario", action="append", choices=[name for name, _ in SCENARIOS],
                        help="只执行指定场景；可重复指定，省略时执行全部")
    return parser


def selected_scenarios(names):
    scenarios = dict(SCENARIOS)
    return [(name, scenarios[name]) for name in dict.fromkeys(names)] if names else list(SCENARIOS)


def main(argv=None):
    parser = argument_parser()
    args = parser.parse_args(argv)
    binary = args.binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("--binary 必须指向可执行的 eve-cognition 文件")
    began = time.monotonic()
    selected = selected_scenarios(args.scenario)
    results = []
    with tempfile.TemporaryDirectory(prefix="eve-cognition-evaluation-") as directory:
        for name, scenario in selected:
            started = time.monotonic()
            case = Scenario(binary, Path(directory) / name)
            result = {"name": name, "status": "passed", "dimensions": DIMENSIONS[name]}
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
        # v2 保留 v1 字段，增加显式场景选择与维度；所有数量仅针对本次所选场景。
        "format_version": 2,
        "evaluation": "bounded-endogenous-process-behavior",
        "provider": "mock-loopback-http-responses",
        "real_model_evaluated": False,
        "semantic_quality_evaluated": False,
        "selected_scenarios": [name for name, _ in selected],
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
            "通用智能水平；流程检查通过不等于 AGI 或模型质量达标",
        ],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"functional_ok": report["functional_ok"], **report["summary"]},
                     ensure_ascii=False))
    return 0 if report["functional_ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
