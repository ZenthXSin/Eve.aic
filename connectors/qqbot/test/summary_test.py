"""语义摘要实际 QQ 进程验收；仅用合成用户消息、本地 HTTP 和桥接替身。"""
import json
import threading
import traceback
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import conversation_digest_test as digest

FIRST = "我喜欢 Mindustry 这个游戏的模组，但还不知道怎么创作"
SECOND = "可以先从最简单的模组开始"
KIND = "eve.semantic-episodes-v1"


class SummaryAcceptance(digest.ConversationDigestAcceptance):
    for _name in dir(digest.ConversationDigestAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.server.shutdown(); self.server.server_close(); self.thread.join()
        self.mode = "valid"
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
                    outer.errors.append(traceback.format_exc()); self.send_error(500)

            def respond(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                latest = body["messages"][-1]["content"]
                try:
                    batch = json.loads(latest)
                except (json.JSONDecodeError, TypeError):
                    batch = {}
                summary = isinstance(batch, dict) and "consumed_ids" in batch
                kind = "summary" if summary else "chat"
                with outer.lock:
                    outer.requests.append({"kind": kind, "body": body, "state": outer.documents()})
                    number = sum(r["kind"] == kind for r in outer.requests)
                outer.arrived(kind, number).touch()
                if summary:
                    job = outer.jobs()[-1]
                    assert job["status"] == "Running" and job["batch"] == batch
                    assert not body.get("tools")
                    assert all("assistant_text" not in source for source in batch["sources"])
                    source = next(s for s in batch["sources"] if s["evidence_id"] in batch["consumed_ids"])
                    points = [*batch["previous"], {"text": "用户提到：" + source["user_text"],
                        "quotes": [{"evidence_id": source["evidence_id"], "text": source["user_text"]}]}][-6:]
                    if outer.mode == "invalid_quote":
                        points[-1]["quotes"][0]["text"] = "没有出现过的伪造来源"
                    latest = json.dumps({"points": points}, ensure_ascii=False)
                    if outer.mode == "duplicate":
                        latest = '{"points":[],"points":[]}'
                    if outer.mode == "hold":
                        outer.run_release.wait(20)
                        return
                if summary and outer.mode == "provider":
                    self.send_error(402)
                    return
                message = {"role": "assistant", "content": latest}
                finish = "stop"
                if summary and outer.mode == "tool":
                    message = {"role": "assistant", "content": None, "tool_calls": [{"id": "forged-tool",
                        "type": "function", "function": {"name": "read_file", "arguments": "{}"}}]}
                    finish = "tool_calls"
                data = json.dumps({"choices": [{"index": 0, "finish_reason": finish, "message": message}]}).encode()
                self.send_response(200); self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True); self.thread.start()

    def run_summary(self, script, **options):
        env = {"EVE_MEMORY_SUMMARY_ENABLED": "true", "EVE_MEMORY_SUMMARY_MIN_NEW_EVIDENCE": "1",
               "EVE_MEMORY_SUMMARY_COOLDOWN_MS": "1000", **options.pop("env_extra", {})}
        return self.run_eve(script, env_extra=env, **options)

    def jobs(self):
        return self.documents().get("eve.memory-summary", {}).get("summaries.v1", {}).get("jobs", [])

    def summary_requests(self):
        return [r for r in self.requests if r["kind"] == "summary"]

    def wait_jobs(self, count, status="Completed"):
        self.wait_until(lambda: len(self.jobs()) == count and all(j["status"] == status for j in self.jobs()), "summary jobs did not settle")

    def views(self, text):
        request = next(r for r in self.requests if r["kind"] == "chat" and r["body"]["messages"][-1]["content"] == text)
        return [json.loads(m["content"][m["content"].index("{"):]) for m in request["body"]["messages"] if KIND in (m.get("content") or "")]

    def test_semantic_context_merges_sources_isolates_users_and_hot_pause_survives_restart(self):
        def disable():
            self.wait_jobs(3)
            self.save_page("runtime.memory_summary", {"enabled": False})
        self.run_summary([*self.send(self.message("first", FIRST)), *self.checkpoint("first"),
            *self.send(self.message("other", "另一个用户的私密表达", user="other-user")),
            *self.send(self.message("second", SECOND)), *self.checkpoint("disable"),
            *self.send(self.message("disabled", "关闭时普通聊天"))], panel=True,
            checkpoints={"first": lambda: self.wait_jobs(1), "disable": disable})
        self.assertEqual(self.views("另一个用户的私密表达"), [])
        [view] = self.views(SECOND)
        self.assertTrue(view["unverified"]); self.assertIn(FIRST, json.dumps(view, ensure_ascii=False))
        self.assertNotIn("另一个用户", json.dumps(view, ensure_ascii=False))
        self.assertEqual(self.views("关闭时普通聊天"), [])
        saved = self.evidence()
        requests = len(self.summary_requests())
        self.run_summary([*self.send(self.message("restart", "重启后配置仍然关闭"))], panel=True)
        self.assertEqual(len(self.summary_requests()), requests)
        self.assertEqual(self.views("重启后配置仍然关闭"), [])
        self.assertTrue(all(e in self.evidence() for e in saved))
        def enable():
            self.save_page("runtime.memory_summary", {"enabled": True})
            self.wait_jobs(4)
        self.run_summary([*self.checkpoint("enable"), *self.send(self.message("restored", "恢复后继续当前话题"))],
            panel=True, checkpoints={"enable": enable})
        self.assertTrue(self.views("恢复后继续当前话题"))

    def test_invalid_quotes_duplicate_json_provider_errors_and_tool_output_are_not_replayed(self):
        for mode in ("invalid_quote", "duplicate", "provider", "tool"):
            with self.subTest(mode=mode):
                self.mode = mode
                start = len(self.jobs()); requests = len(self.summary_requests())
                def settled():
                    self.wait_until(lambda: len(self.jobs()) == start + 1 and isinstance(self.jobs()[-1]["status"], dict), "failed summary was not saved")
                    self.assertEqual(self.jobs()[-1]["points"], [])
                    self.assertFalse(self.run_release.wait(1.1))
                    self.assertEqual(len(self.summary_requests()), requests + 1)
                self.run_summary([*self.send(self.message(mode, FIRST)), *self.checkpoint(mode)], checkpoints={mode: settled})
                self.run_summary([])
                self.assertEqual(len(self.summary_requests()), requests + 1)

    def test_sigterm_cancels_inflight_summary_and_restart_does_not_repeat_the_request(self):
        self.mode = "hold"
        self.run_summary([*self.send(self.message("pending", FIRST)), {"wait_file": str(self.work / "never") }], cancel_at=self.arrived("summary"))
        self.assertEqual(self.jobs()[0]["status"], {"Failed": "Cancelled"})
        self.mode = "valid"; self.run_summary([])
        self.assertEqual(len(self.summary_requests()), 1)

    def test_shared_budget_is_charged_before_summary_request_and_survives_restart(self):
        flags = ["--cognition", "--interest-learning", "--autonomy-budget", "1"]
        def pause_interest():
            status, page = self.panel("/api/plugin-pages/read", {"plugin_id": "eve.interest.settings", "page_id": "learning"})
            self.assertEqual(status, 200)
            status, saved = self.panel("/api/plugin-pages/save", {"plugin_id": "eve.interest.settings", "page_id": "learning",
                "instance": page["instance"], "expected_revision": page["revision"], "values": {"enabled": False}})
            self.assertEqual(status, 200, saved)
        def exhausted():
            self.assertFalse(self.run_release.wait(1.1))
            self.assertEqual(len(self.summary_requests()), 1)
            self.assertEqual(len(self.jobs()), 1)
            budget = self.documents()["eve.budget"]["usage.v1"]
            self.assertIn('"summary": 1', json.dumps(budget))
        self.run_summary([*self.checkpoint("pause"), *self.send(self.message("one", FIRST)), *self.checkpoint("first"),
            *self.send(self.message("two", SECOND)), *self.checkpoint("budget")],
            panel=True, command_extra=flags, checkpoints={"pause": pause_interest, "first": lambda: self.wait_jobs(1), "budget": exhausted})
        self.run_summary([*self.checkpoint("restart")], command_extra=flags, checkpoints={"restart": exhausted})

    def test_provider_timeout_is_visible_in_model_health_before_component_deadline(self):
        self.mode = "hold"
        def timed_out():
            self.wait_jobs(1, {"Failed": "Timeout"})
            status, ready = self.panel("/api/readiness")
            self.assertEqual(status, 200)
            role = next(r for r in ready["roles"] if r["role"] == "summary")
            self.assertEqual((role["requests"], role["failures"], role["state"]), (1, 1, "failing"))
            self.assertEqual(role["last_failure"]["kind"], "timed_out")
        self.run_summary([*self.send(self.message("timeout", FIRST)), *self.checkpoint("timeout")],
            panel=True, env_extra={"EVE_OPENAI_TIMEOUT_SECONDS": "1"}, checkpoints={"timeout": timed_out})

    def test_provider_failure_cooldown_protects_other_scopes_and_survives_restart(self):
        self.mode = "provider"
        def failed():
            self.wait_jobs(1, {"Failed": "Provider"})
        def paused():
            self.assertFalse(self.run_release.wait(0.7))
            self.assertEqual(len(self.jobs()), 1)
            self.assertEqual(len(self.summary_requests()), 1)
        extra = {"EVE_MEMORY_SUMMARY_COOLDOWN_MS": "10000"}
        self.run_summary([*self.send(self.message("first", FIRST)), *self.checkpoint("failed"),
            *self.send(self.message("other", SECOND, user="other-user")), *self.checkpoint("paused")],
            env_extra=extra, checkpoints={"failed": failed, "paused": paused})
        self.run_summary([*self.checkpoint("restart")], env_extra=extra, checkpoints={"restart": paused})


if __name__ == "__main__":
    unittest.main(verbosity=2)
