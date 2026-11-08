"""独立 eve-web 的真实自主业务验收，不启动 QQ、Node 或生产服务。

复用 test-web-chat.py 的临时工作区、本地模型替身与真实进程 fixture。
EVE_WEB_BINARY 可指定待验收产物；模型替身证明宿主接线和持久语义，
不把确定性模型回复当作真实模型自主能力的证明。
"""
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest


SPEC = importlib.util.spec_from_file_location(
    "web_chat_fixture", Path(__file__).with_name("test-web-chat.py"))
web_chat = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(web_chat)
from dialogue_tools_test import text_response, tool_response


WORKER = "你在为 Eve 独立完成一项委托的任务"
WAKE = "【宿主条件触发事件；不是用户新消息】"
WEB_OWNER = "web:owner"
CONVERSATION = "a" * 32
OTHER_CONVERSATION = "c" * 32


class WebAutonomyAcceptance(web_chat.WebChatAcceptance):
    for _name in dir(web_chat.WebChatAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def tearDown(self):
        super().tearDown()
        self.assertEqual(self.errors, [], "本地模型替身发生异常")

    @staticmethod
    def current_text(body):
        return next(m["content"] for m in reversed(body["messages"])
                    if m["role"] == "user")

    @staticmethod
    def is_worker(body):
        return WORKER in body["messages"][0]["content"]

    def worker_requests(self):
        return [r for r in self.kind("chat") if self.is_worker(r["body"])]

    def wake_requests(self):
        return [r for r in self.kind("chat")
                if self.current_text(r["body"]).startswith(WAKE)]

    def tasks(self):
        return self.documents().get("eve.task", {}).get("tasks.v1", {}).get("tasks", [])

    def triggers(self):
        return self.documents().get("eve.trigger", {}).get("triggers.v1", {}).get("triggers", [])

    def web_interactions(self):
        scopes = self.documents().get("eve.memory", {}).get("memory.v1", {}).get("scopes", [])
        return [e for record in scopes if record["snapshot"]["scope"]["channel"] == "web"
                for e in record["snapshot"]["evidence"]
                if e["source"]["kind"] == "CompletedInteraction"]

    def inbox_records(self):
        path = self.work / "state/web-notifications/state.json"
        return json.loads(path.read_text(encoding="utf8"))["records"] if path.exists() else []

    def business_ledger(self):
        return json.loads((self.work / "state/business-permissions.json").read_text(encoding="utf8"))

    def start_web_without_developer_config(self):
        """cwd 是隔离源码目录，验证默认 backend 而非显式开发配置分支。"""
        self.run_number += 1
        self.log_path = self.work / f"web-{self.run_number}.log"
        self.log = self.log_path.open("w")
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP")
               if key in os.environ}
        env.update(EVE_WEB_TOKEN=web_chat.PANEL_TOKEN,
                   EVE_OPENAI_API_KEY="test-model-secret",
                   EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
                   EVE_OPENAI_PROTOCOL="chat", EVE_CONTEXT_ADAPTIVE_LIMIT_TOKENS="0",
                   EVE_CONTEXT_COMPACT_AT_TOKENS="0")
        self.assertNotIn("EVE_DEVELOPER_CONFIG", env)
        self.child = subprocess.Popen([
            str(web_chat.WEB), "--listen", "127.0.0.1:0", "--state-dir", str(self.work / "state"),
            "--agent", str(web_chat.ROOT / "AGENT.md")], cwd=self.source, env=env,
            stdout=subprocess.PIPE, stderr=self.log)
        self.wait_web_ready(timeout=25)

    def send_and_finish(self, text, number, conversation=CONVERSATION):
        request = self.request(text, conversation, format(number, "032x"))
        status, receipt = self.web("/api/chat/send", request)
        self.assertEqual(status, 202, receipt)
        state = self.finish_web(request)
        self.assertEqual(state["status"], "completed", state)
        return state

    def assert_quiet(self, workers=None, wakes=None):
        # 只用于负向重放断言，覆盖两个 500 ms 后台扫描；正向完成靠持久账本门控。
        deadline = time.monotonic() + 1.2
        while time.monotonic() < deadline:
            self.assertIsNone(self.child.poll(), self.log_path.read_text())
            if workers is not None:
                self.assertEqual(len(self.worker_requests()), workers)
            if wakes is not None:
                self.assertEqual(len(self.wake_requests()), wakes)
            time.sleep(.03)

    def test_default_tools_and_preference_use_web_channel(self):
        preference = "回答先给结论，再给核验证据。"
        required = {
            "dispatch_background_task", "list_background_tasks", "cancel_background_task",
            "run_subagent", "create_subagent", "list_subagents", "delete_subagent",
            "manage_subject_focus", "configure_subject_autonomy", "read_exploration", "task_progress", "sleep",
            "set_wakeup_trigger", "list_wakeup_triggers", "cancel_wakeup_trigger",
            "save_workflow_tool", "list_workflow_tools", "run_workflow_tool",
            "manage_business_permissions", "list_skills", "manage_skill",
            "list_forged_tools", "manage_forged_tool", "search_memory", "save_preference",
        }

        def respond(body):
            if self.current_text(body) == "重启后读取我的偏好":
                self.assertIn(preference, json.dumps(body, ensure_ascii=False))
                return text_response("已读取 Web 偏好")
            self.assertTrue(required <= self.definitions(body),
                            sorted(required - self.definitions(body)))
            self.capability_names(body)
            if not self.results(body):
                return tool_response([("save_preference", {"scope": "session", "text": preference})])
            [saved] = self.results(body)
            self.assertEqual(saved["sessions_written"], 1, saved)
            self.assertEqual(saved["failed"], [], saved)
            self.assertEqual(saved["written"][0]["user_id"], WEB_OWNER)
            return text_response("偏好已保存")

        self.callback = respond
        self.start_web()
        self.assertEqual(self.web("/api/chat")[1]["role"], "owner")
        ask = "请记住：" + preference
        self.send_and_finish(ask, 1)
        scopes = self.documents()["eve.memory"]["memory.v1"]["scopes"]
        matched = [s for s in scopes if any(
            p["text"] == preference for p in s["snapshot"]["preferences"])]
        self.assertEqual(len(matched), 1, scopes)
        self.assertEqual(matched[0]["snapshot"]["scope"], {
            "channel": "web", "session_id": "web:chat:" + CONVERSATION, "user_id": WEB_OWNER})
        self.assertFalse(any(s["snapshot"]["scope"]["channel"] == "qq" for s in scopes))
        self.wait_until(lambda: len(self.web_interactions()) == 1,
                        "Web 完成回执没有采集真实交流记忆")
        [interaction] = self.web_interactions()
        self.assertEqual(interaction["source"]["message_id"], "web-request:" + format(1, "032x"))
        self.assertEqual(interaction["source"]["user_text"], ask)
        self.assertEqual(interaction["source"]["assistant_text"], "偏好已保存")
        self.assertGreater(interaction["source"]["turn_id"], 0)
        receipt_path = self.work / "state/web-chat-requests" / f"{CONVERSATION}-{1:032x}.json"
        self.wait_until(lambda: json.loads(receipt_path.read_text())["memory_recorded"],
                        "真实交流已保存，但回执没有结算 memory_recorded")
        self.stop_web(graceful=True)
        self.start_web()
        receipt_bytes = receipt_path.read_bytes()
        receipt_mtime = receipt_path.stat().st_mtime_ns
        before = self.chat_rounds
        duplicate = self.request(ask, request=format(1, "032x"))
        status, receipt = self.web("/api/chat/send", duplicate)
        self.assertEqual((status, receipt["status"]), (202, "completed"), receipt)
        self.assertEqual(self.chat_rounds, before)
        self.assertEqual(self.web_interactions(), [interaction])
        self.assertEqual(receipt_path.read_bytes(), receipt_bytes)
        self.assertEqual(receipt_path.stat().st_mtime_ns, receipt_mtime,
                         "已结算记忆的重复请求不应再次持久写回执")
        self.send_and_finish("重启后读取我的偏好", 2)
        self.wait_until(lambda: len(self.web_interactions()) == 2,
                        "重启后的新对话没有生成独立交流证据")
        self.assertEqual([e for e in self.web_interactions()
                          if e["source"]["message_id"] == interaction["source"]["message_id"]],
                         [interaction])

    def test_background_text_contract_finishes_without_browser_polling_and_is_not_replayed(self):
        result = "后台文本验收：输入有三项，整理结果已完成。"
        contract = {"kind": "text", "required_fragments": [result]}

        def respond(body):
            if self.is_worker(body):
                self.assertFalse(body.get("tools"), "纯文本合同不得开放副作用工具")
                self.assertIn("宿主已冻结以下验收条件", self.current_text(body))
                self.assertIn(result, self.current_text(body))
                return text_response(result)
            if not self.results(body):
                return tool_response([("dispatch_background_task", {
                    "title": "后台文本验收", "instructions": "整理甲、乙、丙三项并给出文本结果。",
                    "max_steps": 1, "include_context": False, "verification": contract,
                })])
            self.assertTrue(self.results(body)[0]["dispatched"], self.results(body))
            return text_response("后台任务已建立")

        self.callback = respond
        self.start_web()
        request = self.request("请在后台整理这三项，完成后通知我")
        self.assertEqual(self.web("/api/chat/send", request)[0], 202)
        # 此后没有浏览器 API 轮询，直接观察宿主提交的任务和通知文件。
        self.wait_until(lambda: len(self.tasks()) == 1 and
                        self.tasks()[0]["status"] == "Completed" and
                        (self.tasks()[0].get("notice") or {}).get("status") == "Delivered",
                        "没有浏览器轮询时后台任务未完成或未保存通知")
        self.wait_until(lambda: len(self.inbox_records()) == 1 and
                        self.inbox_records()[0]["settlement"] == "recorded",
                        "通知正文和本地回执未完整结算")
        [task] = self.tasks()
        self.assertEqual(task["result"], result)
        self.assertEqual(task["execution"]["completion_contract"], {
            **contract, "valid_json": False, "json_required_pointers": []})
        self.assertIsNone(task["spec"].get("context"))
        self.assertEqual(task["usage"], {"tool_calls": 0, "model_requests": 1})
        self.assertEqual(task["owner"], {
            "session_id": "web:chat:" + CONVERSATION, "user_id": WEB_OWNER})
        [attempt] = task["notice"]["attempts"]
        self.assertEqual(attempt["channel"], "local_inbox")
        self.assertEqual(attempt["result"], {
            "stored_locally": {"notification_id": self.inbox_records()[0]["event"]}})
        self.assertEqual(self.web("/api/notifications", token="invalid")[0], 401)
        status, page = self.web("/api/notifications")
        self.assertEqual(status, 200, page)
        self.assertEqual(page["unread"], 1)
        [notification] = page["items"]
        self.assertEqual((notification["kind"], notification["status"], notification["body"]),
                         ("task", "completed", result))
        self.assertEqual(notification["source_id"], task["id"])
        self.assertIsNone(notification["read_at_ms"])
        self.assertEqual(self.web("/api/notifications/read", {"id": notification["id"]})[0], 200)
        read_page = self.web("/api/notifications")[1]
        self.assertEqual(read_page["unread"], 0)
        self.assertIsNotNone(read_page["items"][0]["read_at_ms"])
        self.assertEqual(len(self.worker_requests()), 1)
        self.stop_web(graceful=True)
        self.start_web()
        self.assert_quiet(workers=1)
        self.assertEqual(self.web("/api/notifications")[1], read_page)
        self.assertEqual(self.tasks(), [task])
        status, receipt = self.web("/api/chat/send", request)
        self.assertEqual((status, receipt["status"]), (202, "completed"), receipt)
        self.assertEqual(len(self.worker_requests()), 1)

    def test_workflow_executes_real_guarded_leaf_and_restores_its_receipt(self):
        saved = None
        run = None
        expected_hash = hashlib.sha256((self.source / "README.md").read_bytes()).hexdigest()

        def respond(body):
            nonlocal saved, run
            results = self.results(body)
            if self.current_text(body) == "读取保存的流程回执":
                if not results:
                    return tool_response([("list_workflow_tools", {"run_id": run["run_id"]})])
                self.assertEqual(results[0], run)
                return text_response("已从持久账本读取原流程回执")
            if self.current_text(body) == "在另一会话读取流程":
                if not results:
                    return tool_response([("list_workflow_tools", {"workflow_id": saved["id"]})])
                self.assertIn("error", results[0])
                return text_response("其他会话的流程不可读取")
            if not results:
                return tool_response([("save_workflow_tool", {
                    "name": "读取源码摘要", "description": "读取指定文件并保留实际内容摘要回执",
                    "steps": [{"id": "read-source", "name": "read_file",
                               "arguments": {"path": {"$param": "path"}}}],
                })])
            if len(results) == 1:
                self.assertTrue(results[0]["saved"], results)
                self.assertFalse(results[0]["executed"])
                saved = results[0]["workflow"]
                return tool_response([("run_workflow_tool", {
                    "workflow_id": saved["id"], "revision": saved["revision"],
                    "parameters": {"path": "README.md"},
                })])
            self.assertEqual(len(results), 2)
            run = results[-1]
            self.assertEqual(run["status"], "completed", run)
            self.assertTrue(run["completed"])
            [step] = run["steps"]
            self.assertEqual((step["id"], step["tool"], step["status"]),
                             ("read-source", "read_file", "returned"))
            self.assertEqual(step["result"]["sha256"], expected_hash)
            return text_response("流程已读取真实源码并保存摘要回执")

        self.callback = respond
        self.start_web()
        self.send_and_finish("保存并执行一次源码读取流程", 1)
        self.assertEqual(saved["owner"], {
            "session_id": "web:chat:" + CONVERSATION, "user_id": WEB_OWNER})
        self.stop_web(graceful=True)
        self.start_web()
        self.send_and_finish("读取保存的流程回执", 2)
        self.send_and_finish("在另一会话读取流程", 3, OTHER_CONVERSATION)
        self.assertEqual((self.source / "README.md").read_text(), "真实源码测试")

    def test_armed_time_trigger_restarts_and_runs_control_without_browser(self):
        result = "定时唤醒已在原 Web 会话完成核对。"
        ask = "八秒后唤醒你并核对这条对话"

        def respond(body):
            text = self.current_text(body)
            if text.startswith(WAKE):
                self.assertTrue(any(m.get("content") == ask for m in body["messages"]))
                self.assertTrue(any(m.get("content") == "定时唤醒已保存" for m in body["messages"]))
                return text_response(result)
            self.assertEqual(text, ask)
            if not self.results(body):
                return tool_response([("set_wakeup_trigger", {
                    "title": "Web 定时唤醒", "instructions": "读取原会话并核对提醒。",
                    "conditions": [{"type": "time", "after_seconds": 8}],
                })])
            self.assertTrue(self.results(body)[0]["created"], self.results(body))
            return text_response("定时唤醒已保存")

        self.callback = respond
        self.start_web()
        self.send_and_finish(ask, 1)
        [armed] = self.triggers()
        self.assertEqual(armed["status"], "Armed")
        self.assertEqual(self.inbox_records(), [])
        self.stop_web(graceful=True)
        self.start_web()
        # 重启后不再发送消息、不调用读取 API，等待同一个已保存触发器自动推进。
        self.wait_until(lambda: len(self.triggers()) == 1 and
                        self.triggers()[0]["delivery"] == "StoredLocally",
                        "重启后的原时间触发器未唤醒或未保存本地通知")
        self.wait_until(lambda: len(self.inbox_records()) == 1 and
                        self.inbox_records()[0]["settlement"] == "recorded",
                        "时间触发器通知未结算")
        [trigger] = self.triggers()
        self.assertEqual(trigger["id"], armed["id"])
        self.assertEqual((trigger["status"], trigger["runs"], trigger["result"]),
                         ("Completed", 1, result))
        self.assertEqual(len(self.wake_requests()), 1)
        # 真实 SessionService 读取并验证已归档轮次正文；热索引不包含 input。
        status, history = self.web("/api/session", {"key": trigger["owner"], "limit": 10})
        self.assertEqual(status, 200, history)
        turns = [turn for turn in history["session"]["turns"] if turn["input"].startswith(WAKE)]
        self.assertEqual(len(turns), 1)
        self.assertEqual(turns[0]["status"]["state"], "Completed")
        self.assertIn(result, json.dumps(turns[0], ensure_ascii=False))
        status, page = self.web("/api/notifications")
        self.assertEqual(status, 200, page)
        self.assertEqual(page["unread"], 1)
        [notification] = page["items"]
        self.assertEqual((notification["kind"], notification["source_id"], notification["body"]),
                         ("trigger", trigger["id"], result))
        self.assertIsNone(notification["read_at_ms"])
        self.stop_web(graceful=True)
        self.start_web()
        self.assert_quiet(wakes=1)
        self.assertEqual(self.web("/api/notifications")[1], page)
        self.assertEqual(self.triggers(), [trigger])

    @unittest.skipUnless(sys.platform.startswith("linux"), "进程身份观察使用 Linux /proc")
    def test_default_developer_file_and_process_wakeups_restore_without_new_input(self):
        results_by_title = {"默认目录文件变化": "已核对默认目录的真实文件变化。",
                            "默认进程退出": "已核对指定真实进程实例已经退出。"}
        watched = self.source / "watched.txt"
        watched.write_text("before", encoding="utf8")
        child = subprocess.Popen([sys.executable, "-c", "import sys; sys.stdin.read()"],
                                 stdin=subprocess.PIPE)
        ask = "文件变化和观察的进程退出时，分别唤醒你继续核对"

        def respond(body):
            text = self.current_text(body)
            if text.startswith(WAKE):
                title = next(title for title in results_by_title if title in text)
                self.assertTrue(any(m.get("content") == ask for m in body["messages"]))
                if title == "默认目录文件变化":
                    self.assertEqual(watched.read_text(), "after")
                else:
                    self.assertIsNotNone(child.poll())
                return text_response(results_by_title[title])
            self.assertEqual(text, ask)
            if not self.results(body):
                self.assertTrue({"set_wakeup_trigger", "run_command", "read_file"}
                                <= self.definitions(body))
                return tool_response([
                    ("set_wakeup_trigger", {"title": "默认目录文件变化", "instructions": "核对文件变化。",
                                            "conditions": [{"type": "file_changed", "path": "watched.txt"}]}),
                    ("set_wakeup_trigger", {"title": "默认进程退出", "instructions": "核对该进程实例退出。",
                                            "conditions": [{"type": "process_state", "pid": child.pid,
                                                            "running": False}]}),
                ])
            self.assertEqual(len(self.results(body)), 2)
            self.assertTrue(all(r.get("created") for r in self.results(body)), self.results(body))
            return text_response("两个观察条件均已持久保存")

        self.callback = respond
        try:
            self.start_web_without_developer_config()
            self.send_and_finish(ask, 1)
            armed = self.triggers()
            self.assertEqual(len(armed), 2)
            self.assertTrue(all(t["status"] == "Armed" for t in armed), armed)
            file_trigger = next(t for t in armed if t["spec"]["title"] == "默认目录文件变化")
            self.assertIn("watched.txt", json.dumps(file_trigger["spec"]["conditions"]))
            self.assertEqual(self.inbox_records(), [])
            self.stop_web(graceful=True)
            self.start_web_without_developer_config()
            self.assertEqual({t["id"] for t in self.triggers()}, {t["id"] for t in armed})
            self.assertEqual(len(self.wake_requests()), 0)
            # 宿主重启后只有外部真实条件变化；不再向 Web 发送任何消息或轮询读取 API。
            watched.write_text("after", encoding="utf8")
            child.stdin.close()
            child.wait(timeout=5)
            self.wait_until(lambda: len(self.triggers()) == 2 and
                            all(t["delivery"] == "StoredLocally" for t in self.triggers()),
                            "默认开发后端的文件/进程条件重启后未实际唤醒并存通知")
            self.wait_until(lambda: len(self.inbox_records()) == 2 and
                            all(r["settlement"] == "recorded" for r in self.inbox_records()),
                            "文件/进程触发器的本地通知没有完整结算")
            completed = self.triggers()
            for trigger in completed:
                self.assertEqual((trigger["status"], trigger["runs"]), ("Completed", 1))
                self.assertEqual(trigger["result"], results_by_title[trigger["spec"]["title"]])
            self.assertEqual(len(self.wake_requests()), 2)
            status, page = self.web("/api/notifications")
            self.assertEqual(status, 200, page)
            self.assertEqual(page["unread"], 2)
            self.assertEqual({n["body"] for n in page["items"]}, set(results_by_title.values()))
            self.assertTrue(all(n["kind"] == "trigger" and n["status"] == "completed"
                                and n["read_at_ms"] is None for n in page["items"]))
            self.stop_web(graceful=True)
            self.start_web_without_developer_config()
            self.assert_quiet(wakes=2)
            self.assertEqual(self.web("/api/notifications")[1], page)
            self.assertEqual(self.triggers(), completed)
        finally:
            if child.stdin and not child.stdin.closed:
                child.stdin.close()
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=5)

    def test_business_revoke_blocks_commands_only_in_its_scope_and_survives_restart(self):
        def respond(body):
            text = self.current_text(body)
            results = self.results(body)
            if text == "查询当前业务权限":
                if not results:
                    return tool_response([("manage_business_permissions", {"action": "status"})])
                self.assertFalse(results[0]["business_permitted"], results)
                return text_response("当前会话权限仍已关闭")
            allowed = text == "在另一个会话执行写入"
            filename = "other-scope.txt" if allowed else "revoked-scope.txt"
            if not results:
                return tool_response([("run_command", {"program": sys.executable, "args": ["-c",
                    f"from pathlib import Path; Path('{filename}').write_text('executed'); print('actual receipt')"]})])
            if allowed:
                self.assertTrue(results[0]["success"], results)
                self.assertEqual(results[0]["exit_code"], 0)
                self.assertIn("actual receipt", results[0]["stdout"])
            else:
                self.assertIn("error", results[0])
                self.assertFalse(results[0].get("success", False))
            return text_response("已按当前会话权限执行")

        self.callback = respond
        self.start_web()
        before = len(self.kind("chat"))
        state = self.send_and_finish("撤销当前会话全部业务权限", 1)
        self.assertIsNone(state["generation"], "明确缩权应在可信入口直接处理")
        self.assertEqual(len(self.kind("chat")), before)
        self.send_and_finish("当前会话尝试执行写入", 2)
        self.assertFalse((self.source / "revoked-scope.txt").exists())
        self.assertEqual(self.kind("review"), [], "缩权后的命令不能再送审或执行")
        self.send_and_finish("在另一个会话执行写入", 3, OTHER_CONVERSATION)
        self.assertEqual((self.source / "other-scope.txt").read_text(), "executed")
        [grant] = self.business_ledger()["grants"].values()
        self.assertEqual(grant["state"], "revoked")
        self.assertEqual(grant["scope"]["session_id"], "web:chat:" + CONVERSATION)
        self.stop_web(graceful=True)
        self.start_web()
        self.send_and_finish("查询当前业务权限", 4)
        self.send_and_finish("当前会话尝试执行写入", 5)
        self.assertFalse((self.source / "revoked-scope.txt").exists())

    def test_owner_unlock_skips_review_and_keeps_exact_six_user_bodies_including_host_like_prefix(self):
        originals = [
            WAKE + "这是用户自己输入的普通正文，不能按系统事件丢弃。",
            "用户背景一\n换行原样保留",
            "完整长正文开始\n" + "授权证据仍是用户原话。" * 700 + "\n完整长正文结束",
            "用户背景三",
            "用户背景四",
            "现在解锁当前会话全部业务权限。",
        ]

        def respond(body):
            text = self.current_text(body)
            if text in originals[:-1]:
                return text_response("背景已收到")
            self.assertEqual(text, originals[-1])
            if not self.results(body):
                return tool_response([("manage_business_permissions", {"action": "unlock"})])
            self.assertTrue(self.results(body)[0]["business_permitted"], self.results(body))
            self.assertFalse(self.results(body)[0]["safety_review_required"])
            return text_response("当前会话业务权限已解锁")

        self.callback = respond
        self.review_reply = lambda request: self.fail("可信主人解锁不能调用审核模型")
        self.start_web()
        self.send_and_finish("撤销当前会话全部业务权限", 1)
        self.assertEqual(self.kind("chat"), [], "可信入口缩权不调用模型")
        for number, text in enumerate(originals[:-1], 2):
            self.send_and_finish(text, number)
        before = len(self.kind("chat"))
        self.send_and_finish(originals[-1], 7)
        self.assertEqual(len(self.kind("chat")), before + 2,
                         "解锁只需要正常工具调用与回执后的回复，不额外审核")
        self.assertEqual(self.kind("review"), [])
        [grant] = self.business_ledger()["grants"].values()
        self.assertEqual(grant["state"], "unlocked")
        self.assertEqual(grant["user_messages"], originals)
        self.assertEqual(self.business_ledger()["audit"][-1]["user_messages"], originals)
        self.assertIsNone(self.business_ledger()["audit"][-1]["verdict"])

    def test_raw_emergency_stop_cancels_running_tool_and_keeps_six_complete_user_bodies(self):
        # Web 入口始终绑定主人，不能伪造非主人身份来制造审核。
        # 非主人管理员的在途审核抢占由 business_restriction_ingress_test.py 验证。
        retained = self.source / "retained"
        retained.mkdir()
        (retained / "keep.txt").write_text("keep", encoding="utf8")
        retained.chmod(0o700)
        originals = ["背景正文 " + str(i) for i in range(5)]
        originals[2] += "\n" + "完整原话不能裁剪。" * 700 + "\n正文末尾标记"
        action = "启动受控慢工具，等我释放后再变更 retained 目录权限"
        stop = "立即急停当前会话并取消既有任务"
        started_tool = self.source / "tool-started.txt"
        released_tool = self.source / "tool-release"
        completed_tool = self.source / "tool-completed.txt"
        command = (
            "from pathlib import Path\nimport time\n"
            "marker = Path('tool-started.txt')\n"
            "marker.write_text((marker.read_text() if marker.exists() else '') + 'x')\n"
            "deadline = time.monotonic() + 20\n"
            "while not Path('tool-release').exists() and time.monotonic() < deadline:\n"
            "    time.sleep(.02)\n"
            "if Path('tool-release').exists():\n"
            "    Path('retained').chmod(0o777)\n"
            "    Path('tool-completed.txt').write_text('completed')\n"
        )

        def respond(body):
            text = self.current_text(body)
            if text in originals:
                return text_response("背景已收到")
            self.assertEqual(text, action)
            if not self.results(body):
                return tool_response([("run_command", {
                    "program": sys.executable, "args": ["-c", command]})])
            self.assertFalse(self.results(body)[0].get("success", False), self.results(body))
            return text_response("急停后未改变权限")

        self.callback = respond
        self.review_reply = lambda request: self.fail("可信主人慢工具不能调用审核模型")
        self.start_web()
        for number, text in enumerate(originals, 1):
            self.send_and_finish(text, number)
        pending = self.request(action, request=format(6, "032x"))
        self.assertEqual(self.web("/api/chat/send", pending)[0], 202)
        self.wait_until(started_tool.exists, "慢工具没有实际开始执行")
        self.assertEqual(started_tool.read_text(), "x")
        self.assertFalse(completed_tool.exists())
        self.assertFalse(released_tool.exists())
        baseline = len(self.kind("chat"))
        stop_request = self.request(stop, request=format(7, "032x"))
        started = time.monotonic()
        status, state = self.web("/api/chat/send", stop_request)
        self.assertEqual((status, state["status"]), (202, "completed"), state)
        self.assertLess(time.monotonic() - started, 3, "急停等待了在途慢工具")
        self.assertIsNone(state["generation"])
        self.assertFalse(released_tool.exists())
        self.assertEqual(len(self.kind("chat")), baseline)
        self.assertEqual(self.kind("review"), [])
        ledger = self.business_ledger()
        [grant] = ledger["grants"].values()
        self.assertEqual(grant["state"], "emergency_stopped")
        committed = [e for e in ledger["audit"]
                     if e["action"] == "emergency_stop" and e["outcome"] == "committed"]
        self.assertEqual(len(committed), 1)
        self.assertEqual(committed[0]["user_messages"], originals[-4:] + [action, stop])
        self.assertEqual(grant["user_messages"], originals[-4:] + [action, stop])
        self.assertEqual((retained / "keep.txt").read_text(), "keep")
        self.assertEqual(retained.stat().st_mode & 0o777, 0o700)
        cancelled = self.finish_web(pending)
        self.assertEqual(cancelled["status"], "failed", cancelled)
        self.assertEqual(cancelled["started_tools"], 1)
        released_tool.touch()
        self.assert_quiet(workers=0, wakes=0)
        self.assertFalse(completed_tool.exists(), "已取消的真实工具不能在放行后继续执行")
        self.assertEqual((retained / "keep.txt").read_text(), "keep")
        self.assertEqual(retained.stat().st_mode & 0o777, 0o700)
        self.assertEqual(self.web("/api/chat/send", stop_request)[1]["status"], "completed")
        self.stop_web(graceful=True)
        self.start_web()
        self.assert_quiet(workers=0, wakes=0)
        self.assertEqual(self.web("/api/chat/send", stop_request)[1]["status"], "completed")
        self.assertEqual(len([e for e in self.business_ledger()["audit"]
                              if e["action"] == "emergency_stop" and e["outcome"] == "committed"]), 1)
        self.assertEqual((retained / "keep.txt").read_text(), "keep")
        self.assertEqual(retained.stat().st_mode & 0o777, 0o700)
        self.assertFalse(completed_tool.exists(), "重启不能重放已取消的工具")
        self.assertEqual(started_tool.read_text(), "x")
        self.assertEqual(len(self.kind("chat")), baseline)
        self.assertEqual(self.kind("review"), [])


if __name__ == "__main__":
    unittest.main()
