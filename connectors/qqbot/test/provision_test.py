"""实际 eve-qqbot 的条件自备验收：开启自主学习却没有实践运行环境时，主人与管理员的对话里提示缺少的条件；
模型自己给出来源调用 acquire_condition，宿主下载、核对摘要并用服务端实际启动一次最小模组，通过才启用；
失败的来源只记原因，重启后自动用上已启用的服务端，实践工具随之出现；普通成员看不到提示也不能取得。

模型替身确定性地给出来源，只证明宿主契约；服务端在本地用替身（设置 EVE_MINDUSTRY_SERVER_JAR 时
`test_real_server_is_acquired_and_verified` 改为下载并实际启动真实服务端）。
"""
import hashlib
import http.server
import json
import os
import sys
import threading
import unittest

import practice_test
import dialogue_tools_test
from dialogue_tools_test import text_response, tool_response

MARKER = '{"kind":"eve.conditions.v1"'
FAKE_JAR = b"PK\x03\x04fake mindustry server for provisioning"
REAL_JAR = os.environ.get("EVE_MINDUSTRY_SERVER_JAR")


def conditions(body):
    found = [m["content"] for m in body["messages"]
             if m["role"] == "system" and isinstance(m.get("content"), str) and MARKER in m["content"]]
    assert len(found) <= 1
    return json.loads(found[0][found[0].index(MARKER):]) if found else None


