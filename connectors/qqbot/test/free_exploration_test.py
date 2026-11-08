"""真实 Eve 子进程的无目标探索、经历保存与恢复；模型和 QQ 均为本地替身。

随机材料只能由实际 list/read 回执提供。此夹具验证宿主接线和持久语义，
不能证明真实模型自主选择了探索。时间只推进已退出进程的隔离状态；中断
只操作公共夹具创建并持有的 Popen，不读取 PID 文件或改动生产数据。
可设置 EVE_FREE_EXPLORATION_ARTIFACTS 保存本地合成证据。需显式构建并指定
EVE_QQBOT_BINARY；本文件不会构建，也不连接真实模型或 QQ。
"""
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import sys
import threading
import time
import unittest
import uuid

import idle_loop_test as idle
from dialogue_tools_test import text_response, tool_response
from model_adapter_fixture import delegated_messages


SOURCE_PREFIX = "subject-explore:"
WORKER_MARKER = "这是一次自由探索会话"
NORMAL_STATUS = "SessionEnded"
POLICY = "autonomy-policy-v1"


class FreeExplorationAcceptance(idle.IdleLoopAcceptance):
    for _name in dir(idle.IdleLoopAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.mode = "explore"
        self.max_segments = 8
        self.steps_per_segment = 10
        self.exploration_field = None
        self.reflection_outputs = []
        self.worker_bodies = []
        self.worker_calls = []
        self.baseline_grant = None
        self.baseline_reviews = None
        self.hold_worker = threading.Event()
        self.worker_waiting = threading.Event()
        self.worker_returned = threading.Event()
        self.foreground = None
        self.after_reflection = None
        self.allow_idle_failure_notice = False
        self.nonce = uuid.uuid4().hex
        self.first_name = "colors_" + self.nonce[:12] + ".txt"
        self.second_name = "sounds_" + self.nonce[12:24] + ".txt"
        (self.source / self.first_name).write_text(
            "这里是色彩的合成材料，读过也可以改变兴趣。\n"
            + "另一个方向的材料路径：" + self.second_name + "\n", encoding="utf8")
        (self.source / self.second_name).write_text(
            "这里是声音的合成材料；尚未验证任何结论。\n"
            + "实际读取标记：" + self.nonce + "\n", encoding="utf8")
        self.initial_files = self.workspace_files()
        self.configure_developer(commands=False, github=False, self_upgrade=False,
                                 host_filesystem=False)
        self.forbidden_action_tools = {
            "run_command", "github_api", "prepare_self_upgrade", "apply_self_upgrade",
            "restart_self", "self_upgrade_status"}
        self.review_reply = self.review_free
        self.callback = self.respond_free

    def tearDown(self):
        self.hold_worker.set()
        # 仅释放本用例的本地命令；工具已被宿主停止时也不会重启它。
        if hasattr(self, "command_release"):
            self.command_release.touch(exist_ok=True)
        try:
            self.save_evidence()
        finally:
            super().tearDown()

    def configure_developer(self, **changes):
        config = json.loads(self.developer_config.read_text())
        config.update(changes)
        self.developer_config.write_text(json.dumps(config), encoding="utf8")

    def workspace_files(self):
        return {str(path.relative_to(self.source)): path.read_bytes()
                for path in self.source.rglob("*") if path.is_file()}

    @staticmethod
    def read_events(path):
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines() if line]

    def save_evidence(self):
        root = os.environ.get("EVE_FREE_EXPLORATION_ARTIFACTS")
        if not root:
            return
        target = Path(root).resolve() / (self._testMethodName + "-" + self.nonce[:8])
        target.mkdir(parents=True, exist_ok=False)
        evidence = {
            "kind": "deterministic_local_fixture",
            "real_model_autonomous_choice_proven": False,
            "source_prefix": SOURCE_PREFIX, "worker_marker": WORKER_MARKER,
            "normal_status": NORMAL_STATUS, "reflections": self.reflection_outputs,
            "worker_requests": self.worker_bodies, "worker_calls": self.worker_calls,
            "tasks": self.tasks(), "subject": self.subject(),
            "experiences": self.experience_rows(), "errors": self.errors,
            "events": {path.name: self.read_events(path)
                       for path in sorted(self.work.glob("events-*.jsonl"))}}
        (target / "evidence.json").write_text(
            json.dumps(evidence, ensure_ascii=False, separators=(",", ":")), encoding="utf8")

    def explorations(self):
        return [task for task in self.tasks() if task["source"].startswith(SOURCE_PREFIX)]

    def policy(self):
        return next(focus for focus in self.subject()["foci"] if focus["id"] == POLICY)

    def grant(self):
        return json.loads(self.policy()["goal"])

    def history_query(self, query, parameters=()):
        database = self.state_path.parent / "self-interests.sqlite3"
        if not database.exists():
            return []
        connection = sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)
        try:
            return connection.execute(query, parameters).fetchall()
        finally:
            connection.close()

    def experience_rows(self):
        tables = self.history_query("SELECT name FROM sqlite_master WHERE type='table'")
        if ("exploration_experiences",) not in tables:
            return []
        return [json.loads(row[0]) for row in self.history_query(
            "SELECT payload FROM exploration_experiences ORDER BY seq")]

    def experience(self, task):
        return next((item for item in self.experience_rows()
                     if item["session_id"] == task["id"]), None)

    def interest_history_snapshot(self):
        self.assertTrue((self.state_path.parent / "self-interests.sqlite3").is_file())
        # 比较实际记录及选择索引列；不能只靠可省略空列表的上下文视图判断未增长。
        orders = {
            "interests": "scope,key", "interest_history": "scope,key,revision",
            "consumed": "scope,key,evidence_id", "reflections": "scope,revision",
            "exploration_experiences": "seq", "exploration_sources": "scope,reflection_id"}
        return {table: self.history_query(f"SELECT * FROM {table} ORDER BY {order}")
                for table, order in orders.items()}

    def wait_reconciled(self):
        def saved():
            ids = {row[0] for row in self.history_query("SELECT task FROM reconciled_tasks")}
            ended = [task for task in self.tasks() if task.get("finished_at_ms") is not None]
            return all(task["id"] in ids and (
                not task["source"].startswith(SOURCE_PREFIX) or (
                    self.experience(task) is not None
                    and task["execution"].get("exploration_acknowledged_at_ms") is not None))
                for task in ended)
        self.wait_for(saved, "终态与真实经历尚未完整保存到主体历史", 20)

    def wait_rejected_reflection_and_notice(self, reason):
        def failures():
            return [task for task in self.tasks()
                    if task["source"].startswith("subject-idle:") and task["status"] == "Failed"]
        self.wait_for(lambda: bool(failures()), reason, 20)
        [failed] = failures()
        task_id = failed["id"]
        # Failed 先落盘，通道随后仍会发通知。须等真实回执再放行
        # checkpoint，否则 fake bridge 退出会与宿主写命令竞争，导致收尾失败。
        self.wait_for(lambda: any(task["id"] == task_id
            and (task.get("notice") or {}).get("status") == "Delivered"
            for task in self.tasks()), "拒绝回顾的通知尚未得到桥接回执", 12)
        settled = next(task for task in self.tasks() if task["id"] == task_id)
        [attempt] = settled["notice"]["attempts"]
        self.assertEqual(attempt["channel"], "proactive")
        self.assertEqual(attempt["result"], {"sent": {"platform_message_id": "push-" + task_id}})
        self.assertIsNotNone(attempt["finished_at_ms"])
        self.assertEqual(len([event for event in self.read_events(
            self.work / f"events-{self.runs}.jsonl")
            if event.get("type") == "push" and event.get("id") == task_id]), 1)

    def review_free(self, request):
        # 配置及普通探索均无需模型审批；恒定拒绝才能暴露残留的审批依赖。
        return {"risk": "low", "authorization": "high", "outcome": "deny",
                "rationale": "本夹具拒绝给普通自由探索追加行动审批"}

    def assert_grant_identity(self, before, after):
        for key in ("generation", "configured_turn_id", "evidence_digest", "review_messages", "expires_at_ms",
                    "max_goals", "steps_per_goal"):
            self.assertEqual(after.get(key), before.get(key), "不得重授探索资源：" + key)
        self.assertLessEqual(after["issued"], before["max_goals"])

    def run_idle(self, script, **options):
        result = super().run_idle(script, **options)
        allowed_replies = {message["id"] for step in script if "send" in step
                           for message in (step["send"] if isinstance(step["send"], list)
                                           else [step["send"]])}
        for event in self.read_events(self.work / f"events-{self.runs}.jsonl"):
            if event.get("direction") != "out" or event["type"] in ("stop", "restore_anchor"):
                continue
            if self.allow_idle_failure_notice:
                idle_failures = {task["id"] for task in self.tasks()
                                 if task["source"].startswith("subject-idle:")
                                 and task["status"] == "Failed"}
                if event.get("id") in idle_failures and event["type"] == "push":
                    continue
            self.assertIn(event.get("id"), allowed_replies,
                          "探索产生了后台 QQ 发送：" + json.dumps(event, ensure_ascii=False))
            self.assertNotIn(event["type"], ("push", "progress"))
        for task in self.explorations():
            self.assert_quiet(task)
        return result

    def initialize(self):
        message = self.message("enable-free", (
            "我是主人，授权 Eve 在当前范围空闲时自由回顾、看看和尝试，"
            "可以转向或休息，不需要预先提出目标或交付成果。"
            f"最多{self.max_segments}段运行，每段{self.steps_per_segment}次工具调用，24小时内有效；"
            "只使用临时工作区已开放的工具，保持现有身份与权限，未知操作不重跑。"),
            expected="自由探索已授权")
        self.run_idle(self.send(message))
        self.baseline_grant = self.grant()
        self.baseline_reviews = len(self.kind("review"))
        self.assertEqual(self.baseline_reviews, 0, "自主预算配置不应调用模型审查")
        self.assertEqual(self.baseline_grant["issued"], 0)

    def observation(self, body):
        packets = delegated_messages(body)
        observations = [json.loads(message["text"][len(idle.PREFIX):]) for message in packets
                        if (message.get("text") or "").startswith(idle.PREFIX)]
        self.assertEqual(len(observations), 1)
        return observations[0]

    def call(self, name, arguments):
        call_id = "free-call-" + str(len(self.worker_calls) + 1)
        self.worker_calls.append({"call_id": call_id, "name": name, "arguments": arguments})
        response = tool_response([(name, arguments)])
        response["choices"][0]["message"]["tool_calls"][0]["id"] = call_id
        return response

    @staticmethod
    def is_worker(body):
        return WORKER_MARKER in body["messages"][0]["content"]

    def reflection(self, body):
        self.assertFalse(body.get("tools"))
        self.review_capabilities.append(self.action_capabilities(body))
        observation = self.observation(body)
        self.observations.append(observation)
        self.reflections += 1
        result = {
            "choice": "explore" if self.reflections == 1 else "wait",
            "thought": "想先随便看看，遇到有意思的内容再决定",
            "reason": "现在有空，也有一点好奇", "judgments": [], "revisit_after_minutes": 30}
        self.assertTrue({"action", "title", "goal", "assessment", "completion_contract"}.isdisjoint(result))
        if self.reflections == 1 and self.exploration_field is not None:
            result["exploration"] = self.exploration_field
        if self.reflections > 1 and self.mode == "explore":
            task = self.explorations()[0]
            actual = next((item for item in observation["evidence"] if item["id"] == task["id"]), None)
            self.assertIsNotNone(actual, "后续回顾没有收到本段探索")
            self.assertEqual(actual["kind"], "exploration_experience")
            detail = json.loads(actual["text"])
            self.assertEqual(detail, self.experience(task), "回顾须获得独立保存的原始经历片段")
            self.assert_successful_read_experience(task, detail)
            result["thought"] = "刚读了两种材料，现在想先放下"
            result["reason"] = "经历本身足够，不需要继续制造成果"
            result["interest_updates"] = [{
                "key": "casual-materials", "topic": "色彩与声音的材料", "phase": "resting",
                "strength": 30, "reason": "实际看过后，现在没有继续的兴致", "want_to_try": "",
                "evidence": [task["id"]]}]
        if self.after_reflection:
            self.after_reflection(observation, result)
        self.reflection_outputs.append(result)
        return text_response(json.dumps(result, ensure_ascii=False))

    def respond_free(self, body):
        self.assertNotIn("temperature", body, "不向不支持温度的模型添加温度")
        system = body["messages"][0]["content"]
        if "本任务只规划一个有价值的下一目标" in system:
            self.fail("无目标探索进入了目标与合同规划")
        if "这是Eve无人打扰时的一次自主回顾" in system:
            return self.reflection(body)
        if self.is_worker(body):
            self.worker_bodies.append(body)
            return self.explore(body)
        if "你在为 Eve 独立完成一项委托的任务" in system:
            self.fail("自由探索被执行为普通目标任务")
        if self.foreground:
            response = self.foreground(body)
            if response is not None:
                return response
        if not self.results(body):
            return tool_response([("configure_subject_autonomy", {
                "action": "configure_autonomy", "enabled": True, "idle_reflection": True,
                "max_goals": self.max_segments, "steps_per_goal": self.steps_per_segment})])
        self.assertTrue(self.results(body)[-1].get("enabled"))
        return text_response("自由探索已授权")

    def explore(self, body):
        results = self.results(body)
        if not results:
            self.assertTrue(self.review_capabilities)
            self.assertEqual(sorted(self.definitions(body)), self.review_capabilities[-1]["tools"])
            self.assertFalse(self.definitions(body) & self.forbidden_action_tools)
            self.assertNotIn(self.nonce, json.dumps(body))
        if self.mode == "rest":
            return text_response("现在又想休息了，先到这里。")
        if self.mode == "hold" or self.mode == "resume" and results:
            self.worker_waiting.set()
            self.hold_worker.wait(20)
            self.worker_returned.set()
            return text_response("已经看过，先放下。")
        if self.mode == "unknown":
            self.assertFalse(results)
            return self.call("run_command", self.inflight_command)
        if self.mode == "denied":
            if not results:
                return self.call("read_file", {"path": "../outside.txt"})
            self.assertNotIn("OUTSIDE_PRIVATE", json.dumps(results))
            self.assertTrue(any(key in json.dumps(results[-1]).lower()
                                for key in ("failed", "failure", "error", "invalid", "denied")), results)
            return text_response("这个位置不可读取，先放下。")
        if self.mode == "budget":
            if len(results) == self.steps_per_segment:
                self.assertFalse(body.get("tools"), "预算用尽后仍开放工具")
                return text_response("这段时间到这里。")
            return self.call("echo", {"text": "只是试试看"})
        if not results:
            return self.call("list_files", {"path": "."})
        if len(results) == 1:
            choices = [entry["name"] for entry in results[-1]["entries"]
                       if entry["kind"] == "file" and entry["name"].startswith("colors_")]
            self.assertEqual(len(choices), 1)
            return self.call("read_file", {"path": choices[0]})
        if len(results) == 2:
            match = re.search(r"另一个方向的材料路径：([^\n]+)", results[-1]["content"])
            self.assertIsNotNone(match)
            return self.call("read_file", {"path": match.group(1)})
        self.assertEqual(len(results), 3)
        self.assertIn(self.nonce, results[-1]["content"])
        return text_response("从色彩材料转去读了声音材料，现在想先放下。没有需要交付的成果。")

    def assert_quiet(self, task):
        self.assertIsNone(task.get("notice"), "探索不产生自动任务通知")
        execution = task.get("execution") or {}
        self.assertEqual(execution.get("kind"), "exploration")
        self.assertEqual(execution.get("progress_notices", []), [])
        self.assertIsNone(execution.get("completion_contract"))

    def assert_session_binding(self, task):
        session = task["execution"]["checkpoint"]["model_session"]
        self.assertEqual(session["version"], 1)
        for key in ("session_id", "scope_id", "turn_id"):
            self.assertRegex(session[key], r"^[0-9a-f]{64}$")
        records = self.worker_requests(task)
        self.assertTrue(records, "没有观察到该探索的实际模型请求")
        worker_keys = {record["body"].get("prompt_cache_key") for record in records}
        self.assertEqual(len(worker_keys), 1, "工具往返和恢复必须沿用同一探索缓存键")
        [worker_key] = worker_keys
        self.assertRegex(worker_key, r"^[0-9a-f]{64}$")
        for name in ("session-id", "thread-id"):
            values = {record["session_headers"].get(name) for record in records}
            self.assertEqual(len(values), 1, "同一探索必须沿用模型会话头：" + name)
            self.assertRegex(next(iter(values)), r"^[0-9a-f]{64}$")
        self.assertEqual(records[0]["session_headers"]["session-id"], session["session_id"])
        for record in records:
            self.assertRegex(record["session_headers"].get("x-client-request-id"), r"^[0-9a-f]{64}$")
        other_keys = {request["body"].get("prompt_cache_key") for request in self.requests
                      if request["kind"] == "chat" and not self.is_worker(request["body"])}
        self.assertNotIn(worker_key, other_keys, "探索不能复用前台或空闲回顾会话")

    @staticmethod
    def exploration_context(body):
        contexts = []
        for saved in delegated_messages(body):
            try:
                value = json.loads(saved.get("text") or "")
            except (ValueError, TypeError):
                continue
            if isinstance(value, dict) and value.get("kind") == "eve.exploration.context.v1":
                contexts.append(value)
        if len(contexts) != 1:
            raise AssertionError("探索请求必须携带唯一的已冻结上下文")
        return contexts[0]

    def worker_requests(self, task):
        messages = json.loads(task["spec"]["context"]["messages_json"])
        reflection_id = json.loads(messages[0]["text"])["input"]["reflection_id"]
        return [record for record in self.requests
                if record["kind"] == "chat" and self.is_worker(record["body"])
                and self.exploration_context(record["body"])["input"]["reflection_id"] == reflection_id]

    def wait_end(self, status=NORMAL_STATUS, tools=None, requests=None):
        self.wait_for(lambda: len(self.explorations()) == 1
                      and self.explorations()[0]["status"] == status,
                      "探索没有保存为期望结局 " + status, 20)
        self.wait_reconciled()
        task = self.explorations()[0]
        self.assert_quiet(task)
        self.assert_session_binding(task)
        self.assertIsNotNone(task.get("finished_at_ms"))
        if tools is not None:
            self.assertEqual(task["usage"]["tool_calls"], tools)
        if requests is not None:
            self.assertEqual(task["usage"]["model_requests"], requests)
        self.assert_grant_identity(self.baseline_grant, self.grant())
        self.assertEqual(len(self.kind("review")), self.baseline_reviews,
                         "普通探索仍被额外行动审核阻挡")
        self.assertFalse(any(task["source"].startswith("subject-plan:") for task in self.tasks()))
        self.assertFalse(self.run_release.wait(0.8))
        return task

    def assert_excerpt(self, excerpt):
        raw = excerpt["text"].encode("utf8")
        self.assertLessEqual(len(raw), 2048)
        self.assertGreaterEqual(excerpt["original_bytes"], len(raw))
        self.assertEqual(excerpt["truncated"], excerpt["original_bytes"] > len(raw))
        self.assertRegex(excerpt["sha256"], r"^[0-9a-f]{64}$")
        if not excerpt["truncated"]:
            self.assertEqual(hashlib.sha256(raw).hexdigest(), excerpt["sha256"])

    def assert_successful_read_experience(self, task, experience):
        self.assertEqual(experience["session_id"], task["id"])
        self.assertEqual(experience["end_reason"], "natural")
        self.assertEqual(experience["total_tool_calls"], 3)
        self.assertEqual(experience["tool_offset"], 0)
        self.assertFalse(experience["tools_truncated"])
        checkpoint = task["execution"]["checkpoint"]
        calls = {call["id"]: call for message in checkpoint["messages"]
                 for call in message.get("tool_calls", [])}
        receipts = {item["call_id"]: item["output"] for message in checkpoint["messages"]
                    for item in message.get("tool_results", [])}
        self.assertEqual(len(experience["tools"]), 3)
        for tool in experience["tools"]:
            original = calls[tool["call_id"]]
            self.assertEqual(tool["tool"], original["name"])
            self.assertEqual(tool["outcome"], "succeeded")
            self.assertIsNone(tool["error"])
            self.assert_excerpt(tool["arguments"])
            self.assert_excerpt(tool["output"])
            self.assertFalse(tool["arguments"]["truncated"])
            self.assertFalse(tool["output"]["truncated"])
            self.assertEqual(json.loads(tool["arguments"]["text"]), original["arguments"])
            self.assertEqual(json.loads(tool["output"]["text"]), receipts[tool["call_id"]]["Success"])
        self.assertIn(self.nonce, experience["tools"][-1]["output"]["text"])
        self.assert_excerpt(experience["subjective_notes"])
        self.assertIn("现在想先放下", experience["subjective_notes"]["text"])
        self.assertGreaterEqual(experience["ended_at_ms"], experience["started_at_ms"])

    def assert_no_replay(self):
        before = {"tasks": self.tasks(), "grant": self.grant(), "mind": self.mind(),
                  "experiences": self.experience_rows(), "requests": len(self.requests),
                  "files": self.workspace_files()}
        name = "quiet-" + str(self.runs + 1)
        self.run_idle(self.checkpoint(name), checkpoints={
            name: lambda: self.assertFalse(self.run_release.wait(1.3))})
        self.assertEqual(self.tasks(), before["tasks"])
        self.assertEqual(self.grant(), before["grant"])
        self.assertEqual(self.mind(), before["mind"])
        self.assertEqual(self.experience_rows(), before["experiences"])
        self.assertEqual(len(self.requests), before["requests"])
        self.assertEqual(self.workspace_files(), before["files"])

    def start_and_end(self, name="ended", **expectations):
        self.initialize()
        self.due()
        self.run_idle(self.checkpoint(name), checkpoints={
            name: lambda: self.wait_end(**expectations)})
        return self.explorations()[0]

    def test_explore_turn_rest_real_experience_interest_and_restart(self):
        task = self.start_and_end(tools=3, requests=4)
        self.assertEqual([call["name"] for call in self.worker_calls],
                         ["list_files", "read_file", "read_file"])
        self.assertEqual(self.workspace_files(), self.initial_files)
        self.assert_successful_read_experience(task, self.experience(task))
        self.assertEqual(self.grant()["issued"], 2, "回顾与探索沿用原授权名额")
        self.assertLessEqual(task["spec"]["max_steps"], self.grant()["steps_per_goal"])
        self.assertEqual(len(self.history_query("SELECT * FROM exploration_sources")), 1)
        self.due()
        self.run_idle(self.checkpoint("reconsider"), checkpoints={
            "reconsider": lambda: (self.wait_revision(2), self.wait_reconciled())})
        interest = next(item for item in self.mind()["self_interests"]
                        if item["current"]["key"] == "casual-materials")
        self.assertEqual(interest["current"]["phase"], "resting")
        self.assertEqual(interest["current"]["evidence"], [task["id"]])
        self.assertEqual(len(self.explorations()), 1)
        self.assertEqual(len(self.experience_rows()), 1)
        self.assert_no_replay()

    def test_new_exploration_receives_saved_interest_and_judgment_without_merging_sessions(self):
        first = self.start_and_end(tools=3, requests=4)
        before_first = copy.deepcopy(first)
        first_context = self.exploration_context(self.worker_requests(first)[0]["body"])
        first_calls = len(self.worker_calls)

        def continue_freely(observation, result):
            if self.reflections != 2:
                return
            result["choice"] = "explore"
            result["thought"] = "再随便看看，也可以休息"
            result["reason"] = "自己决定这次怎么过"
            result["judgments"] = [{
                "key": "materials-view", "statement": "这些材料的变化值得慢慢看",
                "confidence": 40, "evidence": [first["id"]],
                "revision_reason": "实际读过这段材料后形成的暂时看法"}]
            # 保留由前一段真实读取形成的 resting 兴趣；探索无需把它强行改回 active。
            self.assertEqual(result["interest_updates"][0]["phase"], "resting")

        self.after_reflection = continue_freely
        original_explore = self.explore

        def next_explore(body):
            context = self.exploration_context(body)
            if context["input"]["reflection_id"] == first_context["input"]["reflection_id"]:
                return original_explore(body)
            # 当前主观背景由宿主传递，不靠工具重新检索、预填目标或夹具补写持久库。
            return text_response("这次想到这里，先休息。")

        self.explore = next_explore
        self.due()

        def second_ended():
            self.wait_for(lambda: len(self.explorations()) == 2 and all(
                task["status"] == NORMAL_STATUS for task in self.explorations()),
                "第二段独立探索未自然结束", 20)
            self.wait_reconciled()
            self.assertFalse(self.run_release.wait(0.8))

        self.run_idle(self.checkpoint("second-exploration"), checkpoints={
            "second-exploration": second_ended})
        second = next(task for task in self.explorations() if task["id"] != first["id"])
        self.assertEqual(next(task for task in self.explorations() if task["id"] == first["id"]),
                         before_first, "后续兴趣变化不能改写原任务")
        self.assertEqual(self.grant()["issued"], 4)
        self.assertEqual(len(self.worker_calls), first_calls)
        self.assertEqual(second["usage"]["tool_calls"], 0)
        self.assert_quiet(second)
        self.assert_session_binding(first)
        self.assert_session_binding(second)
        first_requests, second_requests = self.worker_requests(first), self.worker_requests(second)
        first_body, second_body = first_requests[0]["body"], second_requests[0]["body"]
        self.assertNotEqual(first_body["prompt_cache_key"], second_body["prompt_cache_key"])
        for name in ("session-id", "thread-id", "x-client-request-id"):
            self.assertTrue({request["session_headers"][name] for request in first_requests}.isdisjoint(
                {request["session_headers"][name] for request in second_requests}), name)
        second_context = self.exploration_context(second_body)
        self.assertNotIn("action", second_context["input"])
        self.assertNotIn("exploration", second_context["input"])
        self.assertTrue(any(item["id"] == first["id"] for item in second_context["recent_experiences"]))
        self.assertIn("current_mind", second_context, "新探索缺少自己的持续认知")
        view = second_context["current_mind"]
        self.assertEqual(view["kind"], "eve.exploration.mind.v1")
        self.assertEqual(view["scope"], second_context["input"]["scope"])
        self.assertFalse(view["complete_history"])
        self.assertEqual(view["snapshot"], self.mind())
        self.assertEqual(view["snapshot"]["revision"], 2)
        self.assertEqual(view["snapshot"]["last_task"], second_context["input"]["reflection_id"])
        self.assertEqual(view["snapshot"]["self_interests"][0]["current"]["phase"], "resting")
        self.assertEqual(view["snapshot"]["judgments"][0]["key"], "materials-view")
        self.assertIn("current_mind", first_context)
        self.assertEqual(first_context["current_mind"]["snapshot"].get("self_interests", []), [])
        self.assertEqual(first_context["current_mind"]["snapshot"]["revision"], 1)
        self.assertEqual(len(self.kind("review")), 0)
        self.assertEqual(self.workspace_files(), self.initial_files)
        self.assert_no_replay()

    def test_exploration_notes_persist_into_next_context_and_history_stays_readable(self):
        self.mode = "notes"
        original_explore = self.explore
        written = {}

        def keep_notes(body):
            results = self.results(body)
            frozen = self.exploration_context(body)["notes_context"]
            self.assertEqual(frozen["revision"], 0)
            self.assertIsNone(frozen["rendered"])
            if len(results) < 3:
                return original_explore(body)
            if len(results) == 3:
                self.assertIn(self.nonce, results[-1]["content"])
                path = re.search(r"另一个方向的材料路径：([^\n]+)",
                                 results[1]["content"]).group(1)
                written["text"] = (f"资料：{path}\n未解问题：声音和色彩的变化有什么联系？\n"
                                   + "还没有答案，可以以后再看。" * 700)
                return self.call("context_notes", {"operation": "update", "expected_revision": 0,
                                                    "text": written["text"]})
            self.assertEqual(len(results), 4)
            self.assertEqual(results[-1]["view"]["revision"], 1)
            self.assertTrue(results[-1]["view"]["truncated"])
            return text_response("留下一点疑问，先休息。")

        self.explore = keep_notes
        first = self.start_and_end(tools=4, requests=5)
        before_first = copy.deepcopy(first)
        original_record = copy.deepcopy(self.experience(first))
        self.assertEqual(original_record["total_tool_calls"], 4)
        self.assertEqual(original_record["tools"][3]["tool"], "context_notes")
        self.assertEqual(original_record["tools"][3]["outcome"], "succeeded")
        note_bytes = written["text"].encode("utf8")
        self.assertGreater(len(note_bytes), 16384)

        def continue_freely(observation, result):
            if self.reflections == 2:
                result["choice"] = "explore"
                result["thought"] = "看看之前留下了什么，也可以继续放下"
                result["reason"] = "有一点好奇"
                actual = next(item for item in observation["evidence"] if item["id"] == first["id"])
                self.assertEqual(json.loads(actual["text"]), original_record)

        self.after_reflection = continue_freely
        readback = {"note": bytearray(), "record": bytearray(), "hash": None}
        current_note = "现在想先放下这个问题；旧资料仍可按版本回查。"

        def follow_notes(body):
            context = self.exploration_context(body)
            self.assertEqual(context["current_mind"]["snapshot"]["revision"], 2)
            frozen = context["notes_context"]
            self.assertEqual(frozen["kind"], "eve.exploration.notes.v1")
            self.assertEqual(frozen["revision"], 1)
            self.assertEqual(frozen["owner"], {"channel": context["input"]["scope"]["channel"],
                "session_id": context["input"]["scope"]["conversation_id"],
                "user_id": context["input"]["scope"]["principal_id"]})
            view = json.loads(frozen["rendered"].splitlines()[-1])
            prefix = view["text_prefix"].encode("utf8")
            self.assertEqual(view["revision"], 1)
            self.assertEqual(view["total_bytes"], len(note_bytes))
            self.assertTrue(view["truncated"])
            self.assertLessEqual(len(prefix), 16384)
            self.assertEqual(prefix, note_bytes[:len(prefix)])
            self.assertIn(self.second_name, view["text_prefix"])
            self.assertIn("未解问题", view["text_prefix"])
            results = self.results(body)
            if not results:
                return self.call("context_notes", {"operation": "read_range", "revision": 1,
                                                    "offset": 0, "limit": 16384})
            last = results[-1]
            if "page" in last:
                page = last["page"]
                self.assertEqual(page["revision"], 1)
                self.assertEqual(page["offset"], len(readback["note"]))
                readback["note"].extend(page["text"].encode("utf8"))
                if page["next_offset"] is not None:
                    return self.call("context_notes", {"operation": "read_range", "revision": 1,
                        "offset": page["next_offset"], "limit": 16384})
                self.assertEqual(bytes(readback["note"]), note_bytes)
                return self.call("read_exploration", {"action": "list", "limit": 1})
            if "items" in last:
                self.assertEqual(last["total"], 1)
                [index] = last["items"]
                self.assertEqual(index["session_id"], first["id"])
                self.assertEqual(index["subjective_notes"], original_record["subjective_notes"])
                self.assertIsNone(last["next_before"])
                return self.call("read_exploration", {"action": "read", "id": index["session_id"],
                                                       "offset": 0, "limit": 8192})
            if "sha256" in last and "text" in last:
                self.assertEqual(last["id"], first["id"])
                self.assertEqual(last["scope"], original_record["scope"])
                self.assertEqual(last["offset"], len(readback["record"]))
                if readback["hash"] is None:
                    readback["hash"] = last["sha256"]
                self.assertEqual(last["sha256"], readback["hash"])
                readback["record"].extend(last["text"].encode("utf8"))
                if last["next_offset"] is not None:
                    return self.call("read_exploration", {"action": "read", "id": first["id"],
                        "offset": last["next_offset"], "limit": 8192})
                raw = bytes(readback["record"])
                self.assertEqual(len(raw), last["total_bytes"])
                self.assertEqual(hashlib.sha256(raw).hexdigest(), readback["hash"])
                self.assertEqual(json.loads(raw), original_record)
                return self.call("context_notes", {"operation": "update", "expected_revision": 1,
                                                    "text": current_note})
            self.assertEqual(last["view"]["revision"], 2)
            self.assertEqual(last["view"]["text"], current_note)
            # 本次工具已保存修订2，但当前探索的冻结预览仍是修订1。
            return text_response("看到了旧问题和真实经历，现在先放下。")

        self.explore = follow_notes
        self.due()

        def second_ended():
            self.wait_for(lambda: len(self.explorations()) == 2 and all(
                task["status"] == NORMAL_STATUS for task in self.explorations()),
                "第二段探索未保存笔记和回查结果", 20)
            self.wait_reconciled()
            self.assertFalse(self.run_release.wait(0.8))

        self.run_idle(self.checkpoint("notes-followed"), checkpoints={"notes-followed": second_ended})
        second = next(task for task in self.explorations() if task["id"] != first["id"])
        self.assertEqual(next(task for task in self.explorations() if task["id"] == first["id"]), before_first)
        self.assertEqual(self.experience(first), original_record)
        self.assertEqual(json.loads(bytes(readback["record"])), original_record)
        self.assertEqual(bytes(readback["note"]), note_bytes)
        self.assert_session_binding(first)
        self.assert_session_binding(second)
        self.assertNotEqual(self.worker_requests(first)[0]["body"]["prompt_cache_key"],
                            self.worker_requests(second)[0]["body"]["prompt_cache_key"])
        self.assertEqual(self.grant()["issued"], 4)
        self.assertEqual(len(self.kind("review")), 0)
        self.assertEqual(self.workspace_files(), self.initial_files)
        self.assert_no_replay()

    def test_empty_exploration_can_rest_without_any_tool_or_artifact(self):
        self.mode = "rest"
        self.exploration_field = {}
        task = self.start_and_end(tools=0, requests=1)
        self.assertEqual(self.worker_calls, [])
        self.assertEqual(self.workspace_files(), self.initial_files)
        self.assertEqual(self.experience(task)["tools"], [])
        self.assertEqual(self.experience(task)["total_tool_calls"], 0)
        self.assertEqual(self.experience(task)["end_reason"], "natural")
        def verify_reflection(observation, result):
            if self.reflections < 2:
                return
            evidence = next(item for item in observation["evidence"] if item["id"] == task["id"])
            self.assertEqual(evidence["kind"], "exploration_reflection")
            self.assertEqual(json.loads(evidence["text"])["tools"], [])
        self.after_reflection = verify_reflection
        self.due()
        self.run_idle(self.checkpoint("rest-review"), checkpoints={
            "rest-review": lambda: (self.wait_revision(2), self.wait_reconciled())})
        self.assert_no_replay()

    def test_actual_tool_budget_ends_session_without_completion_contract(self):
        self.mode = "budget"
        self.steps_per_segment = 2
        task = self.start_and_end(tools=2)
        self.assertLessEqual(task["usage"]["model_requests"], 3)
        self.assertEqual(len(self.worker_calls), 2)
        self.assertEqual(task["execution"]["recoveries"], 0)
        self.assertEqual(task["execution"]["session_end_reason"], "tool_budget_exhausted")
        self.assertEqual(self.experience(task)["end_reason"], "budget_exhausted")
        self.assert_no_replay()

    def test_real_deadline_ends_session_without_forcing_a_report(self):
        self.mode = "hold"
        self.initialize()
        self.due()
        self.run_idle(self.checkpoint("deadline"), env_extra={"EVE_TASK_TIMEOUT_MS": "1500"},
                      checkpoints={"deadline": lambda: self.wait_end(tools=0, requests=1)})
        self.hold_worker.set()
        self.assertTrue(self.worker_returned.wait(5))
        task = self.explorations()[0]
        self.assertEqual(task["execution"]["session_end_reason"], "deadline_expired")
        self.assertEqual(task["execution"]["recoveries"], 0)
        self.assertIsNone(task["execution"]["checkpoint"]["result"])
        self.assertEqual(self.experience(task)["end_reason"], "budget_exhausted")
        self.assertIsNone(self.experience(task)["subjective_notes"])
        self.assert_no_replay()

    def test_confirmed_provider_failure_is_quiet_and_keeps_actual_receipts(self):
        self.mode = "failure"
        self.status_for_body = lambda body: 400 if self.is_worker(body) and self.results(body) else None
        task = self.start_and_end(status="Failed", tools=1, requests=2)
        self.assertTrue(task.get("failure"))
        self.assertFalse(task["execution"]["checkpoint"]["pending_calls"])
        self.assertEqual(self.experience(task)["end_reason"], "failed")
        self.assertEqual(self.experience(task)["tools"][0]["tool"], "list_files")
        self.assert_no_replay()

    def test_path_denial_is_a_real_experience_and_does_not_force_retry(self):
        self.mode = "denied"
        (self.work / "outside.txt").write_text("OUTSIDE_PRIVATE", encoding="utf8")
        task = self.start_and_end(tools=1, requests=2)
        [tool] = self.experience(task)["tools"]
        self.assertEqual(tool["outcome"], "denied")
        self.assertIsNotNone(tool["error"])
        self.assertNotIn("OUTSIDE_PRIVATE", json.dumps(self.experience(task)))
        self.assertEqual(self.workspace_files(), self.initial_files)
        self.assert_no_replay()

    def test_forged_evidence_does_not_become_an_interest_or_erase_history(self):
        self.mode = "rest"
        self.allow_idle_failure_notice = True
        task = self.start_and_end(tools=0, requests=1)
        before = copy.deepcopy(self.mind())
        experience = copy.deepcopy(self.experience(task))
        history = self.interest_history_snapshot()
        self.assertEqual(before.get("self_interests", []), [])
        self.assertEqual(len(history["exploration_experiences"]), 1)
        self.assertEqual(len(history["reflections"]), 1)
        def invent(observation, result):
            if self.reflections > 1:
                result["interest_updates"] = [{
                    "key": "invented-interest", "topic": "从未实际观察的材料", "phase": "active",
                    "strength": 90, "reason": "合成负例", "want_to_try": "继续",
                    "evidence": ["fabricated-external-evidence"]}]
        self.after_reflection = invent
        self.due()
        self.run_idle(self.checkpoint("forged"), checkpoints={"forged": lambda:
            self.wait_rejected_reflection_and_notice("伪造经历来源没有拒绝")})
        self.assertEqual(self.reflections, 2)
        self.assertEqual(self.mind()["revision"], before["revision"])
        self.assertEqual(self.mind().get("self_interests", []), before.get("self_interests", []))
        self.assertEqual(self.interest_history_snapshot(), history)
        self.assertEqual(self.experience(task), experience)
        self.assertEqual(len(self.explorations()), 1)
        self.assertEqual(len(self.kind("review")), self.baseline_reviews)

    def test_zero_tool_reflection_cannot_masquerade_as_external_evidence(self):
        self.mode = "rest"
        self.allow_idle_failure_notice = True
        task = self.start_and_end(tools=0, requests=1)
        before = copy.deepcopy(self.mind())
        experience = copy.deepcopy(self.experience(task))
        history = self.interest_history_snapshot()
        self.assertEqual(before.get("self_interests", []), [])
        self.assertEqual(len(history["exploration_experiences"]), 1)
        self.assertEqual(len(history["reflections"]), 1)
        def invent(observation, result):
            if self.reflections > 1:
                self.assertEqual(next(item for item in observation["evidence"]
                    if item["id"] == task["id"])["kind"], "exploration_reflection")
                result["interest_updates"] = [{
                    "key": "imagined-sound", "topic": "没有实际听到的声音", "phase": "active",
                    "strength": 90, "reason": "合成负例", "want_to_try": "继续",
                    "evidence": [task["id"]]}]
        self.after_reflection = invent
        self.due()
        self.run_idle(self.checkpoint("imagined"), checkpoints={"imagined": lambda:
            self.wait_rejected_reflection_and_notice("零工具感想被当成外部新证据")})
        self.assertEqual(self.reflections, 2)
        self.assertEqual(self.mind()["revision"], before["revision"])
        self.assertEqual(self.mind().get("self_interests", []), before.get("self_interests", []))
        self.assertEqual(self.interest_history_snapshot(), history)
        self.assertEqual(self.experience(task), experience)
        self.assertEqual(len(self.explorations()), 1)
        self.assertEqual(len(self.kind("review")), self.baseline_reviews)

    def test_saved_receipt_resumes_same_session_without_replaying_tool(self):
        self.mode = "resume"
        original_explore = self.explore
        note = "先记下一个未解问题；这次会话的原始便笺仍为空。"

        def save_note_then_wait(body):
            frozen = self.exploration_context(body)["notes_context"]
            self.assertEqual(frozen["revision"], 0)
            self.assertIsNone(frozen["rendered"])
            results = self.results(body)
            if not results:
                return self.call("context_notes", {"operation": "update", "expected_revision": 0,
                                                    "text": note})
            self.assertEqual(results[-1]["view"]["revision"], 1)
            self.assertEqual(results[-1]["view"]["text"], note)
            return original_explore(body)

        self.explore = save_note_then_wait
        self.initialize()
        self.due()
        stop = self.gate("stop-saved")
        def saved():
            self.wait_for(lambda: self.worker_waiting.is_set() and len(self.explorations()) == 1
                and self.explorations()[0]["execution"]["checkpoint"]["used_tool_calls"] == 1
                and not self.explorations()[0]["execution"]["checkpoint"]["pending_calls"],
                "真实工具回执没有保存", 20)
        self.run_idle([*self.checkpoint("saved"), {"touch": str(stop)},
                       {"wait_file": str(self.gate("never-saved"))}],
                      checkpoints={"saved": saved}, stop_at=stop)
        self.hold_worker.set()
        self.assertTrue(self.worker_returned.wait(5))
        before = copy.deepcopy(self.explorations()[0])
        self.assertEqual(before["status"], "Running")
        saved_checkpoint = before["execution"]["checkpoint"]
        self.assertGreater(saved_checkpoint["pending_model_request"], 0)
        self.assertRegex(saved_checkpoint["pending_request_fingerprint"], r"^[0-9a-f]{64}$")
        self.run_idle(self.checkpoint("resumed"), checkpoints={
            "resumed": lambda: self.wait_end(tools=1, requests=3)})
        after = self.explorations()[0]
        for field in ("id", "owner", "source", "spec", "created_at_ms", "started_at_ms"):
            self.assertEqual(after[field], before[field])
        for field in ("kind", "deadline_at_ms", "authorization_deadline_at_ms", "review_messages"):
            self.assertEqual(after["execution"].get(field), before["execution"].get(field))
        restored_checkpoint = after["execution"]["checkpoint"]
        self.assertEqual(restored_checkpoint["model_session"], saved_checkpoint["model_session"])
        self.assertIsNone(restored_checkpoint.get("pending_model_request"))
        self.assertIsNone(restored_checkpoint.get("pending_request_fingerprint"))
        self.assertEqual(after["execution"]["recoveries"], 1)
        self.assertEqual(len(self.worker_calls), 1)
        [record] = self.documents()["eve.context-notes"]["index.v1"]["records"].values()
        self.assertEqual(len(record["versions"]), 1, "恢复不得重放便笺保存")
        self.assertEqual(record["versions"][0]["revision"], 1)
        self.assert_no_replay()

    def test_real_inflight_operation_stays_unknown_and_is_never_replayed(self):
        self.mode = "unknown"
        self.configure_developer(commands=True)
        self.forbidden_action_tools.remove("run_command")
        self.command_release = self.work / "release-local-command"
        self.inflight_command = {"program": sys.executable, "args": ["-c",
            "from pathlib import Path; import time; "
            "p=Path('once.txt'); p.write_text(p.read_text()+'x' if p.exists() else 'x'); "
            "assert p.read_text()=='x'; gate=Path(" + repr(str(self.command_release)) + "); "
            "deadline=time.monotonic()+8\n"
            "while not gate.exists() and time.monotonic()<deadline: time.sleep(.01)\n"
            "print('LOCAL_OPERATION_RETURNED')"]}
        self.initialize()
        self.due()
        stop = self.gate("stop-operation")
        def inflight():
            self.wait_for(lambda: (self.source / "once.txt").exists() and len(self.explorations()) == 1
                and len(self.explorations()[0]["execution"]["checkpoint"]["pending_calls"]) == 1,
                "操作没有在持久pending检查点后真实启动", 20)
            self.assertEqual((self.source / "once.txt").read_text(), "x")
        self.run_idle([*self.checkpoint("inflight"), {"touch": str(stop)},
                       {"wait_file": str(self.gate("never-operation"))}],
                      checkpoints={"inflight": inflight}, stop_at=stop)
        self.command_release.touch()
        before = copy.deepcopy(self.explorations()[0])
        requests = len(self.requests)
        self.run_idle(self.checkpoint("unknown"), checkpoints={
            "unknown": lambda: self.wait_end(status="Interrupted", tools=1, requests=1)})
        task = self.explorations()[0]
        self.assertEqual(len(self.requests), requests)
        self.assertEqual(task["id"], before["id"])
        self.assertEqual(task["execution"]["checkpoint"]["pending_calls"],
                         before["execution"]["checkpoint"]["pending_calls"])
        self.assertEqual((self.source / "once.txt").read_text(), "x")
        experience = self.experience(task)
        self.assertEqual(experience["end_reason"], "unknown")
        self.assertEqual(experience["tools"][0]["outcome"], "unknown")
        self.assertIsNone(experience["tools"][0]["output"])
        self.assert_no_replay()

    def test_cancellation_ends_quietly_without_worker_replay(self):
        self.mode = "hold"
        self.initialize()
        self.due()
        def cancel(body):
            if self.results(body):
                return text_response("已请求停止探索")
            return tool_response([("cancel_background_task", {"task_id": self.explorations()[0]["id"]})])
        self.foreground = cancel
        self.run_idle([*self.checkpoint("waiting"), *self.send(self.message(
            "stop-free", "暂停这一段探索", expected="已请求停止探索")),
            *self.checkpoint("cancelled")], checkpoints={
                "waiting": lambda: self.wait_for(self.worker_waiting.is_set, "探索未启动", 20),
                "cancelled": lambda: self.wait_end(status="Cancelled", tools=0, requests=1)})
        self.hold_worker.set()
        self.assertTrue(self.worker_returned.wait(5))
        self.assertEqual(self.experience(self.explorations()[0])["end_reason"], "cancelled")
        self.assert_no_replay()

    def test_authorization_budget_exhaustion_cannot_create_another_session(self):
        self.mode = "rest"
        self.max_segments = 2
        self.start_and_end(tools=0, requests=1)
        self.assertEqual(self.grant()["issued"], 2)
        before = copy.deepcopy(self.tasks())
        requests = len(self.requests)
        self.due()
        self.run_idle(self.checkpoint("exhausted"), checkpoints={
            "exhausted": lambda: self.assertFalse(self.run_release.wait(1.3))})
        self.assertEqual(self.tasks(), before)
        self.assertEqual(len(self.requests), requests)
        self.assertEqual(self.grant()["issued"], 2)

    def age_stopped_exploration(self, task_id):
        """只把已退出夹具的任务时刻前移，保留原授权期限和精确检查点。"""
        raw = json.loads(self.state_path.read_text())
        ledger = json.loads(bytes(raw["entries"]["eve.task"]["tasks.v1"]))
        task = copy.deepcopy(next(task for task in self.tasks() if task["id"] == task_id))
        self.assertEqual(task["status"], "Running")
        deadline = task["execution"]["deadline_at_ms"]
        self.assertTrue(deadline is None or deadline > int(time.time() * 1000))
        delta = 31 * 60 * 1000
        task["created_at_ms"] -= delta
        task["started_at_ms"] -= delta
        if ledger["format_version"] in (4, 5):
            entry = next(entry for entry in ledger["tasks"] if entry["directory"]["id"] == task_id)
            if ledger["format_version"] == 5:
                self.assertIs(entry["archived"], False, "运行任务不能占用历史归档位")
            payload = json.dumps(task, ensure_ascii=False, separators=(",", ":")).encode()
            digest = hashlib.sha256(payload).hexdigest()
            archive = self.state_path.parent / "task-archive" / (digest + ".json")
            self.assertFalse(archive.exists())
            archive.write_bytes(payload)
            metadata = copy.deepcopy(task)
            metadata["spec"].pop("context", None)
            metadata["result"] = metadata["failure"] = None
            metadata["execution"]["checkpoint"] = None
            metadata["execution"].pop("review_messages", None)
            entry.update(directory=metadata, record={"hash": digest, "bytes": len(payload)})
        else:
            index = next(index for index, item in enumerate(ledger["tasks"]) if item["id"] == task_id)
            ledger["tasks"][index] = task
        raw["entries"]["eve.task"]["tasks.v1"] = list(json.dumps(ledger).encode())
        self.state_path.write_text(json.dumps(raw), encoding="utf8")
        self.assertEqual(next(item for item in self.tasks() if item["id"] == task_id), task)
        return task

    def test_due_running_session_remains_silent_across_real_sender_scans(self):
        self.mode = "hold"
        self.initialize()
        self.due()
        stop = self.gate("stop-young")
        self.run_idle([*self.checkpoint("young"), {"touch": str(stop)},
                       {"wait_file": str(self.gate("never-young"))}],
                      checkpoints={"young": lambda: self.wait_for(
                          self.worker_waiting.is_set, "探索模型未启动", 20)}, stop_at=stop)
        self.hold_worker.set()
        self.assertTrue(self.worker_returned.wait(5))
        self.hold_worker = threading.Event()
        self.worker_waiting.clear()
        self.worker_returned.clear()
        task = self.age_stopped_exploration(self.explorations()[0]["id"])
        stop = self.gate("stop-aged")
        def silent():
            self.wait_for(self.worker_waiting.is_set, "已到通知时刻的探索未接续", 20)
            until = time.monotonic() + 2.2
            while time.monotonic() < until:
                actual = self.explorations()[0]
                self.assertEqual(actual["status"], "Running")
                self.assert_quiet(actual)
                self.assertFalse(self.run_release.wait(0.05))
        self.run_idle([*self.checkpoint("aged"), {"touch": str(stop)},
                       {"wait_file": str(self.gate("never-aged"))}],
                      checkpoints={"aged": silent}, stop_at=stop)
        self.hold_worker.set()
        self.assertTrue(self.worker_returned.wait(5))
        after = self.explorations()[0]
        self.assertEqual(after["id"], task["id"])
        self.assertEqual(after["execution"]["authorization_deadline_at_ms"],
                         task["execution"]["authorization_deadline_at_ms"])
        self.assertEqual(self.worker_calls, [])


class ExistingIdleActionCompatibility(idle.IdleLoopAcceptance):
    """复用旧 Act 真实写读、冻结合同与重启测试；不把普通任务改成探索。"""
    for _name in dir(idle.IdleLoopAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def test_old_act_still_uses_frozen_receipts_and_completes(self):
        idle.IdleLoopAcceptance.test_idle_action_and_changed_judgment_across_processes_without_new_user_message(self)

    def test_legacy_act_still_plans_missing_contract_before_tools(self):
        idle.IdleLoopAcceptance.test_legacy_action_freezes_receipts_in_zero_tool_planning_before_execution(self)


if __name__ == "__main__":
    unittest.main()
