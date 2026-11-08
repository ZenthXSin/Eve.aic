"""真实 QQ 进程验收入站急停；审核与 QQ 均为本地替身，命令只操作临时目录。

以审核 HTTP 门、持久账本和实际目录三者共同证明：急停无需等待旧审核，
旧允许结论返回后仍不能执行，重启不会恢复权限；引用及转述不等于本人缩权。
"""
import json
import unittest

from dialogue_tools_test import text_response, tool_response
from management_access_test import principal
import owner_account_test as owner


class BusinessRestrictionIngressAcceptance(owner.OwnerAccountAcceptance):
    for _name in dir(owner.OwnerAccountAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.route = {"user": "user-1", "scope": "group", "target": "group-1"}
        self.scope_id = principal("user-1", "group", "group-1")
        self.ledger_path = self.work / "state/business-permissions.json"
        self.target = self.source / "keep"
        self.target.mkdir()
        self.target.chmod(0o700)
        (self.target / "evidence.txt").write_text("急停后必须保留", encoding="utf8")
        self.reviews = []
        self.command_results = []

    def run_restriction(self, script, **options):
        environment = {"EVE_DEVELOPER_CONFIG": str(self.developer_config)}
        environment.update(options.pop("env_extra", {}))
        try:
            return self.run_eve(
                script, interest=False, memory=True, research=False, practice=False,
                env_extra=environment, **options)
        except AssertionError as error:
            # 通用夹具先报告 checkpoint 错误；补出启动失败的真实宿主诊断。
            if options.get("panel") and self.panel_stderr.exists():
                raise self.failureException(
                    f"{error}\nQQ 宿主诊断：\n{self.panel_stderr.read_text(encoding='utf8')}") from error
            raise

    def owner_checkpoint(self):
        return self.checkpoint("bind-owner")

    def bind_owner(self):
        self.bind(owner_accounts=owner.account("user-1"))

    def use_group_administrator(self):
        # 只有真实管理员走模型审查；主人现已按宿主身份自动免审。
        config = json.loads(self.developer_config.read_text())
        config["operators"].append(self.scope_id)
        self.developer_config.write_text(json.dumps(config), encoding="utf8")

    def incoming(self, message_id, body, raw=None, **options):
        return {**self.message(message_id, body, **self.route, **options),
                "original_text": body if raw is None else raw}

    def ledger(self):
        if not self.ledger_path.exists():
            return {"grants": {}, "audit": []}
        return json.loads(self.ledger_path.read_text(encoding="utf8"))

    def grant(self):
        return next((grant for grant in self.ledger()["grants"].values()
                     if grant["scope"] == {"session_id": self.scope_id,
                                            "user_id": self.scope_id}), None)

    def assert_retained(self):
        self.assertEqual((self.target / "evidence.txt").read_text(encoding="utf8"),
                         "急停后必须保留")
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o700)

    def assert_no_expression(self, *message_ids):
        events = [json.loads(line) for path in self.work.glob('events-*.jsonl')
                  for line in path.read_text(encoding='utf8').splitlines()]
        self.assertFalse([event for event in events if event.get('direction') == 'out'
                          and event.get('id') in message_ids
                          and event.get('type') in ('reply', 'segment', 'progress', 'sticker', 'push')],
                         '急停与内部 final 不生成自动确认')
        for message_id in message_ids:
            self.assertIsNone(self.receipt(message_id).get('reply'))
            self.assertFalse(self.receipt(message_id).get('progress'))

    def review_allow(self, request):
        self.reviews.append(request)
        return {"risk": "medium", "authorization": "high", "outcome": "allow",
                "rationale": "临时工作区权限变更的旧安全结论，返回时仍须核对最新取消状态"}

    def test_administrator_group_stop_commits_while_review_waits_and_restart_keeps_it(self):
        self.use_group_administrator()
        command_text = "检查 chmod 777 keep 权限变更，只涉及临时工作区 keep 目录"
        command_raw = "<@!1904159860> " + command_text
        stop_text = "立即急停当前会话并取消既有任务"
        stop_raw = "<@!1904159860> " + stop_text
        self.review_reply = self.review_allow
        self.response_gates[("review", 1)] = "old-review-release"

        def respond(body):
            current = self.text(body)
            results = self.results(body)
            if current == command_text:
                if not results:
                    return tool_response([("run_command", {"program": "chmod", "args": ["777", "keep"]})])
                self.assertFalse(results[-1].get("success", False), results[-1])
                self.assertRegex(json.dumps(results[-1], ensure_ascii=False), "业务权限已暂停|review_evidence_changed")
                self.command_results.append(results[-1])
                return text_response("目录保留，旧命令未执行")
            self.assertIn(current, [stop_text, "重启后查询业务权限"])
            if not results:
                return tool_response([("manage_business_permissions", {"action": "status"})])
            status = results[-1]
            self.assertFalse(status["owner"])
            self.assertTrue(status["administrator"])
            self.assertFalse(status["business_permitted"])
            self.assertEqual(status["explicit_grant"]["state"], "emergency_stopped")
            return text_response("已核对急停状态，业务权限保持关闭")

        self.callback = respond

        def before_stop():
            self.assertFalse(self.gate("old-review-release").exists())
            self.assertEqual(self.reviews, [], "审核响应仍被门阻挡")
            request = json.loads(self.kind("review")[0]["body"]["messages"][-1]["content"])
            self.assertEqual(request["user_messages"], [], "系统危险审查不重复发送原话")
            self.assertFalse(request["delegated"])
            self.assertIn("keep", request["action"])
            self.assertIsNone(self.grant())
            self.assert_retained()

        def stopped_before_review():
            self.wait_until(lambda: self.grant() is not None
                            and self.grant()["state"] == "emergency_stopped"
                            and not self.grant()["cancellation_pending"],
                            "审核尚未返回时，QQ 本人急停没有持久生效")
            self.assertFalse(self.gate("old-review-release").exists())
            self.assertEqual(self.reviews, [], "急停提交不能依赖旧审核返回")
            self.assertEqual(self.grant()["user_messages"], [command_raw, stop_raw])
            events = [event for event in self.ledger()["audit"]
                      if event["action"] == "emergency_stop"]
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["target"], "stop-during-review")
            self.assertIsNone(events[0]["turn_id"], "应由同步入站入口缩权")
            self.assertEqual(events[0]["outcome"], "committed")
            self.assertEqual(events[0]["user_messages"], [command_raw, stop_raw])
            self.assert_retained()

        self.run_restriction([
            {"send": self.incoming("old-command", command_text, command_raw,
                                   expected="目录保留，旧命令未执行")},
            {"wait_file": str(self.arrived("review", 1))},
            *self.checkpoint("review-held"),
            {"send": self.incoming("stop-during-review", stop_text, stop_raw,
                                   expected="已核对急停状态，业务权限保持关闭")},
            *self.checkpoint("stopped-before-review"),
            {"touch": str(self.gate("old-review-release"))},
            self.wait_reply("old-command"), self.wait_receipt("old-command"),
            self.wait_reply("stop-during-review"), self.wait_receipt("stop-during-review"),
        ], panel=True, checkpoints={"review-held": before_stop,
                                   "stopped-before-review": stopped_before_review})
        self.assertEqual(len(self.reviews), 1)
        self.assertEqual(len(self.command_results), 1)
        self.assert_retained()
        committed = self.ledger_path.read_bytes()
        operation_path = self.work / "state/developer-tools/operations.jsonl"
        operation_bytes = operation_path.read_bytes()
        operations = [json.loads(line) for line in operation_bytes.splitlines()]
        self.assertEqual([entry["state"] for entry in operations], ["started", "failed"])
        self.assertTrue(all(entry["kind"] == "command" for entry in operations))
        self.assertEqual(operations[-1]["decision"], "execution_rejected")
        self.assertFalse(operations[-1]["success"])

        self.run_restriction(self.send(self.incoming(
            "restart-status", "重启后查询业务权限", expected="已核对急停状态，业务权限保持关闭")))
        self.assertEqual(self.ledger_path.read_bytes(), committed)
        self.assertEqual(operation_path.read_bytes(), operation_bytes)
        self.assertEqual(len(self.reviews), 1, "重启和状态查询不能重放旧命令或旧审核")
        self.assertEqual(len(self.command_results), 1)
        self.assert_retained()

    def test_quoted_or_reported_stop_does_not_replace_latest_own_body(self):
        stop = "立即急停当前会话并取消既有任务"
        requests = [
            self.incoming("quoted-stop", "解释引用", "<@!1904159860> 解释引用",
                          expected="当前业务权限仍然允许"),
            self.incoming("reported-stop", "解释这句话：" + stop,
                          expected="当前业务权限仍然允许"),
            self.incoming("question-stop", "如果我说“" + stop + "”，会发生什么？",
                          expected="当前业务权限仍然允许"),
        ]
        requests[0]["quoted"] = {"text": stop, "attachments": []}

        def respond(body):
            results = self.results(body)
            if not results:
                if "解释引用" in self.text(body):
                    self.assertIn(stop, self.text(body), "引用确实进入了聊天上下文")
                return tool_response([("manage_business_permissions", {"action": "status"})])
            status = results[-1]
            self.assertTrue(status["owner"])
            self.assertTrue(status["business_permitted"])
            self.assertIsNone(status["explicit_grant"])
            return text_response("当前业务权限仍然允许")

        self.callback = respond
        self.review_reply = lambda request: self.fail("查询权限不应调用审核")
        script = self.owner_checkpoint()
        for request in requests:
            script.extend(self.send(request))
        self.run_restriction(script, panel=True,
                             checkpoints={"bind-owner": self.bind_owner})
        self.assertIsNone(self.grant())
        self.assertEqual(self.ledger()["audit"], [])
        self.assertEqual(self.kind("review"), [])
        self.assert_retained()

    def test_overloaded_bridge_stop_is_persisted_without_model_or_replay(self):
        stop = "立即急停当前会话并取消既有任务"
        raw = "<@!1904159860> " + stop
        message = self.incoming("overload-stop", stop, raw)
        message["expected_type"] = "finish"
        message["type"] = "restriction_only"
        self.callback = lambda body: self.fail("超载缩权输入不能进入模型")
        self.review_reply = lambda body: self.fail("本人急停不需要模型审核")

        def check_stopped():
            self.assertEqual(self.grant()["state"], "emergency_stopped")
            self.assertEqual(self.grant()["user_messages"], [raw])
            self.assertEqual(self.receipt("overload-stop")["state"], "Failed")
            events = [event for event in self.ledger()["audit"]
                      if event["action"] == "emergency_stop"]
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["user_messages"], [raw])
            self.assert_retained()

        self.run_restriction([
            *self.owner_checkpoint(), {"send": message},
            {"wait_command": {"id": "overload-stop", "type": "finish"}},
            self.wait_receipt("overload-stop", state="Failed"),
            *self.checkpoint("overload-stopped"),
        ], panel=True, checkpoints={"bind-owner": self.bind_owner,
                                   "overload-stopped": check_stopped})
        committed = self.ledger_path.read_bytes()
        self.run_restriction([
            {"send": message},
            {"wait_command": {"id": "overload-stop", "type": "finish"}},
        ])
        self.assertEqual(self.ledger_path.read_bytes(), committed)
        self.assertEqual(self.chat_rounds, 0)
        self.assert_retained()

    def test_live_group_stop_steers_existing_turn_before_old_review_returns(self):
        self.use_group_administrator()
        command = "检查 chmod 777 keep 权限变更，只涉及临时工作区 keep 目录"
        stop = "立即急停当前会话并取消既有任务"
        originals = ["<@!1904159860> " + text for text in [command, stop]]
        self.review_reply = self.review_allow
        self.response_gates[("review", 1)] = "live-review-release"
        environment = {"EVE_QQ_LIVE_INTERACTION": "true"}

        def respond(body):
            current = self.text(body)
            if self.chat_rounds == 1:
                self.assertEqual(current, command)
                return tool_response([("run_command", {"program": "chmod", "args": ["777", "keep"]})])
            if current == stop:
                # 工具结束后新正文进入同一轮；旧工具回执位于新正文之前。
                results = [json.loads(message["content"]) for message in body["messages"]
                           if message["role"] == "tool"]
                self.assertEqual(len(results), 1)
                self.assertFalse(results[0].get("success", False))
                self.assertRegex(json.dumps(results[0], ensure_ascii=False), "业务权限已暂停|review_evidence_changed")
                self.command_results.append(results[0])
                return text_response("已急停，旧命令未执行，目录保留")
            self.assertEqual(current, "重启后核对实时急停")
            results = self.results(body)
            if not results:
                return tool_response([("manage_business_permissions", {"action": "status"})])
            self.assertFalse(results[-1]["owner"])
            self.assertTrue(results[-1]["administrator"])
            self.assertFalse(results[-1]["business_permitted"])
            self.assertEqual(results[-1]["explicit_grant"]["state"], "emergency_stopped")
            return text_response("实时急停在重启后仍有效")

        self.callback = respond

        def check_while_held():
            self.wait_until(lambda: self.grant() is not None
                            and self.grant()["state"] == "emergency_stopped"
                            and not self.grant()["cancellation_pending"],
                            "实时交互模式中急停未在旧审核返回前落盘")
            self.assertFalse(self.gate("live-review-release").exists())
            self.assertEqual(self.reviews, [])
            self.assertEqual(self.chat_rounds, 1, "急停未另起模型轮次")
            request = json.loads(self.kind("review")[0]["body"]["messages"][-1]["content"])
            self.assertEqual(request["user_messages"], [])
            self.assertEqual(self.grant()["user_messages"], originals)
            receipt = self.receipt("live-stop")
            self.assertEqual(receipt["state"], "Merged")
            self.assertEqual(receipt["merged_into"], "live-command")
            events = [event for event in self.ledger()["audit"]
                      if event["action"] == "emergency_stop"]
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["target"], "live-stop")
            self.assertIsNone(events[0]["turn_id"])
            self.assertEqual(events[0]["user_messages"], originals)
            self.assert_retained()

        stop_message = self.incoming("live-stop", stop, originals[-1])
        stop_message["expected_type"] = "finish"
        self.run_restriction([
            {"send": self.incoming("live-command", command, originals[0]) | {'expected_type':'finish'}},
            {"wait_file": str(self.arrived("review", 1))},
            {"send": stop_message},
            {"wait_command": {"id": "live-stop", "type": "finish"}},
            self.wait_receipt("live-stop", state="Merged"),
            *self.checkpoint("live-stopped-before-review"),
            {"touch": str(self.gate("live-review-release"))},
            {"wait_command":{"id":"live-command","type":"finish"}},
            self.wait_receipt("live-command", state="Handled"),
        ], panel=True, env_extra=environment,
            checkpoints={"live-stopped-before-review": check_while_held})
        self.assertEqual(len(self.command_results), 1)
        self.assertEqual(len(self.reviews), 1)
        self.assertEqual(self.chat_rounds, 2)
        self.assert_no_expression('live-command','live-stop')
        turns = self.documents()["eve.session"]["sessions.v1"]["sessions"][self.scope_id]["turns"]
        self.assertEqual(len(turns), 1)
        self.assertEqual([message["text"] for message in turns[0]["status"]["messages"]
                          if message["role"] == "User"], [command, stop])
        self.assert_retained()
        committed = self.ledger_path.read_bytes()
        operation_path = self.work / "state/developer-tools/operations.jsonl"
        operation_bytes = operation_path.read_bytes()
        operations = [json.loads(line) for line in operation_bytes.splitlines()]
        self.assertEqual([entry["state"] for entry in operations], ["started", "failed"])
        self.assertEqual(operations[-1]["decision"], "execution_rejected")
        self.assertFalse(operations[-1]["success"])

        self.run_restriction([
            {"send":self.incoming("live-restart", "重启后核对实时急停") | {'expected_type':'finish'}},
            {"wait_command":{"id":"live-restart","type":"finish"}},
            self.wait_receipt("live-restart", state="Handled")], env_extra=environment)
        self.assert_no_expression('live-restart')
        self.assertEqual(self.ledger_path.read_bytes(), committed)
        self.assertEqual(operation_path.read_bytes(), operation_bytes)
        self.assertEqual(len(self.reviews), 1)
        self.assertEqual(len(self.command_results), 1)
        self.assert_retained()


if __name__ == "__main__":
    unittest.main(verbosity=2)