class ProvisionAcceptance(dialogue_tools_test.DialogueToolsAcceptance):
    for _name in dir(dialogue_tools_test.DialogueToolsAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.files = {"/bad.jar": b"this is not a jar", "/server.jar": FAKE_JAR}
        outer = self

        class Files(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                body = outer.files.get(self.path)
                if body is None:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.file_server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Files)
        threading.Thread(target=self.file_server.serve_forever, daemon=True).start()
        self.base_url = f"http://127.0.0.1:{self.file_server.server_port}"

    def tearDown(self):
        self.file_server.shutdown()
        self.file_server.server_close()
        super().tearDown()

    def tools(self, operations):
        # 每批工具调用使用不同 ID；相同 ID 会被当作同一次调用。
        response = tool_response(operations)
        for call in response["choices"][0]["message"]["tool_calls"]:
            call["id"] += f"-{self.chat_rounds}"
        return response

    def ledger(self):
        return self.documents().get("eve.provision", {}).get("provisions.v1") or {"conditions": {}}

    def start(self, script, fake=True, env_extra=None):
        # 换来源重试需要同一轮里的第二批工具调用。
        more = ["--max-tool-rounds", "4"]
        if fake:
            more += ["--practice-java", sys.executable,
                    "--practice-java-arg", str(practice_test.FAKE_SERVER),
                    "--practice-java-arg", "--record", "--practice-java-arg", str(self.record)]
        self.run_eve(script, research=False, practice=False, more=more, timeout=90, env_extra=env_extra)

    def say(self, mid, text, user="user-1", expected="好"):
        return self.send(self.message(mid, text, user=user, scope="group", target="group-1", expected=expected))

    def test_missing_server_is_acquired_by_eve_verified_and_used_after_restart(self):
        seen = {}

        def respond(body):
            messages = body["messages"]
            current = max(i for i, m in enumerate(messages) if m["role"] == "user")
            text = messages[current]["content"]
            results = self.results(body)
            if not results:
                seen[text] = (conditions(body), self.definitions(body))
            if text == "实践环境呢":
                if not results:
                    return self.tools([("acquire_condition", {
                        "condition": "mindustry-server", "url": f"{self.base_url}/bad.jar",
                        "reason": "实践验证缺少服务端"})])
                if len(results) == 1:
                    self.assertFalse(results[0]["ready"])
                    return self.tools([("acquire_condition", {
                        "condition": "mindustry-server", "url": f"{self.base_url}/server.jar",
                        "sha256": hashlib.sha256(FAKE_JAR).hexdigest(), "reason": "实践验证缺少服务端"})])
                self.assertTrue(results[1]["ready"], results)
                self.assertEqual(results[1]["takes_effect"], "下次启动")
                return text_response("准备好了，重启后生效")
            if text == "我也来试试":
                if not results:
                    return self.tools([("acquire_condition", {
                        "condition": "mindustry-server", "url": f"{self.base_url}/server.jar",
                        "reason": "试试"})])
                self.assertIn("error", results[0])
                return text_response("好")
            return text_response("好")
        self.callback = respond

        self.start([*self.say("ask", "实践环境呢", expected="准备好了，重启后生效"),
                    *self.say("member", "我也来试试", user="user-3"),
                    *self.say("again", "现在呢")])
        missing, tools = seen["实践环境呢"]
        [item] = missing["missing"]
        self.assertEqual(item["condition"], "mindustry-server")
        self.assertIn("实践验证", item["needed_by"])
        self.assertEqual(item["recent_failures"], [])
        self.assertIn("acquire_condition", tools)
        self.assertNotIn("run_practice", tools, "缺少服务端时实践未启用")
        self.assertIsNone(seen["我也来试试"][0], "普通成员看不到条件提示")
        self.assertIsNone(seen["现在呢"][0], "已取得后不再提示")

        entry = self.ledger()["conditions"]["mindustry-server"]
        self.assertEqual(entry["ready"]["sha256"], hashlib.sha256(FAKE_JAR).hexdigest())
        self.assertEqual(entry["ready"]["source"], {"url": f"{self.base_url}/server.jar"})
        self.assertEqual([a["ready"] for a in entry["attempts"]], [True, False], "失败的来源也记下原因")
        self.assertIn("jar", entry["attempts"][1]["detail"])
        stored = self.work / "state" / "conditions" / entry["ready"]["path"]
        self.assertEqual(stored.read_bytes(), FAKE_JAR)
        self.assertFalse(any((self.work / "state" / "conditions" / ".staging").glob("*")), "暂存文件不保留")
        launches = [json.loads(line) for line in self.record.read_text(encoding="utf8").splitlines()]
        self.assertEqual(len(launches), 1, "只为验证启动一次")

        # 重启：没有指定服务端，自动使用自己取得并验证过的版本，实践工具出现；
        # 实践启用后还缺少做动态测试的模组验证器，同样提示。
        self.start(self.say("restart", "重启了吗"))
        missing, tools = seen["重启了吗"]
        self.assertIn("run_practice", tools)
        [item] = missing["missing"]
        self.assertEqual(item["condition"], "mindustry-mod-validator")
        self.assertIn("vendor/mindustry-mod-validator", item["description"], "说明怎样自己构建")

        # 验证器没有下载地址：Eve 在本机构建出文件后用 path 取得；宿主用正确与缺少必填字段的最小模组实际跑一次。
        built = self.work / "build" / "mindustry-mod-validator-1.0.0-all.jar"
        built.parent.mkdir()
        built.write_bytes(b"PK\x03\x04fake validator built by eve")

        def build(body):
            messages = body["messages"]
            text = messages[max(i for i, m in enumerate(messages) if m["role"] == "user")]["content"]
            results = self.results(body)
            if not results:
                seen[text] = (conditions(body), self.definitions(body))
            if text == "验证器也准备一下" and not results:
                return self.tools([("acquire_condition", {
                    "condition": "mindustry-mod-validator", "path": str(built), "reason": "实践需要动态测试"})])
            if text == "验证器也准备一下":
                self.assertTrue(results[0]["ready"], results)
                self.assertIn("错误模组报告", results[0]["evidence"])
                return text_response("验证器也好了")
            return text_response("好")
        self.callback = build
        self.start(self.say("validator", "验证器也准备一下", expected="验证器也好了"))
        validator = self.ledger()["conditions"]["mindustry-mod-validator"]["ready"]
        self.assertEqual(validator["source"], {"local_file": str(built)})

        self.start(self.say("restart-again", "再重启"))
        self.assertIsNone(seen["再重启"][0], "条件都已具备，不再提示")

    def test_switch_off_rejects_acquisition_and_hides_the_hint(self):
        seen = {}

        def respond(body):
            results = self.results(body)
            if not results:
                seen["context"] = conditions(body)
                return self.tools([("acquire_condition", {
                    "condition": "mindustry-server", "url": f"{self.base_url}/server.jar", "reason": "需要"})])
            seen["result"] = results[0]
            return text_response("好")
        self.callback = respond
        self.start(self.say("off", "准备一下"), env_extra={"EVE_PROVISION_ENABLED": "false"})
        self.assertIsNone(seen["context"])
        self.assertIn("关闭", json.dumps(seen["result"], ensure_ascii=False))
        self.assertEqual(self.ledger()["conditions"], {})

    @unittest.skipUnless(REAL_JAR, "需要实际 Mindustry 无头服务端 JAR")
    def test_real_server_is_acquired_and_verified(self):
        with open(REAL_JAR, "rb") as file:
            self.files["/server.jar"] = file.read()
        digest = hashlib.sha256(self.files["/server.jar"]).hexdigest()

        def respond(body):
            results = self.results(body)
            if not results:
                return self.tools([("acquire_condition", {
                    "condition": "mindustry-server", "url": f"{self.base_url}/server.jar",
                    "sha256": digest, "reason": "实践验证缺少服务端"})])
            self.assertTrue(results[0]["ready"], results)
            self.assertIn("build", results[0]["evidence"])
            return text_response("准备好了，重启后生效")
        self.callback = respond
        self.start(self.say("real", "实践环境呢", expected="准备好了，重启后生效"), fake=False)


if __name__ == "__main__":
    unittest.main()
