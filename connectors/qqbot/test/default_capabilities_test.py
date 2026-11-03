"""QQ 产品默认配置的真实进程验收：不使用最小配置或能力开关。

只把模型服务与 QQ 传输替换成本地协议服务；工具执行、权限、持久账本、重启和主动投递
均由实际 eve-qqbot 完成。模型替身只证明宿主契约，不代表真实模型理解质量。
"""
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import threading
import time
import traceback
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from fixture_auth import (
    bootstrap_test_admin,
    is_progress_request,
    progress_fixture_text,
    read_archived_state,
)

ROOT = pathlib.Path(__file__).resolve().parents[3]
BINARY = pathlib.Path(os.environ.get("EVE_QQBOT_BINARY", ROOT / "target/debug/eve-qqbot")).resolve()
FAKE = ROOT / "connectors/qqbot/test/fake-bridge.mjs"
WORKER = "你在为 Eve 独立完成一项委托的任务"
MINUTE = 60_000
# 注册证据取实际协议，不使用助手的能力自述或一份静态帮助文本。
DEFAULT_TOOLS = {
    "inspect_capabilities", "manage_business_permissions", "search_memory", "save_preference", "note_commitment",
    "list_interests", "list_goals", "list_skills", "list_forged_tools",
    "manage_skill", "manage_forged_tool", "set_reminder", "cancel_reminder",
    "dispatch_background_task", "list_background_tasks", "cancel_background_task",
    "run_subagent", "create_subagent", "list_subagents", "delete_subagent",
    "set_wakeup_trigger", "list_wakeup_triggers", "cancel_wakeup_trigger",
    "sleep", "task_progress", "save_workflow_tool", "list_workflow_tools",
    "run_workflow_tool", "inspect_runtime_health", "send_message",
    "read_file", "write_file", "edit_file", "run_command", "prepare_self_upgrade",
}


def text_response(text):
    return {"choices": [{"index": 0, "finish_reason": "stop", "message": {
        "role": "assistant", "content": text}}]}


def tool_response(call_id, name, arguments):
    return {"choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
        "role": "assistant", "content": None, "tool_calls": [{
            "id": call_id, "type": "function", "function": {
                "name": name, "arguments": json.dumps(arguments, ensure_ascii=False)}}]}}]}


