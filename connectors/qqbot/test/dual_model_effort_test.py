"""双模型与思考强度的真实宿主验收；模型和 QQ 仅用独立环回替身。"""
from contextlib import closing
import argparse
import hashlib
import json
import sqlite3
import sys
import threading
import time
import traceback
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from model_adapter_fixture import (
    CHAT_PATH, RESPONSES_PATH, MODEL, MODEL_KEY, MAX_OUTPUT, Reply, WireAcceptance,
    content_text, current_results, items, latest_user, tool_definitions,
)

CODING_MODEL = "coding-process-fixture"
CODING_KEY = "coding-process-fixture-key"
CODING_OUTPUT = 160
LEARNING_PREFIX = "你整理 Eve 从真实交流中学到的交往经验。"
LEAF_PREFIX = "你是 Eve 的子代理"
WORKER_PREFIX = "你在为 Eve 独立完成一项委托的任务"
DIGEST_PREFIX = "你是对话历史压缩器"
RECURSIVE = {
    "run_subagent", "dispatch_background_task", "create_subagent", "list_subagents",
    "delete_subagent", "set_wakeup_trigger",
}


def first_system(body):
    return next((content_text(item) for item in items(body)
                 if item.get("role") == "system"), body.get("instructions", ""))


def reasoning(body):
    return body.get("reasoning_effort") if "messages" in body else body.get("reasoning", {}).get("effort")


def receipt_contract(path, content):
    return {"kind": "receipts", "contract": {"checks": [{
        "tool": "read_file", "arguments": {"path": path}, "output_pointer": "/sha256",
        "expected": hashlib.sha256(content.encode()).hexdigest(),
    }], "require_final_batch": True}}


