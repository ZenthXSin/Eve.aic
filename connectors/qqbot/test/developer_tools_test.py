"""实际 QQ 宿主验收：连续读写/执行、身份校验、升级候选语义与旧调用零重放。"""
import json
import hashlib
import pathlib
import sys
import unittest

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
            "operators": [principal], "commands": True, "github": True,
            "self_upgrade": True, "max_tool_rounds": 4}), encoding="utf8")

    def enabled(self, script):
        self.run_eve(script, interest=False, memory=True, research=False, practice=False,
                     env_extra={"EVE_DEVELOPER_CONFIG": str(self.developer_config)})

    def test_actual_three_step_edit_command_and_restart_keep_complete_history_without_replay(self):
        config = json.loads(self.developer_config.read_text())
        config["storage_directory"] = str(self.work / "developer-data")
        self.developer_config.write_text(json.dumps(config))
        names = {"echo", "search_memory", "list_files", "read_file", "write_file", "edit_file",
                 "search_files", "run_command", "github_api", "prepare_self_upgrade"}
        def respond(body):
            self.assertEqual(self.definitions(body), names)
            self.current_capabilities(body)
            results = self.results(body)
            if not results:
                return tool_response([("read_file", {"path": "README.md"})])
            if len(results) == 1:
                self.assertEqual(results[0]["content"], "真实源码测试")
                return tool_response([("edit_file", {"path": "README.md", "old_string": "真实源码测试", "new_string": "已修改并验证", "expected_sha256": results[0]["sha256"]}),
                                      ("write_file", {"path": "notes.txt", "content": "已修改并验证", "expected_sha256": ""})])
            if len(results) == 3:
                self.assertTrue(results[1]["success"])
                self.assertTrue(results[2]["success"])
                return tool_response([("search_files", {"query": "已修改并验证"})])
            if len(results) == 4:
                self.assertEqual(sorted(m["path"] for m in results[3]["matches"]), ["README.md", "notes.txt"])
                return tool_response([("run_command", {"program": sys.executable, "args": ["-c",
                    "from pathlib import Path; assert Path('README.md').read_text(encoding='utf8')=='已修改并验证'; assert Path('notes.txt').read_text(encoding='utf8')=='已修改并验证'; Path('ran.txt').write_text('1'); print('验证通过')"]})])
            self.assertEqual(len(results), 5)
            self.assertEqual(results[-1]["exit_code"], 0)
            self.assertIn("验证通过", results[-1]["stdout"])
            return text_response("实际读写和命令已完成")
        self.callback = respond
        self.enabled([*self.send(self.message("help", "/help tools", contains=["10 个", "github_api", "run_command", "edit_file", "search_files"])),
                      *self.send(self.message("edit", "修改源码并执行验证", expected="实际读写和命令已完成"))])
        self.assertEqual(self.chat_rounds, 5)
        journal = self.work / "developer-data/operations.jsonl"
        self.assertFalse((self.work / "state/developer-tools").exists())
        before = journal.read_bytes()
        def restored(body):
            self.current_capabilities(body)
            self.assertEqual(sum(len(m.get("tool_calls", [])) for m in body["messages"]), 5)
            self.assertEqual(sum(m["role"] == "tool" for m in body["messages"]), 5)
            return text_response("原工具未重放")
        self.callback = restored
        self.enabled(self.send(self.message("restart", "查看之前的工作", expected="原工具未重放")))
        self.assertEqual(journal.read_bytes(), before)
        self.assertEqual((self.source / "ran.txt").read_text(), "1")

    def test_other_qq_user_cannot_use_files_commands_or_github(self):
        def respond(body):
            if not self.results(body):
                return tool_response([("read_file", {"path": "README.md"}),
                    ("run_command", {"program": sys.executable, "args": ["-c", "open('forbidden','w').write('bad')"]}),
                    ("github_api", {"method": "GET", "repository": "test/repo"})])
            self.assertEqual(len(self.results(body)), 3)
            for result in self.results(body):
                self.assertIn("仅供宿主配置的操作者", json.dumps(result, ensure_ascii=False))
            return text_response("未执行越权操作")
        self.callback = respond
        self.enabled(self.send(self.message("foreign", "使用宿主工具", user="user-2", expected="未执行越权操作")))
        self.assertFalse((self.source / "forbidden").exists())
        self.assertFalse((self.work / "state/developer-tools/operations.jsonl").exists())

    def test_invalid_arguments_and_missing_eve_source_never_claim_self_upgrade(self):
        def respond(body):
            if not self.results(body):
                return tool_response([("read_file", {"path": "../secrets"}),
                    ("github_api", {"method": "GET", "repository": "test/repo", "url": "http://other.invalid", "token": "model-cannot-authorize"}),
                    ("prepare_self_upgrade", {})])
            results = self.results(body)
            self.assertIn("路径", json.dumps(results[0], ensure_ascii=False))
            self.assertIn("参数", json.dumps(results[1], ensure_ascii=False))
            self.assertIn("不是 Eve 源码仓库", json.dumps(results[2], ensure_ascii=False))
            return text_response("升级尚未准备，运行版本未变化")
        self.callback = respond
        self.enabled(self.send(self.message("invalid", "核对升级", expected="升级尚未准备，运行版本未变化")))
        self.assertFalse((self.work / "state/developer-tools/candidates").exists())

if __name__ == "__main__":
    unittest.main()
