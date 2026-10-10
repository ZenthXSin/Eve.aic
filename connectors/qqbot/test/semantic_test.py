"""实际 eve-qqbot 的语义记忆召回验收；本地模型替身同时提供确定性的 /v1/embeddings。

已送达的交互先由词项记忆导入；开启 --semantic-recall 后，后台为它们建立向量索引。之后一句与历史
没有任何共同字词、但意思相近的话，也能把那段历史作为有来源的低优先级资料交给模型。
向量模型不可用时，召回改用词项召回并在资料中标明，索引不写入。向量替身按概念关键词给出方向，
只证明宿主契约、持久化与降级语义，不代表真实向量模型的召回质量。
"""
import json
import os
import subprocess
import unittest

import memory_test as memory_harness
import recall_test

HISTORY = "以后回答尽量简短一些"
PARAPHRASE = "别写长篇大论"
SEMANTIC_ENV = {"EVE_MODELS_SEMANTIC_ENABLED": "true", "EVE_MODELS_SEMANTIC_PROVIDER": "openai",
                "EVE_MODELS_SEMANTIC_MODEL": "concept-embed", "EVE_MODELS_SEMANTIC_DIMENSIONS": "4"}
CONCEPTS = [["简短", "简洁", "精简", "长篇", "啰嗦"], ["辣", "川菜", "吃"], ["Mindustry", "模组", "游戏"]]


def concept_vector(text):
    return [float(sum(word in text for word in group)) for group in CONCEPTS] + [0.05]


class SemanticAcceptance(recall_test.RecallAcceptance):
    # 只运行本文件的用例；召回用例仍由 recall_test.py 运行。
    for _name in [name for name in dir(recall_test.RecallAcceptance) if name.startswith("test_")]:
        locals()[_name] = None
    del _name

    def setUp(self):
        recall_test.RecallAcceptance.setUp(self)
        self.embeddings = []
        self.embedding_failure = False
        outer = self
        chat_handler = self.server.RequestHandlerClass

        class EmbeddingHandler(chat_handler):
            def respond(self):
                if self.path != "/v1/embeddings":
                    return chat_handler.respond(self)
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                with outer.lock:
                    outer.embeddings.append(body)
                if outer.embedding_failure:
                    encoded = json.dumps({"error": {"message": "embedding unavailable"}}).encode()
                    status = 503
                else:
                    assert body["model"] == "concept-embed" and body["dimensions"] == 4
                    data = [{"index": index, "embedding": concept_vector(text)}
                            for index, text in enumerate(body["input"])]
                    encoded = json.dumps({"object": "list", "data": data}).encode()
                    status = 200
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        self.server.RequestHandlerClass = EmbeddingHandler

    def semantic_run(self, script, checkpoints=None, env=None, **options):
        return self.run_eve(script, recall=True, extra=["--semantic-recall"],
                            env_extra={**SEMANTIC_ENV, **(env or {})}, checkpoints=checkpoints, **options)

    def index(self):
        return self.documents().get("eve.semantic", {}).get("index.v1") or {"entries": []}

    def indexed(self, count):
        return lambda: self.wait_until(lambda: len(self.index()["entries"]) >= count, "history was not indexed")

    def test_a_paraphrase_without_shared_terms_brings_back_delivered_history(self):
        evidence = self.seed_interaction("source", HISTORY)
        self.semantic_run([*self.checkpoint("indexed"), *self.send(self.message("semantic", PARAPHRASE)),
                           *self.send(self.message("status", "/semantic", contains=["语义召回已启用", "已建索引"]))],
                          checkpoints={"indexed": self.indexed(2)})
        [data] = self.recall_data(self.requests[-1])
        self.assertEqual(data["retrieval"], "hybrid")
        self.assertEqual({hit["source"]["evidence_id"] for hit in data["hits"]}, {evidence["id"]})
        self.assertEqual({hit["source"]["field"] for hit in data["hits"]}, {"User", "Assistant"})
        self.assertTrue(all(HISTORY in hit["excerpt"] for hit in data["hits"]))
        # 先一次批量建索引（用户与助手两个字段），再为本轮查询请求一次向量。
        self.assertEqual(self.embeddings[0]["input"], [HISTORY, HISTORY])
        self.assertEqual(self.embeddings[1]["input"], [PARAPHRASE])
        # 索引只保存范围摘要、条目键、正文摘要与量化向量。
        raw = json.dumps(self.index(), ensure_ascii=False)
        for secret in [HISTORY, PARAPHRASE, "user-1"]:
            self.assertNotIn(secret, raw)

        # 只有词项召回时，另一句同样没有共同字词的说法找不到这段历史。
        other = "能不能啰嗦少点"
        self.run_eve(self.send(self.message("lexical", other)), recall=True)
        self.assertEqual(self.recall_data(self.requests[-1]), [])

        # 重启后索引从账本恢复：已建索引的历史不再嵌入，只为新内容与本轮查询请求向量。
        embedded = lambda: sum(text == HISTORY for body in self.embeddings for text in body["input"])
        self.assertEqual(embedded(), 2)
        self.semantic_run([*self.send(self.message("again", other)), *self.checkpoint("quiet-again")],
                          checkpoints={"quiet-again": lambda: self.assertFalse(self.run_release.wait(0.6))})
        self.assertEqual(embedded(), 2)
        [data] = self.recall_data(self.requests[-1])
        self.assertEqual(data["retrieval"], "hybrid")
        # 与查询原文相同的历史排在前面；其余命中与查询没有共同字词，只能来自语义排名。
        self.assertTrue(any(other not in hit["excerpt"] for hit in data["hits"]))

    def test_embedding_outage_falls_back_to_lexical_with_a_mark_and_writes_no_index(self):
        self.seed_interaction("source", HISTORY)
        self.embedding_failure = True
        self.semantic_run([*self.send(self.message("lexical-term", "回答简短")),
                           *self.send(self.message("status", "/semantic", contains=["失败", "改用词项召回"]))])
        [data] = self.recall_data(self.requests[-1])
        self.assertEqual(data["retrieval"], "lexical_fallback")
        self.assertTrue(data["hits"])
        self.assertEqual(self.index()["entries"], [], "请求失败不写入索引")

    def refused(self, arguments, env=None):
        child = subprocess.Popen([str(memory_harness.BINARY), *arguments, "--state-dir", str(self.work / "state"),
                                  "--agent", str(memory_harness.ROOT / "AGENT.md")],
                                 env={"PATH": os.environ.get("PATH", ""), **(env or {})},
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        stdout, stderr = child.communicate(timeout=20)
        self.assertNotEqual(child.returncode, 0)
        self.assertEqual(stdout, "")
        return stderr

    def test_semantic_recall_requires_recall_and_a_configured_embedding_role(self):
        self.assertIn("--semantic-recall 需要同时开启 --memory-recall",
                      self.refused(["--memory", "--semantic-recall"]))
        # 语义模型角色未启用：通道启动前拒绝，不发出任何请求。
        env = {"QQBOT_APP_SECRET": "test-app-secret", "QQBOT_APP_ID": "1904159860",
               "EVE_OPENAI_API_KEY": "test-model-secret",
               "EVE_OPENAI_BASE_URL": f"http://127.0.0.1:{self.server.server_port}"}
        self.assertIn("语义模型角色", self.refused(["--memory", "--memory-recall", "--semantic-recall"], env))
        self.assertFalse(self.requests)
        self.assertFalse(self.embeddings)


if __name__ == "__main__":
    unittest.main(verbosity=2)
