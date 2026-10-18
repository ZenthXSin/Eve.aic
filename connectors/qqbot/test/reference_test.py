"""实际 eve-qqbot 的回复时参考资料验收；只连接本地模型替身和 fake bridge。

--reference 装配的资料在回复中可查：模型先 search_reference，再 read_reference 读带行号的原文，
最后按工具结果作答；--max-tool-rounds 放宽单轮工具批次。资料目录无效时在连接 QQ 之前拒绝启动。
确定性替身只证明宿主契约与工具往返，不代表真实模型会主动查证。
"""
import json
import os
import subprocess
import unittest

import interest_test
from memory_test import BINARY, FAKE, ROOT

CORPUS = ROOT / "vendor/eve-portable-knowledge"
REVISION = "0123456789abcdef0123456789abcdef01234567"
QUESTION = "修复投影的回复间隔是多少？"


def base():
    return interest_test.InterestAcceptance


def tool_names(body):
    return {entry["function"]["name"] for entry in body.get("tools", [])}


def tool_results(body):
    messages = body["messages"]
    current = max(index for index, message in enumerate(messages) if message["role"] == "user")
    return [json.loads(m["content"]) for m in messages[current + 1:] if m["role"] == "tool"]


def tool_call(name, arguments, index):
    return {"choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
        "role": "assistant", "content": None, "tool_calls": [
            {"id": f"call-{index}", "type": "function",
             "function": {"name": name, "arguments": json.dumps(arguments, ensure_ascii=False)}}]}}]}


def text(content):
    return {"choices": [{"index": 0, "finish_reason": "stop", "message": {
        "role": "assistant", "content": content}}]}


class ReferenceAcceptance(unittest.TestCase):
    for _name in ("documents", "memory", "snapshots", "evidence", "interactions", "gate", "arrived",
                  "checkpoint", "wait_until", "message", "wait_reply", "wait_receipt", "send",
                  "classify", "reply", "default_observe", "kind", "run_eve"):
        locals()[_name] = getattr(base(), _name)
    del _name

    def setUp(self):
        base().setUp(self)
        game = self.work / "game"
        (game / "core/src/blocks").mkdir(parents=True)
        (game / "core/src/blocks/MendProjector.java").write_text(
            "public class MendProjector {\n    public float reload = 250f;\n    public float range = 60f;\n}\n",
            encoding="utf8")
        (game / ".git").mkdir()
        (game / ".git/HEAD").write_text(REVISION + "\n", encoding="utf8")
        self.references = ["--reference", f"game={game}", "--reference", f"eve-portable={CORPUS}"]

    def tearDown(self):
        base().tearDown(self)

    def test_reply_searches_reads_and_cites_reference_lines(self):
        seen = []

        def respond(body, kind, fallback):
            if kind != "chat":
                return fallback
            results = tool_results(body)
            seen.append(results)
            if not results:
                self.assertLessEqual({"search_reference", "read_reference"}, tool_names(body))
                description = next(entry["function"]["description"] for entry in body["tools"]
                                   if entry["function"]["name"] == "search_reference")
                self.assertIn("game（1 个文件，提交 0123456789，顶层 core/）", description)
                self.assertIn("eve-portable（", description)
                return tool_call("search_reference", {"query": "reload", "root": "game"}, 1)
            if len(results) == 1:
                hit = results[0]["hits"][0]
                return tool_call("read_reference", {"root": hit["root"], "path": hit["path"],
                                                    "start_line": hit["line"], "max_lines": 1}, 2)
            excerpt = results[1]
            return text(f"回复间隔是 250 帧，见 {excerpt['root']}/{excerpt['path']}:{excerpt['start_line']}")

        self.provider_response = respond
        self.run_eve([*self.send(self.message(
            "ask", QUESTION, expected="回复间隔是 250 帧，见 game/core/src/blocks/MendProjector.java:2"))],
            interest=False, extra=[*self.references, "--max-tool-rounds", "8"])
        self.assertEqual(len(seen), 3)
        search, excerpt = seen[2]
        self.assertEqual(search["hits"][0], {"root": "game", "path": "core/src/blocks/MendProjector.java",
                                             "line": 2, "text": "public float reload = 250f;"})
        self.assertEqual(excerpt["text"], "2:     public float reload = 250f;\n")
        self.assertEqual((excerpt["revision"], excerpt["total_lines"]), (REVISION, 4))

    def test_inherited_corpus_is_searchable_from_replies(self):
        found = []

        def respond(body, kind, fallback):
            if kind != "chat":
                return fallback
            results = tool_results(body)
            if not results:
                return tool_call("search_reference", {"query": "ContentParser", "root": "eve-portable",
                                                      "max_results": 5}, 1)
            found.extend(results[0]["hits"])
            return text("查到了")

        self.provider_response = respond
        self.run_eve([*self.send(self.message("corpus", "ContentParser 在哪？", expected="查到了"))],
                     interest=False, extra=self.references)
        self.assertTrue(found)
        self.assertTrue(all(hit["root"] == "eve-portable" for hit in found))
        self.assertTrue(all((CORPUS / hit["path"]).is_file() for hit in found))

    def test_without_references_no_reference_tools_are_registered(self):
        names = []

        def respond(body, kind, fallback):
            if kind == "chat":
                names.append(tool_names(body))
            return fallback

        self.provider_response = respond
        self.run_eve([*self.send(self.message("plain", "你好", expected="你好"))], interest=False)
        self.assertTrue(names)
        self.assertFalse({"search_reference", "read_reference"} & names[0])

    def test_invalid_reference_directory_refuses_to_start(self):
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", QQBOT_APP_ID="1904159860",
                   EVE_OPENAI_API_KEY="test-model-secret",
                   EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}", EVE_OPENAI_PROTOCOL="chat")
        scenario = self.work / "unused.json"
        scenario.write_text(json.dumps({"script": []}), encoding="utf8")
        for reference, expected in ((f"game={self.work / 'missing'}", "目录无法读取"),
                                    (f"empty={self.work / 'state'}", "参考资料无效")):
            (self.work / "state").mkdir(exist_ok=True)
            result = subprocess.run([str(BINARY), "--state-dir", str(self.work / "state"), "--agent",
                                     str(ROOT / "AGENT.md"), "--bridge-script", str(FAKE), "--bridge-arg",
                                     str(scenario), "--reference", reference],
                                    env=env, capture_output=True, text=True, timeout=30)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(expected, result.stderr)
            self.assertFalse(self.requests, "启动失败前不能请求模型")


if __name__ == "__main__":
    unittest.main()
