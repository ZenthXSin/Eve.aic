"""实际 QQ 宿主验收：连续读写/执行、身份校验、升级候选语义与旧调用零重放。"""
import json
import hashlib
import pathlib
import sys
import unittest
import time

import dialogue_tools_test as dialogue
from dialogue_tools_test import tool_response, text_response

class DeveloperAcceptance(dialogue.DialogueToolsAcceptance):
    for _name in dir(dialogue.DialogueToolsAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.source = self.work / "source"
        self.source.mkdir()
        (self.source / "README.md").write_text("真实源码测试", encoding="utf8")
        self.developer_config = self.work / "developer.json"
        identity = json.dumps(["1904159860", "c2c", "user-1", "user-1"], separators=(",", ":")).encode()
        principal = "qq:" + hashlib.sha256(identity).hexdigest()
        self.developer_config.write_text(json.dumps({"workspace": str(self.source.resolve()),
            "operators": [principal], "ordinary_access": False, "commands": True, "github": True,
            "self_upgrade": True, "max_tool_rounds": 4}), encoding="utf8")

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and {"action", "policy_reason", "user_messages"} <= set(decoded):
            return "review"
        return super().classify(decoded, latest)

    def reply(self, kind, decoded, latest):
        if kind == "review":
            return json.dumps(self.review_reply(decoded), ensure_ascii=False)
        return super().reply(kind, decoded, latest)

    def test_prompt_commands_are_reviewed_against_user_words_and_denials_do_not_run(self):
        for name in ["old-a", "old-b"]:
            (self.source / name).mkdir()
            (self.source / name / "keep.txt").write_text("x", encoding="utf8")
        reviews = []
        def review(request):
            reviews.append(request)
            if "old-a" in request["action"]:
                return {"risk": "medium", "authorization": "high", "outcome": "allow", "rationale": "用户明确要求删除 old-a"}
            return {"risk": "medium", "authorization": "unknown", "outcome": "deny", "rationale": "用户没有要求删除 old-b"}
        self.review_reply = review
        def respond(body):
            user = [m["content"] for m in body["messages"] if m["role"] == "user"][-1]
            target = "old-b" if "old-b" in user else "old-a"
            results = self.results(body)
            if not results:
                return {"choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                    "role": "assistant", "content": None, "tool_calls": [
                        {"id": f"call-rm-{target}", "type": "function", "function": {
                            "name": "run_command", "arguments": json.dumps({"program": "rm", "args": ["-r", target]})}}]}}]}
            outcome = json.dumps(results[-1], ensure_ascii=False)
            return text_response("已删除" if results[-1].get("success") else
                                 "没有删除，行动审查未放行" if "行动审查未放行" in outcome else "失败：" + outcome)
        self.callback = respond
        self.enabled([*self.send(self.message("rm-a", "old-a 目录不要了，帮我删掉", expected="已删除")),
                      *self.send(self.message("rm-b", "old-b 里有什么", expected="没有删除，行动审查未放行"))])
        self.assertFalse((self.source / "old-a").exists(), "审查放行后实际执行")
        self.assertTrue((self.source / "old-b" / "keep.txt").exists(), "审查未放行时没有执行")
        self.assertEqual(len(reviews), 2)
        self.assertIn("old-a 目录不要了，帮我删掉", reviews[0]["user_messages"][-1])
        self.assertFalse(reviews[0]["delegated"])
        self.assertIn("递归删除", reviews[0]["policy_reason"])

    def receipt(self, message_id):
        return next(r for r in self.documents()["eve.channel.qqbot"]["receipts.v1"]["entries"] if r["message"]["id"] == message_id)

    def turn(self, text):
        return next(t for session in self.documents()["eve.session"]["sessions.v1"]["sessions"].values() for t in session["turns"] if t["input"] == text)

    def interactions(self):
        memory = self.documents().get("eve.memory", {}).get("memory.v1", {})
        return [e for scope in memory.get("scopes", []) for e in scope["snapshot"]["evidence"] if e["source"]["kind"] == "CompletedInteraction"]

    def enabled(self, script):
        self.run_eve(script, interest=False, memory=True, research=False, practice=False,
                     env_extra={"EVE_DEVELOPER_CONFIG": str(self.developer_config)})

    def test_actual_three_step_edit_command_and_restart_keep_complete_history_without_replay(self):
        config = json.loads(self.developer_config.read_text())
        config["storage_directory"] = str(self.work / "developer-data")
        self.developer_config.write_text(json.dumps(config))
        names = {"clear_context", "clear_all_contexts", "echo", "search_memory", "save_preference", "note_commitment", "settle_commitment", "send_file", "read_attachment", "describe_image", "read_raw_message", "acquire_condition", "read_source", "list_files", "read_file", "write_file", "edit_file",
                 "search_files", "apply_patch", "run_command", "github_api", "prepare_self_upgrade"}
        def respond(body):
            self.assertEqual(self.definitions(body), names)
            self.current_capabilities(body)
            results = self.results(body)
            if not results:
                return tool_response([("read_file", {"path": "README.md"})])
            if len(results) == 1:
                self.assertEqual(results[0]["content"], "真实源码测试")
                return tool_response([("edit_file", {"path": "README.md", "old_string": "真实源码测试", "new_string": "已修改并验证", "expected_sha256": results[0]["sha256"]}),
                                      ("write_file", {"path": "notes.txt", "content": "已修改并验证", "expected_sha256": ""}),
                                      ("apply_patch", {"patch": "*** Begin Patch\n*** Add File: patched.txt\n+已修改并验证\n*** End Patch"})])
            if len(results) == 4:
                self.assertTrue(results[1]["success"])
                self.assertTrue(results[2]["success"])
                self.assertTrue(results[3]["success"])
                self.assertEqual(results[3]["files"][0]["action"], "add")
                return tool_response([("search_files", {"query": "已修改并验证"})])
            if len(results) == 5:
                self.assertEqual(sorted(m["path"] for m in results[4]["matches"]), ["README.md", "notes.txt", "patched.txt"])
                return tool_response([("run_command", {"program": sys.executable, "args": ["-c",
                    "from pathlib import Path; assert Path('README.md').read_text(encoding='utf8')=='已修改并验证'; assert Path('notes.txt').read_text(encoding='utf8')=='已修改并验证'; Path('ran.txt').write_text('1'); print('验证通过')"]})])
            self.assertEqual(len(results), 6)
            self.assertEqual(results[-1]["exit_code"], 0)
            self.assertIn("验证通过", results[-1]["stdout"])
            return text_response("实际读写和命令已完成")
        self.callback = respond
        self.enabled([*self.send(self.message("help", "/help tools", contains=["22 个", "github_api", "run_command", "edit_file", "search_files"])),
                      *self.send(self.message("edit", "修改源码并执行验证", expected="实际读写和命令已完成"))])
        self.assertEqual(self.chat_rounds, 5)
        journal = self.work / "developer-data/operations.jsonl"
        self.assertFalse((self.work / "state/developer-tools").exists())
        before = journal.read_bytes()
        def restored(body):
            self.current_capabilities(body)
            self.assertEqual(sum(len(m.get("tool_calls", [])) for m in body["messages"]), 6)
            self.assertEqual(sum(m["role"] == "tool" for m in body["messages"]), 6)
            return text_response("原工具未重放")
        self.callback = restored
        self.enabled(self.send(self.message("restart", "查看之前的工作", expected="原工具未重放")))
        self.assertEqual(journal.read_bytes(), before)
        self.assertEqual((self.source / "ran.txt").read_text(), "1")

    def test_provider_failure_after_command_notifies_preserves_receipts_and_restart_never_replays(self):
        def first(body):
            self.current_capabilities(body)
            self.failing.add("chat")  # Only the response after the real command fails HTTP 503.
            return tool_response([("run_command", {"program": sys.executable, "args": ["-c",
                "from pathlib import Path; p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); print('ACTUAL_COMMAND_RECEIPT')"]})])
        self.callback = first
        self.enabled(self.send(self.message("failed-command", "执行一次本地检查", contains=["HTTP 503", "已启动 1 次", "本轮已停止", "不再自动重试"])))
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.assertEqual(len(self.kind('chat')),5)  # 一次工具请求 + 后续模型最多四次尝试。
        failed = self.turn("执行一次本地检查")["status"]
        self.assertEqual(failed["state"], "Failed")
        evidence = failed["failure"]["evidence"]
        self.assertEqual(evidence["provider_http_status"], 503)
        self.assertEqual(evidence["tool_names"], ["run_command"])
        self.assertIn("ACTUAL_COMMAND_RECEIPT", json.dumps(evidence["tool_results"]))
        self.assertEqual(len(self.interactions()), 0)
        self.assertEqual(self.receipt("failed-command")["state"], "Sent")  # Sent fault notice, not successful model answer.
        self.failing.clear()
        def restored(body):
            self.current_capabilities(body)
            notice = "\n".join(str(m.get("content", "")) for m in body["messages"])
            self.assertIn("宿主失败记录", notice)
            self.assertIn("HTTP 503", notice)
            self.assertIn("已启动 1 次", notice)
            self.assertNotIn("ACTUAL_COMMAND_RECEIPT", notice)  # Raw failed output is stored, never replayed as instructions/history.
            self.assertFalse(any(m.get("tool_calls") for m in body["messages"]))
            return text_response("此前已执行检查，后续模型回复失败；没有重跑。")
        self.callback = restored
        duplicate = {**self.message("failed-command", "执行一次本地检查"), "expected_type": "finish"}
        self.enabled([{"send": duplicate}, {"wait_command": {"id":"failed-command", "type":"finish"}},
                      *self.send(self.message("after-restart", "刚才的检查进展如何", expected="此前已执行检查，后续模型回复失败；没有重跑。"))])
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.assertEqual(len(self.interactions()), 1)

    def test_image_only_input_saves_reference_and_truthful_limit_without_training_placeholder(self):
        url = "https://media.example.invalid/images/sample.png?synthetic=signed-reference"
        def respond(body):
            content = body["messages"][-1]["content"]
            self.assertIn("收到 1 个图片附件", content)
            self.assertIn("尚未获取像素或进行视觉解析", content)
            self.assertNotIn(url, json.dumps(body))
            return text_response("图片附件收到了，目前尚不能读取图片内容。")
        self.callback = respond
        image = {**self.message("image-only", "", expected="图片附件收到了，目前尚不能读取图片内容。"),
                 "attachments": [{"content_type": "image/png", "url": url}]}
        self.run_eve(self.send(image), interest=False, memory=True, research=False, practice=False,
                     more=["--training"], env_extra={"EVE_DEVELOPER_CONFIG": str(self.developer_config)})
        receipt = self.receipt("image-only")
        self.assertEqual(receipt["state"], "Sent")
        self.assertEqual(receipt["message"]["text"], "")
        self.assertEqual(receipt["message"]["attachments"][0]["url"], url)
        self.assertEqual(len(self.interactions()), 0)
        expression = self.documents().get("eve.training", {}).get("expression.v1", {"scopes": []})
        self.assertEqual(expression["scopes"], [])

    def test_other_qq_user_has_project_tools_but_cannot_read_host_or_use_private_github(self):
        def respond(body):
            if not self.results(body):
                return tool_response([("read_file", {"path": "README.md"}),
                    ("run_command", {"program": "sh", "args": ["-c", "printf member-project > project-output"]}),
                    ("github_api", {"method": "GET", "repository": "test/repo"})])
            self.assertEqual(len(self.results(body)), 3)
            read, command, github = self.results(body)
            self.assertIn('error', read)
            self.assertNotIn('真实源码测试', json.dumps(read, ensure_ascii=False))
            if sys.platform == 'linux' and pathlib.Path('/usr/bin/bwrap').is_file():
                self.assertTrue(command['success'],command)
            else:
                self.assertIn('error',command)
            self.assertIn('授权',json.dumps(github,ensure_ascii=False))
            return text_response("未执行越权操作")
        self.callback = respond
        self.enabled(self.send(self.message("foreign", "使用宿主工具", user="user-2", expected="未执行越权操作")))
        self.assertFalse((self.source / "project-output").exists())
        self.assertTrue((self.work / "state/developer-tools/operations.jsonl").exists())

    def test_explicit_unlimited_runs_forty_batches_and_restart_never_replays_them(self):
        config = json.loads(self.developer_config.read_text())
        config["max_tool_rounds"] = 0
        self.developer_config.write_text(json.dumps(config))
        def respond(body):
            results = self.results(body)
            if len(results) == 40:
                self.assertTrue(all(r["content"] == "真实源码测试" for r in results))
                return text_response("四十批完成")
            reply = tool_response([("read_file", {"path": "README.md"})])
            reply["choices"][0]["message"]["tool_calls"][0]["id"] = f"read-{len(results)}"
            return reply
        self.callback = respond
        self.enabled(self.send(self.message("long-tools", "连续只读核对四十批", expected="四十批完成")))
        self.assertEqual(self.chat_rounds, 41)
        journal = self.work / "state/developer-tools/operations.jsonl"
        before = journal.read_bytes()
        self.callback = lambda body: text_response("此前操作未重放")
        self.enabled(self.send(self.message("long-tools-restored", "查看完成状态", expected="此前操作未重放")))
        self.assertEqual(journal.read_bytes(), before)

    def test_invalid_arguments_and_missing_eve_source_never_claim_self_upgrade(self):
        def respond(body):
            if not self.results(body):
                return tool_response([("read_file", {"path": "../secrets"}),
                    ("github_api", {"method": "GET", "repository": "test/repo", "url": "http://other.invalid", "token": "model-cannot-authorize"}),
                    ("prepare_self_upgrade", {})])
            results = self.results(body)
            self.assertIn("仅限主人", json.dumps(results[0], ensure_ascii=False))
            self.assertIn("参数", json.dumps(results[1], ensure_ascii=False))
            self.assertIn("不是 Eve 源码仓库", json.dumps(results[2], ensure_ascii=False))
            return text_response("升级尚未准备，运行版本未变化")
        self.callback = respond
        self.enabled(self.send(self.message("invalid", "核对升级", expected="升级尚未准备，运行版本未变化")))
        self.assertFalse((self.work / "state/developer-tools/candidates").exists())

    def test_live_upgrade_request_uses_host_turn_and_scope_and_does_not_claim_applied(self):
        queue = self.work / "upgrade-queue"
        queue.mkdir()
        storage = self.work / "live-developer"
        candidate_id = "a" * 64
        candidate = storage / "candidates" / candidate_id
        candidate.mkdir(parents=True)
        (candidate / "manifest.json").write_text(json.dumps({"format_version":1,"prepared":True,"success":True,
            "candidate_sha256":candidate_id,"checks":[{"success":True}]*4}))
        config = json.loads(self.developer_config.read_text())
        config.update(live_upgrade=str(queue),storage_directory=str(storage))
        self.developer_config.write_text(json.dumps(config))
        (queue / "supervisor.json").write_text(json.dumps({"format_version":1,"pid":1,
            "heartbeat_ms":int(time.time()*1000),"phase":"running","active_sha256":"b"*64,
            "candidate_sha256":None,"result":None}))
        def respond(body):
            self.assertTrue({"apply_self_upgrade","self_upgrade_status"} <= self.definitions(body))
            results = self.results(body)
            if not results:
                return tool_response([("apply_self_upgrade", {"candidate_sha256":candidate_id})])
            self.assertTrue(results[0]["queued"])
            self.assertFalse(results[0]["applied"])
            return text_response("切换已排队，尚未声称应用成功")
        self.callback=respond
        self.enabled(self.send(self.message("apply", "应用已验证候选", expected="切换已排队，尚未声称应用成功")))
        request=json.loads((queue/"request.json").read_text())
        self.assertEqual(request["turn_id"],self.turn("应用已验证候选")["id"])
        self.assertEqual(request["requested_by"],config["operators"][0])
        self.assertEqual(request["session_id"],config["operators"][0])

if __name__ == "__main__":
    unittest.main()
