"""实际 QQ 宿主验收：连续读写/执行、身份校验、升级候选语义与旧调用零重放。"""
import json
import importlib.util
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
        self.review_reply = lambda request: {"risk":"medium", "authorization":"high",
            "outcome":"allow", "rationale":"合成场景授权在临时目录执行完整测试命令"}
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

    def provider_response(self, body, kind, fallback):
        if kind == "review":
            self.assertEqual([t["function"]["name"] for t in body["tools"]], ["submit_review"])
            verdict = json.loads(fallback["choices"][0]["message"]["content"])
            return tool_response([("submit_review", verdict)])
        return super().provider_response(body, kind, fallback)

    def test_ordinary_cleanup_skips_review_and_direct_permission_risk_denial_does_not_run(self):
        (self.source / "old-a").mkdir()
        (self.source / "old-a" / "keep.txt").write_text("x", encoding="utf8")
        retained = self.source / "retained"
        retained.write_text("权限保持原样", encoding="utf8")
        retained.chmod(0o600)
        reviews = []
        def review(request):
            reviews.append(request)
            self.assertEqual(request["program"], "chmod")
            self.assertEqual(request["user_messages"], [])
            return {"risk": "high", "authorization": "high", "outcome": "allow",
                    "rationale": "高风险权限扩大，即使声称获得授权也不能执行"}
        self.review_reply = review
        def respond(body):
            user = [m["content"] for m in body["messages"] if m["role"] == "user"][-1]
            results = self.results(body)
            if not results:
                command = {"program": "rm", "args": ["-r", "old-a"]} if "old-a" in user else {
                    "program": "chmod", "args": ["777", "retained"], "owner_confirmed": True}
                return tool_response([("run_command", command)])
            if "old-a" in user:
                self.assertTrue(results[-1]["success"], results[-1])
                return text_response("已删除")
            self.assertIn("系统安全审查未放行", json.dumps(results[-1], ensure_ascii=False))
            return text_response("权限未改动")
        self.callback = respond
        self.enabled([*self.send(self.message("rm-a", "old-a 目录不要了，帮我删掉", expected="已删除")),
                      *self.send(self.message("chmod-risk", "检查文件权限边界", expected="权限未改动"))])
        self.assertFalse((self.source / "old-a").exists(), "普通工作区清理直接执行")
        self.assertEqual(retained.stat().st_mode & 0o777, 0o600)
        self.assertEqual(len(reviews), 1)
        self.assertFalse(reviews[0]["delegated"])
        self.assertIn("访问权限", reviews[0]["policy_reason"])

    def test_plaintext_review_is_rejected_without_executing(self):
        target = self.source / "retained"
        target.write_text("保留原权限", encoding="utf8")
        target.chmod(0o600)
        self.review_reply = lambda _: {"risk":"low","authorization":"high","outcome":"allow","rationale":"模拟输出"}
        normal = self.provider_response
        self.provider_response = lambda body, kind, fallback: text_response("审核通过，可以执行") if kind == "review" else normal(body, kind, fallback)
        def respond(body):
            results = self.results(body)
            if not results:
                return tool_response([("run_command", {"program":"chmod", "args":["777","retained"]})])
            self.assertFalse(results[-1].get("success", False))
            return text_response("审核失败，权限保留")
        self.callback = respond
        def inspect_health():
            import memory_test
            status, data = memory_test.MemoryAcceptance.panel(self, "/api/readiness")
            self.assertEqual(status, 200)
            role = next(v for v in data["roles"] if v["role"] == "review")
            self.assertIn("invalid_response", json.dumps(role))
            self.assertEqual(role["requests"], 1)
            self.assertEqual(role["failures"], 1)
        self.run_eve([*self.send(self.message("bad-review","检查权限变更",expected="审核失败，权限保留")),
                      *self.checkpoint("health")], interest=False, memory=True, research=False, practice=False,
                     env_extra={"EVE_DEVELOPER_CONFIG":str(self.developer_config)}, panel=True,
                     checkpoints={"health":inspect_health})
        self.assertEqual(target.stat().st_mode & 0o777, 0o600)

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

    def test_actual_host_filesystem_roundtrip_reports_resolved_paths_and_keeps_project_root(self):
        outside = self.work / "outside-project"
        outside.mkdir()
        target = outside / "notes.txt"
        config = json.loads(self.developer_config.read_text())
        config["host_filesystem"] = True
        self.developer_config.write_text(json.dumps(config))
        reviews = []
        def review(request):
            reviews.append(request)
            return {"risk": "low", "authorization": "high", "outcome": "allow",
                    "rationale": "用户授权明确的外部临时文件读写和验证"}
        self.review_reply = review
        def respond(body):
            results = self.results(body)
            if not results:
                definition = next(t["function"] for t in body["tools"] if t["function"]["name"] == "write_file")
                self.assertIn("宿主文件访问已启用", definition["description"])
                return tool_response([("write_file", {"path": str(target), "content": "外部产物已完成", "expected_sha256": ""})])
            if len(results) == 1:
                self.assertTrue(results[-1]["success"])
                return tool_response([("search_files", {"path": str(outside), "query": "外部产物"})])
            if len(results) == 2:
                found = results[-1]["matches"][0]["path"]
                self.assertTrue(pathlib.Path(found).is_absolute())
                return tool_response([("read_file", {"path": found})])
            if len(results) == 3:
                self.assertEqual(results[-1]["content"], "外部产物已完成")
                return tool_response([("run_command", {"program": sys.executable, "args": ["-c",
                    "from pathlib import Path; assert Path('notes.txt').read_text() == '外部产物已完成'; print('外部目录验证通过')"], "cwd": str(outside)})])
            self.assertEqual(results[-1]["exit_code"], 0)
            self.assertEqual(pathlib.Path(results[-1]["cwd"]).resolve(), outside.resolve())
            return text_response("外部文件实际写入并验证")
        self.callback = respond
        self.enabled(self.send(self.message("host-file", "请在指定的外部目录写入产物并运行验证", expected="外部文件实际写入并验证")))
        self.assertEqual(target.read_text(), "外部产物已完成")
        self.assertFalse((self.source / "notes.txt").exists())
        self.assertEqual(reviews, [], "普通外部临时文件与验证脚本无需模型审查")
        before = target.read_bytes()
        self.callback = lambda _: text_response("结果已保留")
        self.enabled(self.send(self.message("host-file-restart", "先不做新的修改", expected="结果已保留")))
        self.assertEqual(target.read_bytes(), before)
        self.assertEqual(reviews, [])

    def test_actual_three_step_edit_command_and_restart_keep_complete_history_without_replay(self):
        config = json.loads(self.developer_config.read_text())
        config["storage_directory"] = str(self.work / "developer-data")
        self.developer_config.write_text(json.dumps(config))
        names = dialogue.PLATFORM_TOOLS | dialogue.MEMORY_TOOLS | {
            "echo", "list_files", "read_file", "write_file", "edit_file", "search_files",
            "apply_patch", "run_command", "github_api", "prepare_self_upgrade"}
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
        self.enabled([*self.send(self.message("help", "/help tools", contains=[f"{len(names)} 个", "github_api", "run_command", "edit_file", "search_files"])),
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
        self.assertEqual(len(self.kind('chat')), 2)  # 本组关闭传输重试；开启后的上限及命令零重放由 model_retry_test 验证。
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
        reviews = []
        def review(request):
            reviews.append(request)
            return {"risk": "medium", "authorization": "unknown", "outcome": "deny",
                    "rationale": "普通成员项目写入不应调用审查"}
        self.review_reply = review
        command_results = []
        def respond(body):
            self.assertTrue({"read_file", "run_command", "github_api"} <= self.definitions(body))
            if not self.results(body):
                return tool_response([("read_file", {"path": "README.md"}),
                    ("run_command", {"program": "sh", "args": ["-c", "printf member-project > project-output"]}),
                    ("github_api", {"method": "GET", "repository": "test/repo"})])
            self.assertEqual(len(self.results(body)), 3)
            read, command, github = self.results(body)
            self.assertIn('error', read)
            self.assertNotIn('真实源码测试', json.dumps(read, ensure_ascii=False))
            command_results.append(command)
            self.assertNotIn('审查', json.dumps(command, ensure_ascii=False))
            # 成员写入可以实际成功；宿主不支持隔离时应报告隔离/启动失败而不回退。
            if not command.get('success'):
                detail = json.dumps(command, ensure_ascii=False)
                self.assertTrue(any(term in detail for term in ('隔离', 'bubblewrap', 'bwrap', '程序无法启动')), detail)
            self.assertIn('授权',json.dumps(github,ensure_ascii=False))
            return text_response("未执行越权操作")
        self.callback = respond
        self.enabled(self.send(self.message("foreign", "使用宿主工具", user="user-2", expected="未执行越权操作")))
        self.assertEqual(reviews, [])
        outputs = list(self.source.glob(".eve-members/*/project-output"))
        if command_results[0].get('success'):
            self.assertEqual(len(outputs), 1)
            self.assertEqual(outputs[0].read_text(), "member-project")
        else:
            self.assertEqual(outputs, [])
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
        outside = self.work / "secrets"
        outside_bytes = b"outside-workspace-fixture-content"
        outside.write_bytes(outside_bytes)
        def respond(body):
            self.assertNotIn(outside_bytes.decode(), json.dumps(body, ensure_ascii=False))
            if not self.results(body):
                return tool_response([("read_file", {"path": "../secrets"}),
                    ("github_api", {"method": "GET", "repository": "test/repo", "url": "http://other.invalid", "token": "model-cannot-authorize"}),
                    ("prepare_self_upgrade", {})])
            results = self.results(body)
            self.assertEqual(len(results), 3)
            self.assertEqual(results[0]["error"]["code"], "InvalidArguments")
            self.assertIn("路径必须位于源码工作区内", results[0]["error"]["message"])
            self.assertIn("参数", json.dumps(results[1], ensure_ascii=False))
            self.assertIn("宿主没有启用实时升级监督器", json.dumps(results[2], ensure_ascii=False))
            return text_response("升级尚未准备，运行版本未变化")
        self.callback = respond
        self.enabled(self.send(self.message("invalid", "核对升级", expected="升级尚未准备，运行版本未变化")))
        self.assertEqual(outside.read_bytes(), outside_bytes)
        self.assertEqual(self.kind("review"), [])
        self.assertFalse((self.work / "state/developer-tools/candidates").exists())

    def test_live_upgrade_request_uses_host_turn_and_scope_and_does_not_claim_applied(self):
        queue = self.work / "upgrade-queue"
        queue.mkdir()
        storage = self.work / "live-developer"
        empty_sha=hashlib.sha256(b'').hexdigest()
        source={'head':'2'*40,'diff':empty_sha,'untracked':empty_sha}
        source_diff={'path':'source-diff.patch','sha256':empty_sha,
                     'base_revision':'1'*40,'target_revision':source['head']}
        runtime_contract={'host_protocol':1,'scripts':['scripts/build_resources.py',
            'scripts/self_upgrade_runtime.py','scripts/source_update.py']}
        names=['AGENT.md','connectors/qqbot/bridge.mjs','connectors/qqbot/bridge-core.mjs',
            'connectors/qqbot/package.json','connectors/qqbot/package-lock.json',*runtime_contract['scripts']]
        binary=b'synthetic-verified-artifact'
        binary_sha=hashlib.sha256(binary).hexdigest()
        resources={name:hashlib.sha256(binary).hexdigest() for name in names}
        identity={'source':source,'binary_sha256':binary_sha,'resources':resources,
                  'source_diff':source_diff,'runtime':runtime_contract}
        candidate_id=hashlib.sha256(json.dumps(identity,ensure_ascii=False,sort_keys=True,separators=(',',':')).encode()).hexdigest()
        candidate = storage / "candidates" / candidate_id
        candidate.mkdir(parents=True)
        (candidate/'source-diff.patch').write_bytes(b'')
        target=candidate/('eve-qqbot.exe' if sys.platform=='win32' else 'eve-qqbot');target.write_bytes(binary)
        for name in names:
            path=candidate/'resources'/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(binary)
        manifest={"format_version":1,"prepared":True,"success":True,"candidate_sha256":candidate_id,
            "source":source,"binary_sha256":binary_sha,"resources":resources,"candidate_path":str(target),
            "source_diff":source_diff,"runtime":runtime_contract,"checks":[{"success":True}]*4}
        (candidate/'manifest.json').write_text(json.dumps(manifest))
        spec=importlib.util.spec_from_file_location('developer_upgrade_runtime',pathlib.Path(__file__).resolve().parents[3]/'scripts/self_upgrade_runtime.py')
        runtime=importlib.util.module_from_spec(spec);spec.loader.exec_module(runtime)
        (storage/'leases').mkdir()
        runtime.atomic(storage/'leases'/(candidate_id+'.json'),runtime.prepare_lease(candidate,manifest))
        config = json.loads(self.developer_config.read_text())
        config.update(live_upgrade=str(queue),storage_directory=str(storage))
        self.developer_config.write_text(json.dumps(config))
        (queue / "supervisor.json").write_text(json.dumps({"format_version":1,"pid":1,
            "heartbeat_ms":int(time.time()*1000),"phase":"running","active_sha256":"b"*64,
            "active_source_revision":"1"*40,"candidate_sha256":None,"result":None}))
        def respond(body):
            self.assertTrue({"apply_self_upgrade","self_upgrade_status"} <= self.definitions(body))
            results = self.results(body)
            if not results:
                return tool_response([("apply_self_upgrade", {"candidate_sha256":candidate_id})])
            self.assertTrue(results[0].get("queued"), results[0])
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