class DualModelEffortAcceptance(WireAcceptance):
    """复用宿主/桥握手，独立记录主模型和 coding 模型的完整请求。"""

    def setUp(self):
        super().setUp()
        self.source = self.work / "source"
        self.source.mkdir()
        (self.source / "README.md").write_text("真实源码测试", encoding="utf8")
        principal = "qq:" + hashlib.sha256(json.dumps(
            ["1904159860", "c2c", "user-1", "user-1"], separators=(",", ":")
        ).encode()).hexdigest()
        self.developer_config = self.work / "developer.json"
        self.developer_config.write_text(json.dumps({
            "workspace": str(self.source.resolve()), "operators": [principal],
            "ordinary_access": False, "commands": True, "github": False,
            "self_upgrade": False, "max_tool_rounds": 4,
        }), encoding="utf8")
        self.coding_requests = []
        self.coding_callback = lambda _: self.fail("未安排的 coding 模型调用")
        self.coding_http_status = lambda _: 200
        self.efforts_by_input = {}
        self.coding_effort = "high"
        outer = self

        class CodingHandler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                outer.errors.append("coding 路由不应探测模型目录：" + self.path)
                self.send_error(404)

            def do_POST(self):
                try:
                    length = int(self.headers["Content-Length"])
                    assert 0 < length <= 8 * 1024 * 1024
                    captured = {"path": self.path,
                                "headers": {key.lower(): value for key, value in self.headers.items()},
                                "body": json.loads(self.rfile.read(length))}
                    with outer.lock:
                        outer.coding_requests.append(captured)
                        number = len(outer.coding_requests)
                    outer.assert_coding_wire(captured)
                    status = outer.coding_http_status(captured)
                    if status != 200:
                        self.send_error(status, "synthetic coding provider failure")
                        return
                    reply = outer.coding_callback(captured)
                    content_type, data = reply.encode(self.path, number)
                    self.send_response(200)
                    self.send_header("Content-Type", content_type)
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    for offset in range(0, len(data), 17):
                        self.wfile.write(data[offset:offset + 17])
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception:
                    outer.errors.append(traceback.format_exc())
                    self.send_error(500)

        self.coding_server = ThreadingHTTPServer(("127.0.0.1", 0), CodingHandler)
        self.coding_server.daemon_threads = True
        self.coding_thread = threading.Thread(target=self.coding_server.serve_forever, daemon=True)
        self.coding_thread.start()

    def tearDown(self):
        self.run_release.set()
        self.coding_server.shutdown()
        self.coding_server.server_close()
        self.coding_thread.join()
        super().tearDown()

    def enabled(self, script, coding_adapter="openai", coding_protocol="chat", training=False,
                **options):
        environment = self.model_environment()
        environment.pop("EVE_OPENAI_REASONING_EFFORT")
        environment.update({
            "EVE_DEVELOPER_CONFIG": str(self.developer_config),
            "EVE_MODEL_USE_CODING_ROLE": "auxiliary_small",
            "EVE_MODELS_AUXILIARY_SMALL_ENABLED": "true",
            "EVE_MODELS_AUXILIARY_SMALL_PROVIDER": coding_adapter,
            "EVE_MODELS_AUXILIARY_SMALL_MODEL": CODING_MODEL,
            "EVE_MODELS_AUXILIARY_SMALL_MAX_OUTPUT_TOKENS": str(CODING_OUTPUT),
            "EVE_MODELS_AUXILIARY_SMALL_CREDENTIAL_REF": "env:EVE_AUXILIARY_API_KEY",
            "EVE_AUXILIARY_API_KEY": CODING_KEY,
            "EVE_OPENAI_AUXILIARY_BASE_URL": f"http://127.0.0.1:{self.coding_server.server_port}/v1",
            "EVE_OPENAI_AUXILIARY_PROTOCOL": coding_protocol,
            "EVE_OPENAI_AUXILIARY_REASONING_EFFORT": "high",
        })
        environment.update(options.pop("env_extra", {}))
        return self.run_eve(script, memory=False, training=training,
                            command_extra=["--subagents", "--background-tasks"],
                            env_extra=environment, **options)

    def assert_wire_identity(self, captured, model, key, maximum, effort):
        body, headers, path = captured["body"], captured["headers"], captured["path"]
        self.assertIn(path, (CHAT_PATH, RESPONSES_PATH))
        self.assertEqual(body["model"], model)
        self.assertEqual(headers.get("authorization"), "Bearer " + key)
        self.assertNotIn("temperature", body)
        self.assertEqual(reasoning(body), effort)
        if effort is None:
            self.assertNotIn("reasoning_effort", body)
            self.assertNotIn("reasoning", body)
        if path == CHAT_PATH:
            self.assertEqual(body["max_tokens"], maximum)
            self.assertFalse(body["stream"])
        else:
            self.assertEqual(body["max_output_tokens"], maximum)
            self.assertTrue(body["stream"])
        for name in ("session-id", "thread-id", "x-client-request-id"):
            self.assertTrue(headers.get(name), "缺少分工会话头：" + name)
        self.assertTrue(body.get("prompt_cache_key"))

    def assert_model_wire(self, captured):
        body = captured["body"]
        learning = first_system(body).startswith(LEARNING_PREFIX)
        self.assert_wire_identity(captured, MODEL, MODEL_KEY, MAX_OUTPUT,
                                  self.efforts_by_input.get(latest_user(body)))
        if first_system(body).startswith(DIGEST_PREFIX):
            self.assertFalse(body.get("tools"))
            self.assertEqual(body.get("response_format"), {"type": "json_object"})
            return
        if learning:
            self.assertFalse(body.get("tools"))
            self.assertEqual([item["role"] for item in items(body)], ["system", "user"])
            self.assertEqual(body.get("response_format"), {"type": "json_object"})
            batch = json.loads(latest_user(body))
            self.assertTrue({"id", "scope", "messages", "lessons", "feedback_pairs"} <= set(batch))
            self.assertTrue(batch["messages"])
            with closing(sqlite3.connect((self.work / "state/training-expressions.sqlite3").resolve().as_uri() + "?mode=ro", uri=True)) as db:
                recorded = {json.loads(payload)["id"]: json.loads(payload) for payload, in db.execute(
                    "SELECT message_json FROM conversation_events WHERE session_id=? AND user_id=?",
                    (batch["scope"]["session_id"], batch["scope"]["user_id"]))}
            for message in batch["messages"]:
                self.assertEqual(message["text"], recorded[message["id"]]["text"])
            return
        definitions = tool_definitions(body)
        if LEAF_PREFIX in first_system(body):
            self.assertFalse(set(definitions) & RECURSIVE)
        elif WORKER_PREFIX not in first_system(body):
            self.assertIn("run_subagent", definitions)
            policy = "\n".join(content_text(item) for item in items(body) if item.get("role") == "system")
            self.assertIn("编程分工：", policy)
            self.assertIn("agent_id=coding", policy)
            self.assertIn("include_context=true", policy)

    def assert_coding_wire(self, captured):
        self.assert_wire_identity(captured, CODING_MODEL, CODING_KEY, CODING_OUTPUT, self.coding_effort)
        body = captured["body"]
        self.assertIn(LEAF_PREFIX, first_system(body))
        if body.get("tools"):
            self.assertIn("同一个 Eve 的编程执行分工", first_system(body))
        else:
            self.assertIn("本任务只交付文本产物", first_system(body))
        self.assertFalse(set(tool_definitions(body)) & RECURSIVE)

    def foreground(self):
        return [request for request in self.requests
                if not first_system(request["body"]).startswith(LEARNING_PREFIX)
                and not first_system(request["body"]).startswith(DIGEST_PREFIX)
                and LEAF_PREFIX not in first_system(request["body"])
                and WORKER_PREFIX not in first_system(request["body"])]

    def assert_isolated(self, groups):
        for group in groups:
            self.assertTrue(group)
            if len(group) > 1:
                self.assert_same_session(group)
        self.assertEqual(len({group[0]["headers"]["session-id"] for group in groups}), len(groups))
        self.assertEqual(len({group[0]["body"]["prompt_cache_key"] for group in groups}), len(groups))

    def test_coding_chat_and_codex_write_verify_return_then_restart_without_replay(self):
        turns = []
        coding_groups = []
        for adapter, protocol in (("openai", "chat"), ("codex", "responses")):
            with self.subTest(adapter=adapter, protocol=protocol):
                path = f"coding-{protocol}.py"
                content = f'value = "{protocol}-真实产物"\n'
                turn = f"实现并验证 {path}，保留当前其他改动"
                message_id = "coding-" + protocol
                turns.append((message_id, turn))
                start = len(self.coding_requests)

                def coding(captured):
                    body = captured["body"]
                    self.assertEqual(captured["path"], CHAT_PATH if protocol == "chat" else RESPONSES_PATH)
                    self.assertIn(turn, latest_user(body), "受派模型须收到本轮真实上下文")
                    results = current_results(body)
                    if not results:
                        return Reply(calls=[("coding-write-" + protocol, "write_file", {
                            "path": path, "content": content, "expected_sha256": ""})])
                    if len(results) == 1:
                        self.assertTrue(results[0]["success"])
                        return Reply(calls=[("coding-read-" + protocol, "read_file", {"path": path})])
                    self.assertEqual(len(results), 2)
                    self.assertEqual(results[-1]["content"], content)
                    return Reply(text="编程文件已实际写入并读取验证")

                def main(captured):
                    body = captured["body"]
                    self.assertEqual(latest_user(body), turn)
                    results = current_results(body)
                    if not results:
                        return Reply(calls=[("delegate-" + protocol, "run_subagent", {
                            "agent_id": "coding", "task": turn, "include_context": True,
                            "verification": receipt_contract(path, content)})])
                    self.assertEqual(len(results), 1)
                    self.assertTrue(results[0]["completed"], results[0])
                    self.assertTrue(results[0]["include_context"])
                    self.assertEqual(results[0]["usage"]["agent_id"], "coding")
                    self.assertEqual(results[0]["usage"]["steps_used"], 2)
                    self.assertIn("实际写入并读取验证", results[0]["result"])
                    return Reply(text="主线程收到真实编程结果")

                self.coding_callback, self.callback = coding, main
                self.enabled(self.send(self.message(message_id, turn, expected="主线程收到真实编程结果")),
                             coding_adapter=adapter, coding_protocol=protocol)
                self.assertEqual((self.source / path).read_text(), content)
                group = self.coding_requests[start:]
                self.assertEqual(len(group), 3)
                coding_groups.append(group)

        self.assertEqual(len(self.foreground()), 4, "两项委派仅调用主线程交接和结果判断")
        self.assert_isolated([self.foreground(), *coding_groups])
        journal = self.work / "state/developer-tools/operations.jsonl"
        before = journal.read_bytes()
        self.callback = lambda _: Reply(text="恢复后聊天正常")
        self.enabled([*(entry for message_id, turn in turns for entry in self.duplicate(message_id, turn)),
                      *self.send(self.message("coding-restored", "恢复后聊天", expected="恢复后聊天正常"))])
        self.assertEqual(len(self.coding_requests), 6, "已完成编程调用不在重投或重启后重放")
        self.assertEqual(journal.read_bytes(), before)
        self.assert_same_session(self.foreground())

    def test_chat_general_task_and_distillation_keep_primary_route_and_separate_sessions(self):
        def main(captured):
            body = captured["body"]
            system = first_system(body)
            if system.startswith(LEARNING_PREFIX):
                return Reply(text='{"lessons":[]}')
            if LEAF_PREFIX in system:
                self.assertNotIn("同一个 Eve 的编程执行分工", system)
                return Reply(text="通用素材整理结果")
            if WORKER_PREFIX in system:
                return Reply(text="后台便签整理结果")
            text, results = latest_user(body), current_results(body)
            if text == "普通聊天":
                return Reply(text="今天随便聊两句")
            if text == "通用整理素材":
                if not results:
                    return Reply(calls=[("delegate-general", "run_subagent", {
                        "agent_id": "general", "task": "整理文字素材", "include_context": True,
                        "verification": {"kind": "text", "required_fragments": ["通用素材整理结果"]}})])
                self.assertTrue(results[-1]["completed"])
                self.assertEqual(results[-1]["usage"]["agent_id"], "general")
                return Reply(text="通用结果已收到")
            self.assertEqual(text, "后台整理便签")
            if not results:
                return Reply(calls=[("dispatch-notes", "dispatch_background_task", {
                    "title": "便签整理", "instructions": "整理文字便签", "max_steps": 2,
                    "verification": {"kind": "text", "required_fragments": ["后台便签整理结果"]}})])
            self.assertTrue(results[-1]["dispatched"])
            return Reply(text="便签已经交给后台")

        def settled():
            def complete():
                tasks = self.documents().get("eve.task", {}).get("tasks.v1", {}).get("tasks", [])
                path = self.work / "state/training-expressions.sqlite3"
                with closing(sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)) as db:
                    learned = db.execute("SELECT COUNT(*) FROM conversation_batches WHERE outcome='completed'").fetchone()[0]
                return (learned > 0 and len(tasks) == 1 and tasks[0]["status"] == "Completed"
                        and (tasks[0].get("notice") or {}).get("status") == "Delivered")
            self.wait_until(complete, "后台或真实蒸馏批次没有完成")

        self.callback = main
        self.enabled([*self.send(self.message("route-chat", "普通聊天", expected="今天随便聊两句")),
                      *self.send(self.message("route-general", "通用整理素材", expected="通用结果已收到")),
                      *self.send(self.message("route-task", "后台整理便签", expected="便签已经交给后台")),
                      *self.checkpoint("settled")], training=True, checkpoints={"settled": settled})
        self.assertEqual(self.coding_requests, [], "普通聊天、通用子任务、后台任务和蒸馏不占编程端点")
        general = [r for r in self.requests if LEAF_PREFIX in first_system(r["body"])]
        worker = [r for r in self.requests if WORKER_PREFIX in first_system(r["body"])
                  and LEAF_PREFIX not in first_system(r["body"])]
        learning = [r for r in self.requests if first_system(r["body"]).startswith(LEARNING_PREFIX)]
        self.assertEqual((len(self.foreground()), len(general), len(worker)), (5, 1, 1))
        self.assertTrue(learning)
        # 蒸馏可能按输入提交边界领取多个批次；这些批次仍属于同一学习会话。
        self.assert_isolated([self.foreground(), general, worker, learning])
        [task] = self.documents()["eve.task"]["tasks.v1"]["tasks"]
        before = json.loads(json.dumps(task))
        self.callback = lambda captured: Reply(text='{"lessons":[]}' if first_system(captured["body"]).startswith(LEARNING_PREFIX)
                                               else "重启后原任务保持完成")
        self.enabled([*self.duplicate("route-task", "后台整理便签"),
                      *self.send(self.message("route-restored", "原任务怎样", expected="重启后原任务保持完成"))], training=True)
        self.assertEqual(self.documents()["eve.task"]["tasks.v1"]["tasks"], [before])
        self.assertEqual(len([r for r in self.requests if WORKER_PREFIX in first_system(r["body"])
                              and LEAF_PREFIX not in first_system(r["body"])]), 1)
        self.assertEqual(self.coding_requests, [])

    def test_coding_failure_after_command_keeps_receipt_without_primary_fallback_or_replay(self):
        marker = self.source / "coding-once.txt"
        turn = "编程检查后上游失败"

        def coding(captured):
            self.assertFalse(current_results(captured["body"]))
            return Reply(calls=[("coding-command-once", "run_command", {
                "program": sys.executable, "args": ["-c", "from pathlib import Path; p=Path('coding-once.txt'); "
                "p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('CODING_ACTUAL_RECEIPT')"]})])

        self.coding_http_status = lambda captured: 503 if current_results(captured["body"]) else 200
        self.coding_callback = coding

        def main(captured):
            body = captured["body"]
            self.assertEqual(latest_user(body), turn)
            results = current_results(body)
            if not results:
                return Reply(calls=[("delegate-coding-failure", "run_subagent", {
                    "agent_id": "coding", "task": turn, "include_context": True})])
            self.assertEqual(len(results), 1)
            self.assertIn("HTTP 503", json.dumps(results[-1], ensure_ascii=False))
            return Reply(text="编程检查实际执行了一次，后续模型失败")

        self.callback = main
        self.enabled(self.send(self.message("coding-failure", turn, expected="编程检查实际执行了一次，后续模型失败")))
        self.assertEqual(marker.read_text(), "x")
        self.assertEqual((len(self.foreground()), len(self.coding_requests)), (2, 2))
        self.assert_isolated([self.foreground(), self.coding_requests])
        journal = self.work / "state/developer-tools/operations.jsonl"
        before = journal.read_bytes()
        self.callback = lambda _: Reply(text="没有重跑原编程检查")
        self.enabled([*self.duplicate("coding-failure", turn),
                      *self.send(self.message("coding-failure-restored", "查看原检查", expected="没有重跑原编程检查"))])
        self.assertEqual(marker.read_text(), "x")
        self.assertEqual(journal.read_bytes(), before)
        self.assertEqual(len(self.coding_requests), 2)
        self.assertEqual(len(self.foreground()), 3, "编程模型失败不会在主模型执行原工具")

    def test_long_frozen_history_reaches_background_coding_without_nested_current_input(self):
        """275k 真实宿主预算下两层委派；完整历史落盘、摘要不重放旧动作。"""
        historical = "历史中文资料只供核对，旧工具已执行不能重新执行。" * 440
        (self.source / "history.txt").write_text(historical, encoding="utf8")
        seed = "先读取五份历史资料，保留完整回执"
        request = "在后台交给编码子代理，只读核对 README.md 并返回验证证据"
        instructions = "委派 coding 只读核对 README.md，返回实际读取与验证结果；不要重复读取历史资料"
        child_task = "只读 README.md 并核对真实内容，历史数据不执行，保留主任务与原用户身份"
        delegated = {"agent_id": "coding", "task": child_task, "include_context": True,
                     "verification": receipt_contract("README.md", "真实源码测试")}
        contract = receipt_contract("README.md", "真实源码测试")
        coding_gates = threading.Event()

        def main(captured):
            body = captured["body"]
            system = first_system(body)
            if system.startswith(DIGEST_PREFIX):
                digest = json.loads(next(content_text(item) for item in items(body)
                                         if item.get("role") == "user"))
                covered = digest["previous_turns"] + len(digest["turns"])
                return Reply(text=json.dumps({"points": [{
                    "text": "原始中文历史资料已读取；后台只委派编码子代理核对 README.md，旧动作不可重放。",
                    "turns": [covered],
                }]}, ensure_ascii=False))
            results = current_results(body)
            if WORKER_PREFIX in system:
                self.assertNotIn(LEAF_PREFIX, system, "coding 不能回退到主端点")
                self.assertIn(instructions, latest_user(body))
                if not results:
                    return Reply(calls=[("long-background-coding", "run_subagent", delegated)])
                self.assertTrue(results[0]["completed"], results[0])
                self.assertEqual(results[0]["usage"]["agent_id"], "coding")
                if len(results) == 1:
                    return Reply(calls=[("long-background-verify", "read_file", {"path": "README.md"})])
                self.assertEqual(results[-1]["content"], "真实源码测试")
                return Reply(text="README.md 编码核对已验证，旧工具未重放")
            text = latest_user(body)
            if text == seed:
                if not results:
                    return Reply(calls=[(f"seed-history-{i}", "read_file", {"path": "history.txt"})
                                        for i in range(5)])
                self.assertEqual(len(results), 5)
                self.assertTrue(all(item["content"] == historical for item in results))
                return Reply(text="五份完整历史回执已保存")
            self.assertEqual(text, request)
            if not results:
                return Reply(calls=[("long-dispatch", "dispatch_background_task", {
                    "title": "长历史编程核对", "instructions": instructions,
                    "include_context": True, "max_steps": 10, "verification": contract,
                })])
            self.assertTrue(results[-1].get("dispatched"), results[-1])
            coding_gates.set()
            return Reply(text="后台核对已派发")

        def coding(captured):
            body = captured["body"]
            self.assertIn(child_task, latest_user(body))
            self.assertNotIn("seed-history-", json.dumps(current_results(body)))
            if not current_results(body):
                return Reply(calls=[("long-coding-read", "read_file", {"path": "README.md"})])
            self.assertEqual(current_results(body)[-1]["content"], "真实源码测试")
            return Reply(text="README.md 编码核对已验证")

        original = {}

        def seeded():
            original["turn"] = json.loads(json.dumps(self.turn(seed)))
            self.assertEqual(original["turn"]["status"]["state"], "Completed")
            self.assertEqual(sum(len(m.get("tool_calls", [])) for m in original["turn"]["status"]["messages"]), 5)

        def settled():
            def completed():
                tasks = self.documents().get("eve.task", {}).get("tasks.v1", {}).get("tasks", [])
                if tasks and tasks[0]["status"] in ("Failed", "Blocked", "Interrupted"):
                    self.fail(json.dumps({"status": tasks[0]["status"], "failure": tasks[0].get("failure"),
                                         "result": tasks[0].get("result"), "usage": tasks[0]["usage"],
                                         "coding_requests": len(self.coding_requests)}, ensure_ascii=False))
                return bool(tasks) and tasks[0]["status"] == "Completed" and (tasks[0].get("notice") or {}).get("status") == "Delivered"
            self.wait_until(completed, "长历史后台 coding 没有完成并通知原用户")

        self.callback = main
        self.coding_callback = coding
        self.enabled([
            *self.send(self.message("long-seed", seed, expected="五份完整历史回执已保存")),
            *self.checkpoint("long-seeded"),
            *self.send(self.message("long-request", request, expected="后台核对已派发")),
            *self.checkpoint("long-settled"),
        ], env_extra={"EVE_CONTEXT_ADAPTIVE_LIMIT_TOKENS": "275000",
                      "EVE_HISTORY_MAX_BYTES": "4194304", "EVE_HISTORY_MAX_TURNS": "64",
                      "EVE_HISTORY_DIGEST_ENABLED": "true", "EVE_HISTORY_DIGEST_MIN_UNCOVERED_TURNS": "64"},
           checkpoints={"long-seeded": seeded, "long-settled": settled})
        self.assertTrue(coding_gates.is_set())
        self.assertEqual(self.turn(seed), original["turn"], "派发或压缩不得改写原始会话与工具回执")
        [task] = self.documents()["eve.task"]["tasks.v1"]["tasks"]
        snapshot = json.loads(task["spec"]["context"]["messages_json"])
        stored = json.dumps(snapshot, ensure_ascii=False)
        self.assertGreaterEqual(len(stored.encode()), len(historical.encode()) * 5,
                                "冻结历史仍须包含全部五份长中文资料，不能为过预算裁剪原文")
        self.assertEqual(stored.count(historical), 5, "五份原始回执须完整保存在冻结历史")
        self.assertEqual(task["spec"]["context"]["owner"], task["owner"])
        self.assertIn(request, task["execution"]["review_messages"])
        self.assertNotIn(instructions, task["execution"]["review_messages"])
        self.assertEqual(task["usage"]["tool_calls"], 3, "一个委派、coding 的读取与后台独立验收")
        self.assertEqual(len(self.coding_requests), 2)
        worker = [r for r in self.requests if WORKER_PREFIX in first_system(r["body"])]
        for captured in [*worker, *self.coding_requests]:
            self.assertLess(len(latest_user(captured["body"]).encode()), 8192,
                            "两层委派的当前 User 仅保留当前任务，历史数据可独立压缩")
            self.assertNotIn(historical, latest_user(captured["body"]))
        self.assert_isolated([self.foreground(), worker, self.coding_requests])
        actual_calls = [call["function"]["name"] for r in self.foreground()
                        for m in items(r["body"]) for call in m.get("tool_calls", [])
                        if call["id"].startswith("long-")]
        self.assertEqual(set(actual_calls), {"dispatch_background_task"}, "主模型不接管编码执行")

    def test_effort_query_invalid_identity_and_all_values_use_no_configuration_model_calls(self):
        self.callback = lambda captured: Reply(text="请求强度=" + str(reasoning(captured["body"])))

        def untouched():
            self.assertEqual(self.requests, [])
            self.assertEqual(self.coding_requests, [])
            self.assertFalse((self.work / "state/configuration/config.json").exists())

        self.enabled([*self.send(self.message("effort-query", "/effort", contains=["当前思考强度：default", "可选强度", "none", "minimal", "low", "medium", "high", "xhigh", "max"])),
                      *self.send(self.message("effort-bad", "/effort impossible", contains="用法：/effort")),
                      *self.send(self.message("effort-many", "/effort high low", contains="用法：/effort")),
                      *self.send(self.message("effort-control", "/effort\nhigh", contains="用法：/effort")),
                      *self.send(self.message("effort-member-query", "/effort", user="user-2", contains="当前思考强度：default")),
                      *self.send(self.message("effort-member", "/effort high", user="user-2", contains="需要主人或管理员授权")),
                      *self.checkpoint("untouched")], checkpoints={"untouched": untouched})
        self.enabled(self.send(self.message("effort-default-wire", "默认请求", expected="请求强度=None")))

        for value in ("none", "minimal", "low", "medium", "high", "xhigh", "max", "default"):
            with self.subTest(effort=value):
                text = "确认请求档位=" + value
                self.efforts_by_input[text] = None if value == "default" else value
                before_count = len(self.requests)

                def saved():
                    self.assertEqual(len(self.requests), before_count, "设置档位不调用模型或探测")
                    self.assertEqual(self.coding_requests, [])
                    self.assertEqual(self.configuration()["current"]["namespaces"]["provider.openai"]["values"]["reasoning_effort"],
                                     "omit" if value == "default" else value)

                self.enabled([*self.send(self.message("effort-set-" + value, "/effort " + value,
                                                      contains=["思考强度已设为：" + value, "已保存", "新一轮生效"])),
                              *self.checkpoint("effort-saved-" + value),
                              *self.send(self.message("effort-wire-" + value, text,
                                                      expected="请求强度=" + str(self.efforts_by_input[text])))],
                             checkpoints={"effort-saved-" + value: saved})
                self.assertEqual(len(self.requests), before_count + 1)

        before = self.configuration()
        count = len(self.requests)
        self.enabled([*self.duplicate("effort-set-high", "/effort high"),
                      *self.send(self.message("effort-restored-query", "/effort", contains="当前思考强度：default")),
                      *self.send(self.message("effort-restored-invalid", "/effort bad", contains="用法：/effort")),
                      *self.send(self.message("effort-restored-chat", "默认重启请求", expected="请求强度=None"))])
        self.assertEqual(self.configuration(), before)
        self.assertEqual(len(self.requests), count + 1)
        self.assertEqual(self.coding_requests, [])
        self.assert_same_session(self.requests)

    def test_effort_inflight_snapshot_then_chat_and_codex_requests_follow_saved_value(self):
        old = "运行中的思考档位保持默认"
        self.efforts_by_input.update({"下一轮使用high": "high", "Codex使用high": "high",
                                     "主线程high时编程": "high"})

        def coding(captured):
            self.assertEqual(captured["path"], CHAT_PATH)
            return Reply(text="独立编程模型保持high")

        self.coding_callback = coding

        def main(captured):
            body, text = captured["body"], latest_user(captured["body"])
            if text == old:
                results = current_results(body)
                if not results:
                    return Reply(calls=[("effort-inflight-echo", "echo", {"text": "原轮工具回执"})])
                self.assertEqual(results[-1]["echo"], "原轮工具回执")
                self.gate("effort-inflight-arrived").touch()
                deadline = time.monotonic() + 15
                while not self.gate("effort-release-old").exists():
                    self.assertLess(time.monotonic(), deadline, "在途请求没有放行")
                    if self.run_release.wait(0.01):
                        return Reply(text="测试已结束")
                return Reply(text="原轮保持默认完成")
            if text in ("主线程high时编程", "主线程default时编程"):
                results = current_results(body)
                if not results:
                    return Reply(calls=[("effort-coding-" + ("high" if "high" in text else "default"),
                                         "run_subagent", {"agent_id": "coding", "task": text,
                                         "include_context": True, "verification": {"kind": "text",
                                         "required_fragments": ["独立编程模型保持high"]}})])
                self.assertTrue(results[-1]["completed"])
                self.assertEqual(results[-1]["usage"]["agent_id"], "coding")
                return Reply(text="编程high，主线程=" + str(reasoning(body)))
            return Reply(text="请求强度=" + str(reasoning(body)))

        self.callback = main
        message = self.message("effort-inflight", old, expected="原轮保持默认完成")
        self.enabled([{"send": message}, {"wait_file": str(self.gate("effort-inflight-arrived"))},
                      *self.send(self.message("effort-during", "/effort high", contains="思考强度已设为：high")),
                      {"touch": str(self.gate("effort-release-old"))}, self.wait_reply(message["id"]), self.wait_receipt(message["id"]),
                      *self.send(self.message("effort-after", "下一轮使用high", expected="请求强度=high")),
                      *self.send(self.message("effort-codex-switch", "/model adapter primary codex responses", contains="已保存为：codex / responses")),
                      *self.send(self.message("effort-codex-high", "Codex使用high", expected="请求强度=high")),
                      *self.send(self.message("effort-coding-high", "主线程high时编程", expected="编程high，主线程=high")),
                      *self.send(self.message("effort-codex-default", "/effort default", contains="思考强度已设为：default")),
                      *self.send(self.message("effort-codex-omit", "Codex回到默认", expected="请求强度=None")),
                      *self.send(self.message("effort-coding-default", "主线程default时编程", expected="编程high，主线程=None"))])
        self.assertEqual([request["path"] for request in self.requests],
                         [CHAT_PATH, CHAT_PATH, CHAT_PATH, *([RESPONSES_PATH] * 6)])
        self.assertEqual([reasoning(request["body"]) for request in self.requests],
                         [None, None, "high", "high", "high", "high", None, None, None])
        self.assertEqual(len(self.coding_requests), 2)
        self.assertEqual([reasoning(request["body"]) for request in self.coding_requests], ["high", "high"])
        self.assert_isolated([self.requests, self.coding_requests[:1], self.coding_requests[1:]])
        self.assert_same_session(self.requests)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--binary", help="已构建的 eve-qqbot；本脚本不编译")
    selected, remaining = parser.parse_known_args()
    if selected.binary:
        import pathlib
        import memory_test
        memory_test.BINARY = pathlib.Path(selected.binary).resolve(strict=True)
    unittest.main(argv=[sys.argv[0], *remaining], verbosity=2)