class DefaultCapabilitiesAcceptance(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.work = pathlib.Path(self.directory.name)
        self.state = self.work / "state"
        self.state_path = self.state / "state.json"
        self.empty_path = self.work / "without-practice-runtimes"
        self.empty_path.mkdir()
        self.node = shutil.which("node")
        self.assertIsNotNone(self.node, "真实 fake bridge 需要 Node.js")
        self.requests = []
        self.errors = []
        self.lock = threading.Lock()
        self.runs = 0
        self.child = None
        self.callback = lambda body: text_response("收到")
        self.observe = lambda batch: {"updates": []}
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    self.respond()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception:
                    outer.errors.append(traceback.format_exc())
                    self.send_error(500)

            def respond(self):
                assert self.path == "/v1/chat/completions"
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                latest = body["messages"][-1].get("content") or ""
                try:
                    decoded = json.loads(latest)
                except (ValueError, TypeError):
                    decoded = None
                kind = "chat"
                if is_progress_request(body):
                    kind = "progress"
                    response = text_response(progress_fixture_text(decoded))
                elif isinstance(decoded, dict) and "observer_version" in decoded and "known_interests" in decoded:
                    kind = "interest"
                    response = text_response(json.dumps(outer.observe(decoded), ensure_ascii=False))
                elif isinstance(decoded, dict) and "extractor_version" in decoded and "evidence" in decoded:
                    kind = "learning"
                    response = text_response('{"candidates":[]}')
                elif isinstance(decoded, dict) and "feedback_pairs" in decoded and "messages" in decoded:
                    kind = "conversation_learning"
                    outer.assertFalse(body.get("tools"), "交流经验分析不能获得业务工具")
                    response = text_response('{"lessons":[]}')
                elif "unverified_waiting_input" in latest:
                    kind = "reflection"
                    response = text_response(json.dumps({"summary": "没有已验证的执行结果",
                        "next_step": "等待可核验的任务证据", "needs_user_input": True}, ensure_ascii=False))
                else:
                    response = outer.callback(body)
                with outer.lock:
                    outer.requests.append({"kind": kind, "body": body, "run": outer.runs,
                                           "state": outer.documents()})
                encoded = json.dumps(response, ensure_ascii=False).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.stop_server)

    def stop_server(self):
        if self.child is not None and self.child.poll() is None:
            self.child.kill()
            self.child.communicate(timeout=10)
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def documents(self):
        return read_archived_state(self.state_path)

    def current(self, body):
        return max(index for index, message in enumerate(body["messages"]) if message["role"] == "user")

    def text(self, body):
        return body["messages"][self.current(body)]["content"]

    def results(self, body):
        return [json.loads(message["content"]) for message in body["messages"][self.current(body) + 1:]
                if message["role"] == "tool"]

    def definitions(self, body):
        return {entry["function"]["name"] for entry in body.get("tools", [])}

    def host_capabilities(self, body):
        for message in body["messages"]:
            content = message.get("content") or ""
            if message["role"] != "system":
                continue
            for line in content.splitlines():
                if not line.startswith("{") or "eve.host.capabilities" not in line:
                    continue
                value = json.loads(line)
                if value.get("kind") == "eve.host.capabilities":
                    return value
        self.fail("模型没有收到真实宿主能力资料")

    def assert_default_tools(self, body):
        names = self.definitions(body)
        self.assertTrue(DEFAULT_TOOLS <= names, sorted(DEFAULT_TOOLS - names))
        host = self.host_capabilities(body)
        self.assertEqual({tool["name"] for tool in host["conversation_tools"]}, names)
        # 在独立工作目录和只含桥接依赖的 PATH 下，缺少浏览器/游戏运行器是明确事实。
        self.assertIsNone(host["practice_runner"])
        self.assertFalse(names & {"run_practice", "check_practice_draft", "use_skill"})
        return names

    def send(self, identity, text, expected):
        return [{"send": {"id": identity, "scope": "c2c", "target_id": "user-1",
                          "user_id": "user-1", "text": text, "original_text": text,
                          "expected": expected, "expected_type": "reply"}},
                {"wait_receipt": {"path": str(self.state_path), "id": identity, "state": "Sent"}}]

    def checkpoint(self, name):
        return [{"touch": str(self.work / (name + ".inspect"))},
                {"wait_file": str(self.work / (name + ".continue"))}]

    def wait_for(self, predicate, message, timeout=20):
        deadline = time.monotonic() + timeout
        while not predicate():
            self.assertLess(time.monotonic(), deadline, message)
            self.assertFalse(self.errors, "\n".join(self.errors))
            if self.child is not None:
                self.assertIsNone(self.child.poll(), message + ": Eve 进程已结束")
            time.sleep(0.02)

    def run_default(self, script, checkpoints=None):
        self.runs += 1
        scenario = self.work / f"scenario-{self.runs}.json"
        events = self.work / f"events-{self.runs}.jsonl"
        error = self.work / f"bridge-error-{self.runs}.txt"
        scenario.write_text(json.dumps({"script": script, "events_file": str(events),
            "error_file": str(error), "wait_timeout_ms": 25_000}), encoding="utf8")
        # 环境只提供服务接入与进程依赖，不设置任何关闭产品能力的环境变量。
        env = {key: os.environ[key] for key in ("SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(PATH=str(self.empty_path), QQBOT_APP_ID="1904159860",
            QQBOT_APP_SECRET="test-app-secret", EVE_OPENAI_API_KEY="test-model-secret",
            EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
            EVE_OPENAI_PROTOCOL="chat", EVE_LLM_RESPONSE_MODE="complete")
        bootstrap_test_admin(self.state, env, users=("user-1",))
        command = [str(BINARY), "--state-dir", str(self.state), "--agent", str(ROOT / "AGENT.md"),
                   "--node", self.node, "--bridge-script", str(FAKE), "--bridge-arg", str(scenario)]
        self.assertEqual([arg for arg in command if arg.startswith("--")],
            ["--state-dir", "--agent", "--node", "--bridge-script", "--bridge-arg"])
        self.child = subprocess.Popen(command, cwd=self.work, env=env,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            for name, callback in (checkpoints or {}).items():
                self.wait_for(lambda: (self.work / (name + ".inspect")).exists(), name + " 未到达")
                callback()
                (self.work / (name + ".continue")).touch()
            stdout, stderr = self.child.communicate(timeout=35)
            self.assertFalse(error.exists(), error.read_text() if error.exists() else "")
            self.assertFalse(self.errors, "\n".join(self.errors))
            self.assertEqual(self.child.returncode, 0, stderr)
            summary = json.loads(stdout)
            self.assertTrue(summary["closed"])
            self.assertFalse(summary["terminal_error"])
            for secret in ("test-app-secret", "test-model-secret"):
                self.assertNotIn(secret, stdout + stderr)
        except Exception as failure:
            if self.child.poll() is None:
                self.child.kill()
            _, stderr = self.child.communicate(timeout=10)
            raise AssertionError(f"{failure}\nEve stderr:\n{stderr}") from failure
        finally:
            if self.child.poll() is None:
                self.child.kill()
            self.child.communicate(timeout=10)
            self.child = None

    def reminders(self):
        return self.documents().get("eve.reminder", {}).get("reminders.v1", {}).get("reminders", [])

    def tasks(self):
        return self.documents().get("eve.task", {}).get("tasks.v1", {}).get("tasks", [])

    def pushes(self):
        return [event for path in sorted(self.work.glob("events-*.jsonl"))
                for line in path.read_text().splitlines()
                if (event := json.loads(line)).get("type") == "push"]

    def test_default_memory_learning_and_cognition_run_from_ordinary_chat(self):
        first = "我喜欢水彩画，但是不知道怎么调色"
        originals = [first, "回复先给简短结论", "再给我必要的依据"]

        def observe(batch):
            updates = []
            for evidence in batch["evidence"]:
                if evidence["source"]["user_text"] == first:
                    updates.append({"target": {"new": {"topic": "水彩画调色"}}, "statements": [
                        {"kind": "interest", "quote": "我喜欢水彩画", "evidence_id": evidence["id"]},
                        {"kind": "difficulty", "quote": "不知道怎么调色", "evidence_id": evidence["id"]}]})
            return {"updates": updates}

        self.observe = observe
        self.callback = lambda body: (self.assert_default_tools(body), text_response("收到"))[1]

        def observed_and_learned():
            documents = self.documents()
            interests = documents.get("eve.interest", {}).get("interests.v1", {}).get("interests", [])
            goals = documents.get("eve.cognition", {}).get("cognition.v1", {}).get("state", {}).get("goals", {})
            jobs = documents.get("eve.learning", {}).get("learning.v1", {}).get("jobs", [])
            return (len(interests) == 1 and any(goal["verification"] == "reflection:v1" and
                goal["status"] == "Completed" for goal in goals.values()) and
                any(record["job"]["status"] == "Completed" for record in jobs))

        script = [step for index, text in enumerate(originals)
                  for step in self.send(f"learning-{index}", text, "收到")]
        self.run_default([*script, *self.checkpoint("learned")], checkpoints={"learned": lambda:
            self.wait_for(observed_and_learned, "默认观察、认知或记忆提炼未实际运行")})
        documents = self.documents()
        snapshots = documents["eve.memory"]["memory.v1"]["scopes"]
        evidence = [item for scope in snapshots for item in scope["snapshot"]["evidence"]
                    if item["source"]["kind"] == "CompletedInteraction"]
        self.assertTrue(set(originals) <= {item["source"]["user_text"] for item in evidence})
        [interest] = documents["eve.interest"]["interests.v1"]["interests"]
        self.assertEqual(interest["topic"], "水彩画调色")
        source = next(item for item in evidence if item["source"]["user_text"] == first)
        for statement in interest["statements"]:
            self.assertEqual(statement["evidence_id"], source["id"])
            self.assertIn(statement["quote"], first)
        goals = list(documents["eve.cognition"]["cognition.v1"]["state"]["goals"].values())
        [goal] = [item for item in goals if item["source"].get("channel") == "interest.learning"]
        self.assertEqual(goal["status"], "Waiting", "反思产物不冒充用户目标已经完成")
        self.assertEqual(goal["source"]["reference"], interest["id"])
        [reflection] = [item for item in goals if item["source"].get("channel") == "endogenous"
                        and item["source"].get("reference") == goal["id"]]
        self.assertEqual((reflection["verification"], reflection["status"]),
                         ("reflection:v1", "Completed"))
        self.assertFalse(any(item["source"]["kind"] == "User" for item in goals),
                         "普通兴趣原话不能伪装成用户执行授权")
        learned = documents["eve.learning"]["learning.v1"]["jobs"]
        self.assertTrue(any(record["job"]["status"] == "Completed" for record in learned))
        maintenance = [request for request in self.requests if request["kind"] == "learning"]
        self.assertTrue(maintenance)
        self.assertTrue(all(not request["body"].get("tools") for request in maintenance))
        self.assertEqual(self.pushes(), [], "未验证反思不能宣称完成并主动投递")

    def test_default_tools_save_thirty_minute_reminder_and_deliver_after_restart(self):
        def respond(body):
            self.assert_default_tools(body)
            results = self.results(body)
            if not results:
                return tool_response("default-live-message", "send_message", {"text": "先保存你的提醒"})
            if len(results) == 1:
                self.assertTrue(results[0]["sent"], results[0])
                self.assertEqual(results[0]["delivery_status"], "sent")
                return tool_response("default-reminder", "set_reminder", {"text": "喝水", "in_minutes": 30})
            result = results[-1]
            self.assertTrue(result["saved"])
            self.assertEqual(result["reminder"]["text"], "喝水")
            [saved] = self.reminders()
            self.assertEqual(saved["id"], result["reminder"]["id"], "确认回复前须已持久化")
            return text_response("三十分钟后提醒你喝水")
        self.callback = respond
        self.run_default(self.send("reminder", "半小时后提醒我喝水", "三十分钟后提醒你喝水"))
        [reminder] = self.reminders()
        self.assertEqual(reminder["status"], "Scheduled")
        self.assertEqual(reminder["due_at_ms"] - reminder["created_at_ms"], 30 * MINUTE)
        self.assertEqual(reminder["source_message_id"],
                         "tool:" + hashlib.sha256(b"default-reminder").hexdigest())
        self.assertEqual(self.pushes(), [])
        progress = [event for path in self.work.glob("events-*.jsonl")
                    for line in path.read_text().splitlines()
                    if (event := json.loads(line)).get("type") == "progress" and
                    event.get("text") == "先保存你的提醒"]
        self.assertEqual(len(progress), 1, "默认 send_message 须走实际桥接投递")
        self.assertEqual(progress[0]["id"], "reminder")

        # 进程停止后只把账本三个时间戳共同前移，代替真实等待 31 分钟。
        state = json.loads(self.state_path.read_text())
        ledger = self.documents()["eve.reminder"]["reminders.v1"]
        for item in ledger["reminders"]:
            self.assertEqual(item["attempts"], [])
            for key in ("created_at_ms", "due_at_ms", "closed_at_ms"):
                if item[key] is not None:
                    item[key] -= 31 * MINUTE
        state["entries"]["eve.reminder"]["reminders.v1"] = list(json.dumps(ledger).encode())
        self.state_path.write_text(json.dumps(state), encoding="utf8")
        chat_count = sum(request["kind"] == "chat" for request in self.requests)
        self.run_default(self.checkpoint("delivered"), checkpoints={"delivered": lambda:
            self.wait_for(lambda: self.reminders()[0]["status"] == "Delivered", "提醒未主动送达")})
        [push] = self.pushes()
        self.assertEqual((push["id"], push["target_id"], push["text"]),
                         (reminder["id"], "user-1", "提醒你：喝水"))
        self.assertNotIn("msg_id", push)
        [attempt] = self.reminders()[0]["attempts"]
        self.assertEqual(attempt["result"], {"sent": {"platform_message_id": "push-" + reminder["id"]}})
        self.assertEqual(sum(request["kind"] == "chat" for request in self.requests), chat_count,
                         "到期投递不需要新用户输入或模型编造回执")
        delivered = self.reminders()

        def stays_delivered():
            # 负向重放检查跨过提醒器的 2 秒扫描周期，进程由桥接门控保持存活。
            deadline = time.monotonic() + 2.1
            while time.monotonic() < deadline:
                self.assertIsNone(self.child.poll())
                self.assertEqual(self.reminders(), delivered)
                self.assertEqual(len(self.pushes()), 1, "已送达提醒重启后不重发")
                time.sleep(0.05)

        self.run_default(self.checkpoint("unchanged"), checkpoints={"unchanged": stays_delivered})

    def test_default_background_execution_rejects_unverified_success_and_keeps_result(self):
        def respond(body):
            results = self.results(body)
            if WORKER in body["messages"][0].get("content", ""):
                if not results:
                    self.assertIn("echo", self.definitions(body))
                    return tool_response("claim", "echo", {"text": "模型声称所有工作已完成"})
                self.assertFalse(body.get("tools"), "一步预算已消耗")
                return text_response("模型声称所有工作已完成")
            self.assert_default_tools(body)
            if self.text(body) == "重启后查任务":
                return text_response("已保留任务状态")
            if not results:
                return tool_response("default-background", "dispatch_background_task", {
                    "title": "核对日志", "instructions": "读取日志并核对错误数量", "max_steps": 1})
            self.assertTrue(results[0]["dispatched"])
            self.assertTrue(self.tasks(), "派发确认前须已保存任务")
            return text_response("已派发后台任务")
        self.callback = respond
        self.run_default([*self.send("background", "请在后台核对日志", "已派发后台任务"),
                          *self.checkpoint("blocked")], checkpoints={"blocked": lambda:
            self.wait_for(lambda: any(t["status"] == "Blocked" and
                (t.get("notice") or {}).get("status") == "Delivered" for t in self.tasks()),
                "未核验任务没有如实阻塞并通知")})
        [task] = self.tasks()
        self.assertEqual(task["usage"], {"tool_calls": 1, "model_requests": 2})
        self.assertTrue(task["result"].startswith("任务尚未通过冻结验收："))
        [notice] = self.pushes()
        self.assertEqual(notice["id"], task["id"])
        self.assertIn("等待补充信息或解除阻塞", notice["text"])
        self.assertNotIn("完成了：", notice["text"])
        workers = sum(WORKER in r["body"]["messages"][0].get("content", "") for r in self.requests)
        self.run_default(self.send("restart", "重启后查任务", "已保留任务状态"))
        self.assertEqual(self.tasks(), [task])
        self.assertEqual(sum(WORKER in r["body"]["messages"][0].get("content", "")
                             for r in self.requests), workers)
        self.assertEqual(len(self.pushes()), 1)

    def test_missing_runner_is_reported_and_cannot_produce_verified_practice(self):
        def respond(body):
            names = self.assert_default_tools(body)
            results = self.results(body)
            if not results:
                return tool_response("inspect", "inspect_capabilities", {})
            if len(results) == 1:
                report = results[0]
                self.assertEqual(set(report["installed_tools"]), names)
                [missing] = [item for item in report["unavailable"] if item["capability"] == "practice"]
                self.assertTrue(missing["reason"], "缺失运行器的原因必须明示")
                self.assertTrue(missing["recovery"], "必须给出恢复所需条件")
                # 未注册的名字也不能因模型强行发出调用而变成已运行。
                return tool_response("invented-runner", "run_practice", {})
            self.assertEqual(results[-1]["error"]["code"], "UnknownTool")
            return text_response("没有运行器，尚未执行验证")
        self.callback = respond
        self.run_default(self.send("no-runner", "实际运行并验证这个产物", "没有运行器，尚未执行验证"))
        practice = self.documents().get("eve.practice", {}).get("practice.v1", {}).get("runs", [])
        self.assertEqual(practice, [], "未装配的运行器不能生成实践成功记录")


if __name__ == "__main__":
    unittest.main(verbosity=2)
