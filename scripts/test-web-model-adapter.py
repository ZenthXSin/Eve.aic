"""独立 eve-web 实际工具入口到模型 wire；仅使用临时状态与本地模型替身。"""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import unittest


SPEC = importlib.util.spec_from_file_location(
    "web_chat_adapter_fixture", Path(__file__).with_name("test-web-chat.py"))
web_chat = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(web_chat)
from fixture_auth import configure_fixture_progress
from model_adapter_fixture import (
    CHAT_PATH, RESPONSES_PATH, MODEL_KEY, Reply, WireAcceptance,
    adapter_call, current_results, latest_user,
)


CONVERSATION = "a" * 32
OTHER_CONVERSATION = "c" * 32


class WebModelAdapterAcceptance(WireAcceptance):
    # 只复用现有 Web HTTP/退出握手；不继承其开发工具场景或审查模型替身。
    web = web_chat.WebChatAcceptance.web
    request = web_chat.WebChatAcceptance.request
    finish_web = web_chat.WebChatAcceptance.finish_web
    wait_web_ready = web_chat.WebChatAcceptance.wait_web_ready
    startup_events = web_chat.WebChatAcceptance.startup_events
    assert_startup_complete = web_chat.WebChatAcceptance.assert_startup_complete
    stop_web = web_chat.WebChatAcceptance.stop_web

    def setUp(self):
        super().setUp()
        self.child = None
        self.run_number = 0
        self.logs = []
        self.source = self.work / "workspace"
        self.source.mkdir()
        # Web 默认启用交流学习；仅为这两个合成会话预置已有的持久暂停模式。
        # 后台交流学习单独验收，这里每个模型请求必须属于适配器切换的主对话。
        modes = {"version": 1, "modes": [
            {"scope": {"session_id": "web:chat:" + conversation, "user_id": "web:owner"},
             "enabled": False}
            for conversation in (CONVERSATION, OTHER_CONVERSATION)
        ]}
        self.state_path.parent.mkdir()
        self.state_path.write_text(json.dumps({"version": 1, "entries": {
            "eve.training": {"modes.v1": list(json.dumps(modes).encode())},
        }}), encoding="utf8")

    def start_web(self):
        self.run_number += 1
        self.log_path = self.work / f"web-{self.run_number}.log"
        self.logs.append(self.log_path)
        self.log = self.log_path.open("w", encoding="utf8")
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP")
               if key in os.environ}
        env.update(self.model_environment())
        env["EVE_WEB_TOKEN"] = web_chat.PANEL_TOKEN
        self.child = subprocess.Popen([
            str(web_chat.WEB), "--listen", "127.0.0.1:0", "--state-dir", str(self.work / "state"),
            "--agent", str(web_chat.ROOT / "AGENT.md")], cwd=self.source,
            env=configure_fixture_progress(env), stdout=subprocess.PIPE, stderr=self.log)
        self.wait_web_ready()

    def tearDown(self):
        self.stop_web()
        try:
            for path in self.logs:
                log = path.read_text(encoding="utf8")
                self.assertNotIn(MODEL_KEY, log)
                self.assertNotIn(web_chat.PANEL_TOKEN, log)
            modes = self.documents()["eve.training"]["modes.v1"]["modes"]
            self.assertEqual(len(modes), 2)
            self.assertTrue(all(mode["enabled"] is False for mode in modes))
        finally:
            super().tearDown()

    def send_and_finish(self, text, number, expected, conversation=CONVERSATION):
        request = self.request(text, conversation, format(number, "032x"))
        status, receipt = self.web("/api/chat/send", request)
        self.assertEqual(status, 202, receipt)
        state = self.finish_web(request)
        self.assertEqual(state["status"], "completed", state)
        self.assertEqual(state["text"], expected)
        return request, state

    def test_owner_switches_both_directions_keep_inflight_sessions_and_restart_receipts(self):
        to_codex = "请把主模型适配器切为 Codex Responses"
        to_chat = "请把主模型适配器切回 OpenAI Chat"
        codex_answer = "Codex 配置已保存。\n当前工具轮继续使用捕获的 Chat 适配器。"
        chat_answer = "Chat 配置已保存，当前 Codex 工具轮已完成。"

        def respond(captured):
            body, path = captured["body"], captured["path"]
            text, results = latest_user(body), current_results(body)
            if text == to_codex:
                self.assertEqual(path, CHAT_PATH)
                if not results:
                    return Reply(calls=[adapter_call("codex", "responses", "web-to-codex")])
                self.assertEqual(len(results), 1)
                self.assert_saved(results[0], "codex", "responses")
                return Reply(text=codex_answer)
            if text == to_chat:
                self.assertEqual(path, RESPONSES_PATH)
                if not results:
                    self.assertNotIn("x-codex-turn-state", captured["headers"])
                    return Reply(calls=[adapter_call("openai", "chat", "web-to-chat")],
                                 turn_state="web-synthetic-turn-state")
                self.assertEqual(len(results), 1)
                self.assert_saved(results[0], "openai", "chat")
                self.assertEqual(captured["headers"]["x-codex-turn-state"],
                                 "web-synthetic-turn-state")
                return Reply(text=chat_answer)
            expected_path = {
                "重启后走 Codex": RESPONSES_PATH,
                "另一个独立 Web 会话": RESPONSES_PATH,
                "切回后的下一轮": CHAT_PATH,
                "第二次重启后的下一轮": CHAT_PATH,
            }[text]
            self.assertEqual(path, expected_path)
            self.assertNotIn("x-codex-turn-state", captured["headers"])
            return Reply(text="已核实：" + text)

        self.callback = respond
        self.start_web()
        self.assertEqual(self.web("/api/chat")[1]["role"], "owner")
        codex_request, _ = self.send_and_finish(to_codex, 1, codex_answer)
        codex_config = self.assert_adapter_config("codex", "responses")
        self.assertEqual([request["path"] for request in self.requests], [CHAT_PATH, CHAT_PATH])
        self.stop_web(graceful=True)
        self.start_web()
        self.assertEqual(self.web("/api/chat/send", codex_request)[1]["status"], "completed")
        self.assertEqual(len(self.requests), 2, "已保存的 Web 请求重启不重复切换或调用模型")
        self.assertEqual(self.assert_adapter_config("codex", "responses"), codex_config)
        self.send_and_finish("重启后走 Codex", 2, "已核实：重启后走 Codex")
        self.send_and_finish("另一个独立 Web 会话", 3, "已核实：另一个独立 Web 会话",
                             conversation=OTHER_CONVERSATION)
        chat_request, _ = self.send_and_finish(to_chat, 4, chat_answer)
        chat_config = self.assert_adapter_config("openai", "chat")
        self.assertEqual(chat_config["revision"], codex_config["revision"] + 1)
        self.send_and_finish("切回后的下一轮", 5, "已核实：切回后的下一轮")
        self.stop_web(graceful=True)
        self.start_web()
        self.assertEqual(self.web("/api/chat/send", chat_request)[1]["status"], "completed")
        self.assertEqual(len(self.requests), 7)
        self.send_and_finish("第二次重启后的下一轮", 6, "已核实：第二次重启后的下一轮")
        self.assertEqual(self.assert_adapter_config("openai", "chat"), chat_config)
        self.assertEqual([request["path"] for request in self.requests], [
            CHAT_PATH, CHAT_PATH, RESPONSES_PATH, RESPONSES_PATH,
            RESPONSES_PATH, RESPONSES_PATH, CHAT_PATH, CHAT_PATH,
        ], "实际入口不产生额外模型审查、目录探测或回退请求")
        self.assert_same_session([request for index, request in enumerate(self.requests) if index != 3])
        for name in ("session-id", "thread-id"):
            self.assertNotEqual(self.requests[2]["headers"][name], self.requests[3]["headers"][name])
        self.assertNotEqual(self.requests[2]["body"]["prompt_cache_key"],
                            self.requests[3]["body"]["prompt_cache_key"])
        self.assertIn(to_codex, json.dumps(self.requests[2]["body"], ensure_ascii=False))
        self.assertNotIn(to_codex, json.dumps(self.requests[3]["body"], ensure_ascii=False))

    def test_web_and_model_identity_fields_never_grant_adapter_write(self):
        request = self.request("核验模型身份字段", CONVERSATION, "d" * 32)

        def respond(captured):
            self.assertEqual(captured["path"], CHAT_PATH)
            results = current_results(captured["body"])
            if not results:
                return Reply(calls=[adapter_call("codex", "responses", "web-forged-owner", user_id="web:owner")])
            self.assertEqual(len(results), 1)
            self.assertIn("error", results[0])
            self.assertFalse(results[0].get("saved", False))
            return Reply(text="模型参数不能伪造平台身份。")

        self.callback = respond
        self.start_web()
        self.assertEqual(self.web("/api/chat/send", request, token="invalid")[0], 401)
        self.assertEqual(self.web("/api/chat/send", request, origin="https://other.example")[0], 403)
        for field, value in (("user_id", "web:owner"), ("role", "owner")):
            self.assertEqual(self.web("/api/chat/send", {**request, field: value})[0], 400)
        self.assertEqual(self.requests, [])
        self.assertEqual(self.web("/api/chat/send", request)[0], 202)
        state = self.finish_web(request)
        self.assertEqual((state["status"], state["text"]),
                         ("completed", "模型参数不能伪造平台身份。"))
        self.assertEqual(len(self.requests), 2)
        self.assertFalse((self.work / "state/configuration/config.json").exists())


if __name__ == "__main__":
    unittest.main()
