"""超过旧回执容量后的真实进程恢复、去重与满容量拒绝验收。"""
import json
import unittest
import sqlite3

import segment_test as segment

LIMIT = 16 * 1024 * 1024


class ReceiptCapacityAcceptance(segment.SegmentAcceptance):
    for _name in dir(segment.SegmentAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def run_eve(self, script, **options):
        # 16 MiB 原回执以字节数组存储，debug 宿主须解析并持久化数倍大小的 JSON。
        # CI 曾在所有行为断言之前超过普通分条夹具30秒期限；此容量验收独立保留90秒。
        return super().run_eve(script, timeout=90, **options)

    def seed(self, count, size):
        entries = [{"app_id": "1904159860", "message": {
            "id": f"old-{i}", "scope": "c2c", "target_id": "user-1", "user_id": "user-1",
            "text": "旧输入" + "x" * 600}, "state": "Sent", "reply": "x" * size}
            for i in range(count)]
        # 未决历史不能被清理、补发或变成新模型任务。
        entries[0].update(state="Processing", reply=None)
        ledger = {"version": 1, "entries": entries}
        raw = json.dumps(ledger, ensure_ascii=False, separators=(",", ":")).encode()
        self.state_path.parent.mkdir(parents=True, exist_ok=True)
        self.state_path.write_text(json.dumps({"version": 1, "entries": {
            "eve.channel.qqbot": {"receipts.v1": list(raw)}}}, separators=(",", ":")))
        return entries, len(raw)

    def test_large_legacy_ledger_keeps_history_and_accepts_new_messages_after_restart(self):
        entries, size = self.seed(4100, 160)
        self.assertGreater(size, 1024 * 1024)
        duplicate = self.message("old-1", segments=None, expected_type="finish")
        summary = self.run_eve([
            {"send": duplicate}, {"wait_command": {"id": "old-1", "type": "finish"}},
            *self.complete_original_window(),
            {"send": self.message("new", "扩容后正常回复", segments=None)},
            self.wait_receipt("new", "Sent")], segmented=False)
        self.assertEqual(summary["capacity_rejected"], 0)
        self.assertEqual(len(self.requests), 1)
        self.assertTrue(self.ledger()["entries"][:len(entries)] == entries)

        self.run_eve([
            {"send": self.message("new", segments=None, expected_type="finish")},
            {"wait_command": {"id": "new", "type": "finish"}},
            {"send": self.message("restart", "重启后仍能回复", segments=None)},
            self.wait_receipt("restart", "Sent")], segmented=False)
        self.assertEqual(len(self.requests), 2)
        self.assertTrue(self.ledger()["entries"][:len(entries)] == entries)

    def complete_original_window(self):
        # 旧账本没有 original_text；新收到的原话只补充窗口，不改写历史缺口。
        return [step for i in range(6) for step in (
            {"send": self.message(f"original-{i}", "/admin", segments=None,
                                  expected="当前平台身份：管理员。")},
            self.wait_receipt(f"original-{i}", "Sent"))]

    def test_over_10m_archives_to_database_keeps_1m_and_restores_scoped_reads(self):
        entries, size = self.seed(340, 32768)
        self.assertGreater(size, 10 * 1024 * 1024)
        # 一条较老的明确失败记录，归档后仍可通过 /last-reply 找回。
        document = json.loads(self.state_path.read_text())
        raw = document["entries"]["eve.channel.qqbot"]["receipts.v1"]
        ledger = json.loads(bytes(raw))
        ledger["entries"][2]["state"] = "Failed"
        entries[2]["state"] = "Failed"
        document["entries"]["eve.channel.qqbot"]["receipts.v1"] = list(json.dumps(ledger,separators=(",",":"),ensure_ascii=False).encode())
        self.state_path.write_text(json.dumps(document,separators=(",",":")))
        self.run_eve([
            {"send": self.message("old-1", segments=None, expected_type="finish")},
            {"wait_command": {"id":"old-1","type":"finish"}},
            *self.complete_original_window(),
            {"send": self.message("after", "归档后正常执行", segments=None)}, self.wait_receipt("after","Sent")], segmented=False)
        active = self.ledger()
        self.assertEqual(active["version"],6)
        self.assertLess(len(json.dumps(active,ensure_ascii=False).encode()), 1024*1024)
        self.assertIn("old-0",[e["message"]["id"] for e in active["entries"]])
        self.assertEqual(len(self.requests),1)
        database = self.work/"state/qq-receipts.sqlite3"
        with sqlite3.connect(database) as db:
            rows = [json.loads(p) for (p,) in db.execute("SELECT payload FROM receipts ORDER BY seq")]
        combined = {e["message"]["id"]:e for e in rows+active["entries"]}
        self.assertTrue(all(combined[e["message"]["id"]] == e for e in entries))
        self.run_eve([
            {"send": self.message("old-1", segments=None, expected_type="finish")},
            {"wait_command": {"id":"old-1","type":"finish"}},
            {"send": self.message("read", "/receipt old-1", segments=None, expected_contains=["old-1","旧输入"])},self.wait_receipt("read","Sent"),
            {"send": self.message("foreign", "/receipt old-1", segments=None, user_id="user-2",target_id="user-2",expected="本会话没有这条回执。")},self.wait_receipt("foreign","Sent"),
            {"send": self.message("last", "/last-reply", segments=None, expected="x"*32768)},self.wait_receipt("last","Sent")], segmented=False)
        self.assertEqual(len(self.requests),1,"旧回执读取与重复消息均不调用模型")
        # 缺失冷库时拒绝启动，不能创建空库并丢掉冷区去重证据。
        moved = database.with_suffix(".saved")
        database.rename(moved)
        stderr = self.run_eve([],segmented=False,success=False)
        self.assertIn("归档数据库缺失",stderr)
        self.assertFalse(database.exists())
        moved.rename(database)

    def test_all_pending_full_storage_sends_explicit_rejection_and_preserves_evidence(self):
        entries, _ = self.seed(507,32768)
        for e in entries:
            e.update(state="Processing",reply=None)
            e["message"]["text"]="x"*32768
        raw=json.dumps({"version":1,"entries":entries},separators=(",",":"),ensure_ascii=False).encode()
        self.assertLess(len(raw),LIMIT)
        self.state_path.write_text(json.dumps({"version":1,"entries":{"eve.channel.qqbot":{"receipts.v1":list(raw)}}},separators=(",",":")))
        summary = self.run_eve([
            {"send":self.message("full","执行任务",segments=None,expected_type="capacity")},
            {"wait_command":{"id":"full","type":"capacity"}},
            {"send":self.message("command","/help",segments=None,expected_type="capacity")},
            {"wait_command":{"id":"command","type":"capacity"}}],segmented=False)
        self.assertEqual(summary["capacity_rejected"],2)
        self.assertEqual(self.requests,[])
        self.assertTrue(self.ledger()["entries"] == entries)


import dialogue_tools_test as dialogue


class ArchivedToolAcceptance(dialogue.DialogueToolsAcceptance):
    ledger = segment.SegmentAcceptance.ledger

    for _name in dir(dialogue.DialogueToolsAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def test_legacy_conversation_keeps_tools_with_original_gap_across_restart(self):
        entries, _ = ReceiptCapacityAcceptance.seed(self, 340, 32768)
        originals = [f"迁移后的普通聊天 {i}，继续聊今天的计划。" for i in range(5)]
        lookup = "现在帮我核对 old-1 的原始回执。"
        tool_results = []
        code_results = []
        reviews = []
        provider = self.provider_response

        def deny_review(body, kind, fallback):
            if self.definitions(body) == {"submit_review"}:
                reviews.append(body)
                return dialogue.tool_response([("submit_review", {
                    "risk": "high", "authorization": "low", "outcome": "deny",
                    "rationale": "可信原文缺口不应阻挡普通工具或归档查询"})])
            return provider(body, kind, fallback)

        self.provider_response = deny_review

        def assert_history_preserved():
            with sqlite3.connect(self.work / "state/qq-receipts.sqlite3") as db:
                archived = [json.loads(row[0]) for row in db.execute("SELECT payload FROM receipts")]
            saved = {entry["message"]["id"]: entry
                     for entry in archived + self.ledger()["entries"]}
            self.assertTrue(all(saved.get(entry["message"]["id"]) == entry for entry in entries))

        def respond(body):
            current = next(m["content"] for m in reversed(body["messages"])
                           if m["role"] == "user")
            results = self.results(body)
            if current in originals:
                names = self.definitions(body)
                self.assertLessEqual({"read_raw_message", "read_source", "inspect_capabilities",
                                      "run_tool_code", "manage_business_permissions"}, names)
                self.assertFalse({"run_command", "write_file"} & names,
                                 "原文缺口不扩大当前宿主未装配的开发能力")
                if current in (originals[0], originals[2]):
                    if not results:
                        return dialogue.tool_response([("run_tool_code", {
                            "tools": ["read_raw_message"],
                            "arguments": {"message_id": "old-1"},
                            "code": "text(await tools.read_raw_message(args));"})])
                    self.assertEqual(len(results), 1)
                    self.assertTrue(results[0]["success"], results[0])
                    [raw] = results[0]["result"]["output"]
                    self.assertTrue(raw["found"] and raw["archived"], raw)
                    self.assertIn("旧输入", raw["data"])
                    self.assertEqual([(item["target"], item["status"])
                                      for item in results[0]["evidence"]],
                                     [("read_raw_message", "succeeded")])
                    code_results.append(results[0])
                return dialogue.text_response("好，接着聊今天的计划。")
            self.assertEqual(current, lookup)
            self.assertIn("read_raw_message", self.definitions(body))
            if not results:
                return dialogue.tool_response([("read_raw_message", {"message_id": "old-1"})])
            self.assertTrue(results[-1]["found"], results[-1])
            self.assertIn("旧输入", results[-1]["data"])
            tool_results.append(results[-1])
            return dialogue.text_response("原始回执仍在，已经核对。")

        self.callback = respond
        options = dict(interest=False, memory=True, research=False, practice=False,
                       env_extra={"EVE_CONTEXT_ADAPTIVE_LIMIT_TOKENS": "200000"})
        script = [step for i in range(2) for step in self.send(self.message(
            f"natural-{i}", originals[i], expected="好，接着聊今天的计划。"))]
        summary = self.run_eve(script, **options)
        self.assertEqual(summary["completed"], 2)
        self.assertEqual(self.chat_rounds, 3)
        self.assertEqual(len(code_results), 1)
        self.assertEqual(reviews, [])
        assert_history_preserved()

        # 重启保留迁移缺口；首条普通消息仍可编排工具，不必补满六条原话。
        script = [step for i in range(2, 5) for step in self.send(self.message(
            f"natural-{i}", originals[i], expected="好，接着聊今天的计划。"))]
        script += self.send(self.message("natural-sixth", lookup, expected="原始回执仍在，已经核对。"))
        summary = self.run_eve(script, **options)
        self.assertEqual(summary["completed"], 4)
        self.assertEqual(len(tool_results), 1)
        self.assertEqual(len(code_results), 2)
        self.assertEqual(self.chat_rounds, 9)
        self.assertEqual(reviews, [])
        assert_history_preserved()
        with sqlite3.connect(self.work / "state/qq-receipts.sqlite3") as db:
            saved = [row[0] for row in db.execute("SELECT text FROM qq_originals ORDER BY seq")]
        self.assertEqual(saved, originals + [lookup], "原文库不得混入加工后的旧回执")

        self.run_eve(self.send(self.message(
            "natural-after-restart", lookup, expected="原始回执仍在，已经核对。")), **options)
        self.assertEqual(len(tool_results), 2)
        self.assertEqual(self.chat_rounds, 11)
        self.assertEqual(reviews, [])
        assert_history_preserved()

        sessions = self.documents()["eve.session"]["sessions.v1"]
        requests = len(self.requests)
        self.run_eve([
            {"send": self.message("natural-0", originals[0]) | {"expected_type": "finish"}},
            {"wait_command": {"id": "natural-0", "type": "finish"}},
        ], **options)
        self.assertEqual(len(self.requests), requests, "重送已完成输入不能重放模型或工具")
        self.assertEqual(self.documents()["eve.session"]["sessions.v1"], sessions)
        self.assertEqual(len(code_results), 2)
        self.assertEqual(reviews, [])
        assert_history_preserved()

    def test_model_pulls_archived_receipt_pages_without_crossing_session_or_replaying(self):
        ReceiptCapacityAcceptance.seed(self,340,32768)
        def respond(body):
            results = self.results(body)
            text = body["messages"][max(i for i,m in enumerate(body["messages"]) if m["role"]=="user")]["content"]
            if text == "原话不足时查询":
                self.assertIn("read_raw_message", self.definitions(body))
                self.assertNotIn("run_command", self.definitions(body))
            if not results:
                return dialogue.tool_response([("read_raw_message",{"message_id":"old-1"}),
                    ("read_raw_message",{"message_id":"old-1","offset":8192})])
            if text in ("本会话查归档", "原话不足时查询"):
                self.assertTrue(all(r["found"] and r["archived"] for r in results))
                self.assertIn("旧输入",results[0]["data"])
                self.assertEqual(results[0]["next_offset"],8192)
                self.assertEqual(results[1]["offset"],8192)
                self.assertTrue(all(len(r["data"].encode())<=8192 for r in results))
            else:
                self.assertTrue(all(not r["found"] for r in results))
                self.assertNotIn("旧输入",json.dumps(results,ensure_ascii=False))
            return dialogue.text_response("已按会话核验旧资料，未执行原请求")
        self.callback=respond
        self.run_eve([
            *self.send(self.message("missing-originals", "原话不足时查询", expected="已按会话核验旧资料，未执行原请求")),
            *self.send(self.message("lookup","本会话查归档",expected="已按会话核验旧资料，未执行原请求")),
            *self.send(self.message("foreign-lookup","另一会话查归档",user="user-2",expected="已按会话核验旧资料，未执行原请求"))],
            interest=False,memory=False,research=False,practice=False)
        self.assertEqual(self.chat_rounds,6)


if __name__ == "__main__":
    unittest.main()
