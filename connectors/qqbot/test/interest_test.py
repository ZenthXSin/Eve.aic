"""实际 eve-qqbot 的兴趣观察与学习目标派生验收；只连接本地模型替身和 fake bridge。

用户只在普通聊天里提一次兴趣，不使用 /goal。正向时序通过文件门控和真实持久状态确认；
短暂观察窗口只验证没有后台重试。停止只操作本测试持有的 Popen，不向任意 PID 发信号。
确定性替身只证明宿主契约与恢复语义，不代表真实模型的观察质量。
"""
import json
import os
import pathlib
import subprocess
import tempfile
import threading
import time
import traceback
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import memory_test
from memory_test import ARTIFACT, BINARY, FAKE, ROOT

MINDUSTRY = "我喜欢 Mindustry 这个游戏的模组，但是不知道怎么创作。"
QUIT = "算了，我现在对 Mindustry 模组没兴趣了。"
WATERCOLOR = "我最近迷上了水彩画，但调色总是很脏。"
DISABLED = "兴趣观察未启用。"
EMPTY = "当前会话还没有记录兴趣。"
WITHDRAWN = "已撤回这条兴趣；之后不会再据此后台学习或主动提起。"


class InterestAcceptance(unittest.TestCase):
    documents = memory_test.MemoryAcceptance.documents
    memory = memory_test.MemoryAcceptance.memory
    snapshots = memory_test.MemoryAcceptance.snapshots
    evidence = memory_test.MemoryAcceptance.evidence
    interactions = memory_test.MemoryAcceptance.interactions
    gate = memory_test.MemoryAcceptance.gate
    arrived = memory_test.MemoryAcceptance.arrived
    checkpoint = memory_test.MemoryAcceptance.checkpoint
    wait_until = memory_test.MemoryAcceptance.wait_until
    message = memory_test.MemoryAcceptance.message
    wait_reply = memory_test.MemoryAcceptance.wait_reply
    wait_receipt = memory_test.MemoryAcceptance.wait_receipt
    send = memory_test.MemoryAcceptance.send

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.work = pathlib.Path(self.directory.name)
        self.state_path = self.work / "state/state.json"
        self.requests = []
        self.errors = []
        self.lock = threading.Lock()
        self.runs = 0
        self.response_gates = {}
        self.run_release = threading.Event()
        self.observe = self.default_observe
        self.provider_failure = False
        self.failing = set()
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
                    outer.errors.append(traceback.format_exc())
                    self.send_error(500)

            def respond(self):
                assert self.path == "/v1/chat/completions"
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                latest = body["messages"][-1]["content"]
                try:
                    decoded = json.loads(latest)
                except (ValueError, TypeError):
                    decoded = None
                kind = outer.classify(decoded, latest)
                release = outer.run_release
                with outer.lock:
                    outer.requests.append({"kind": kind, "body": body, "run": outer.runs,
                                           "state": outer.documents()})
                    number = sum(request["kind"] == kind for request in outer.requests)
                outer.arrived(kind, number).touch()
                gate = outer.response_gates.get((kind, number))
                if gate is not None:
                    deadline = time.monotonic() + 15
                    while not outer.gate(gate).exists():
                        if release.wait(0.01):
                            return
                        assert time.monotonic() < deadline, "response gate timed out"
                failing = kind in outer.failing or (kind == "interest" and outer.provider_failure)
                status = 503 if failing else 200
                text = outer.reply(kind, decoded, latest)
                response = ({"error": {"message": "local provider unavailable", "type": "test_error"}}
                            if status != 200 else {"choices": [{"index": 0, "finish_reason": "stop",
                                                                 "message": {"role": "assistant", "content": text}}]})
                encoded = json.dumps(response).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.run_release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.directory.cleanup()

    def classify(self, decoded, latest):
        if isinstance(decoded, dict) and "observer_version" in decoded and "known_interests" in decoded:
            return "interest"
        if "unverified_waiting_input" in latest:
            return "reflection"
        return "chat"

    def reply(self, kind, decoded, latest):
        if kind == "interest":
            return json.dumps(self.observe(decoded), ensure_ascii=False)
        if kind == "reflection":
            return json.dumps(ARTIFACT, ensure_ascii=False)
        return latest

    # 确定性观察替身：只按批次中真实的用户原文作答，不读取助手回复。
    def default_observe(self, batch):
        updates = []
        for evidence in batch["evidence"]:
            text = evidence["source"]["user_text"]
            if text == MINDUSTRY:
                updates.append({
                    "target": {"new": {"topic": "Mindustry 模组创作"}},
                    "statements": [
                        {"kind": "interest", "quote": "我喜欢 Mindustry 这个游戏的模组", "evidence_id": evidence["id"]},
                        {"kind": "difficulty", "quote": "不知道怎么创作", "evidence_id": evidence["id"]}],
                    "inferred_need": "可能希望学习如何制作模组"})
            elif text == WATERCOLOR:
                updates.append({
                    "target": {"new": {"topic": "水彩画调色"}},
                    "statements": [
                        {"kind": "interest", "quote": "我最近迷上了水彩画", "evidence_id": evidence["id"]},
                        {"kind": "difficulty", "quote": "调色总是很脏", "evidence_id": evidence["id"]}]})
            elif text == QUIT and batch["known_interests"]:
                updates.append({
                    "target": {"existing": {"id": batch["known_interests"][0]["id"]}},
                    "statements": [{"kind": "withdrawal", "quote": "我现在对 Mindustry 模组没兴趣了",
                                    "evidence_id": evidence["id"]}]})
        return {"updates": updates}

    def interest_state(self):
        return self.documents().get("eve.interest", {}).get("interests.v1")

    def jobs(self):
        return [record["job"] for record in (self.interest_state() or {}).get("jobs", [])]

    def interests(self):
        return (self.interest_state() or {}).get("interests", [])

    def cognition(self):
        return self.documents().get("eve.cognition", {}).get("cognition.v1") or {"state": {"goals": {}, "events": []}}

    def goals(self):
        return list(self.cognition()["state"]["goals"].values())

    def learning_goal(self, interest_id):
        return next((goal for goal in self.goals() if goal["source"] == {
            "kind": "Inference", "channel": "interest.learning", "reference": interest_id}), None)

    def reflection(self, parent_id):
        return next((goal for goal in self.goals() if goal["source"]["kind"] == "Inference"
                     and goal["source"]["channel"] == "endogenous"
                     and goal["source"]["reference"] == parent_id), None)

    def kind(self, kind):
        return [request for request in self.requests if request["kind"] == kind]

    def quiet(self):
        # 只作负向断言：覆盖至少两轮 250ms 扫描，不能替代正向状态门控。
        self.assertFalse(self.run_release.wait(0.65), "process stopped during observation")

    def run_eve(self, script, interest=True, memory=False, cognition=False, max_executions=1,
                checkpoints=None, stop_at=None, extra=None, timeout=30, env_extra=None):
        self.runs += 1
        self.run_release = threading.Event()
        events_path = self.work / f"events-{self.runs}.jsonl"
        error_path = self.work / f"error-{self.runs}.txt"
        scenario = self.work / f"scenario-{self.runs}.json"
        scenario.write_text(json.dumps({"script": script, "events_file": str(events_path),
                                       "error_file": str(error_path)}), encoding="utf8")
        env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP") if key in os.environ}
        env.update(QQBOT_APP_SECRET="test-app-secret", QQBOT_APP_ID="1904159860",
                   EVE_OPENAI_API_KEY="test-model-secret",
                   EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}",
                   EVE_OPENAI_PROTOCOL="chat", EVE_LLM_RESPONSE_MODE="complete")
        env.update(env_extra or {})
        command = [str(BINARY), "--state-dir", str(self.work / "state"),
                   "--agent", str(ROOT / "AGENT.md"), "--bridge-script", str(FAKE),
                   "--bridge-arg", str(scenario), "--cognition-max-executions", str(max_executions)]
        # 只传 --interest-learning 时须自动开启记忆和认知。
        if interest:
            command.extend(["--interest-learning", "--interest-cooldown-ms", "0"])
        if memory:
            command.append("--memory")
        if cognition:
            command.append("--cognition")
        command.extend(extra or [])

        def inspect():
            for name, callback in (checkpoints or {}).items():
                try:
                    self.wait_until(lambda: self.gate(name + "-inspect").exists(), name + " not reached")
                    callback()
                except Exception:
                    self.errors.append(traceback.format_exc())
                finally:
                    self.gate(name + "-continue").touch()

        inspector = threading.Thread(target=inspect, daemon=True)
        child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        inspector.start()
        try:
            if stop_at is not None:
                self.wait_until(lambda: stop_at.exists() or child.poll() is not None, "stop gate was not reached")
                self.assertTrue(stop_at.exists(), "process exited before stop synchronization")
                child.kill()
            stdout, stderr = child.communicate(timeout=timeout)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate()
            self.run_release.set()
            inspector.join(timeout=2)
        self.assertFalse(inspector.is_alive(), "state inspector did not stop")
        self.assertFalse(error_path.exists(), error_path.read_text() if error_path.exists() else "")
        self.assertFalse(self.errors, "\n".join(self.errors))
        for secret in ("test-model-secret", "test-app-secret"):
            self.assertNotIn(secret, stdout + stderr)
        if stop_at is not None:
            self.assertNotEqual(child.returncode, 0)
            return None
        self.assertEqual(child.returncode, 0, stderr)
        summary = json.loads(stdout)
        self.assertTrue(summary["closed"])
        self.assertFalse(summary["terminal_error"])
        return summary

    def inspect_run(self, callback, prefix=None, **options):
        name = f"inspect-{self.runs + 1}"
        self.run_eve([*(prefix or []), *self.checkpoint(name)], checkpoints={name: callback}, **options)

    def reflected(self):
        interests = self.interests()
        if len(interests) != 1:
            return False
        goal = self.learning_goal(interests[0]["id"])
        child = goal and self.reflection(goal["id"])
        return bool(child and child["status"] == "Completed")

    def test_one_casual_mention_becomes_sourced_interest_and_background_learning_without_replay(self):
        self.inspect_run(lambda: self.wait_until(self.reflected, "learning goal was not reflected in background"),
                         prefix=self.send(self.message("casual", MINDUSTRY)))
        # 一次聊天、一次观察、一次后台反思；没有 /goal，也没有用户的后续消息。
        self.assertEqual([request["kind"] for request in self.requests], ["chat", "interest", "reflection"])
        observation = self.kind("interest")[0]
        saved = observation["state"]["eve.interest"]["interests.v1"]["jobs"]
        self.assertEqual([record["job"]["status"] for record in saved], ["Running"], "请求前已保存准入")
        rules = observation["body"]["messages"][0]["content"]
        for domain in ("Mindustry", "模组", "游戏"):
            self.assertNotIn(domain, rules, "观察规则不能按领域关键词分支")
        self.assertFalse(observation["body"].get("tools"), "观察请求不安装工具")

        interaction = self.interactions()[0]
        self.assertEqual(interaction["source"]["user_text"], MINDUSTRY)
        [interest] = self.interests()
        self.assertEqual(interest["topic"], "Mindustry 模组创作")
        self.assertEqual(interest["status"], "Active")
        for statement in interest["statements"]:
            self.assertIn(statement["quote"], MINDUSTRY)
            self.assertEqual(statement["evidence_id"], interaction["id"])
            self.assertEqual(statement["message_id"], "casual")
            self.assertEqual(statement["observed_at_ms"], interaction["at_ms"])
        self.assertEqual([s["kind"] for s in interest["statements"]], ["interest", "difficulty"])
        self.assertEqual(interest["inferences"][0]["text"], "可能希望学习如何制作模组")

        goal = self.learning_goal(interest["id"])
        self.assertEqual(goal["status"], "Waiting")
        self.assertEqual(goal["verification"], "interest-learning:v1")
        self.assertEqual(goal["visibility"], {"User": self.cognition_user()})
        self.assertLess(goal["priority"], 50)
        for expected in ("不是用户下达的任务", "“我喜欢 Mindustry 这个游戏的模组”", "“不知道怎么创作”",
                         "模型推断（未经用户确认，不是任务）"):
            self.assertIn(expected, goal["description"])
        self.assertFalse(any(goal["source"]["kind"] == "User" for goal in self.goals()), "没有用户待办")
        reflection = self.kind("reflection")[0]["body"]["messages"][-1]["content"]
        self.assertIn("不是用户下达的任务", reflection)
        before = self.documents()

        # 重启不重放观察或反思；命令只读当前会话，其他用户看不到这条兴趣。
        self.run_eve([
            *self.send(self.message("own-list", "/interests", contains=["Mindustry 模组创作", "后台学习中",
                                                                       interest["id"]])),
            *self.send(self.message("other-list", "/interests", user="user-2", expected=EMPTY)),
            *self.checkpoint("quiet")], checkpoints={"quiet": self.quiet})
        self.assertEqual(len(self.requests), 3, "restart must not replay observation or reflection")
        self.assertEqual(self.interest_state(), before["eve.interest"]["interests.v1"])
        self.assertEqual(self.learning_goal(interest["id"]), goal)

    def cognition_user(self):
        user_ids = {snapshot["scope"]["user_id"] for snapshot in self.snapshots()}
        self.assertEqual(len(user_ids), 1)
        return user_ids.pop()

    def test_chat_withdrawal_and_command_withdrawal_cancel_goals_and_held_out_domain_uses_same_contract(self):
        def withdrawn():
            [interest] = self.interests()
            goal = self.learning_goal(interest["id"])
            return interest["status"] == "Withdrawn" and goal and goal["status"] == "Cancelled"

        def derived():
            return any(self.learning_goal(interest["id"]) for interest in self.interests())

        self.inspect_run(lambda: self.wait_until(derived, "learning goal was not derived"),
                         prefix=self.send(self.message("casual", MINDUSTRY)), max_executions=4)
        self.inspect_run(lambda: self.wait_until(withdrawn, "chat withdrawal did not cancel the goal"),
                         prefix=self.send(self.message("quit", QUIT)), max_executions=4)
        [interest] = self.interests()
        self.assertEqual(interest["statements"][-1]["kind"], "withdrawal")
        self.assertEqual(interest["statements"][-1]["quote"], "我现在对 Mindustry 模组没兴趣了")
        observations = self.kind("interest")
        self.assertEqual(observations[1]["body"]["messages"][-1]["content"].count(interest["id"]), 1)

        # 留出领域：同一观察规则、同一描述结构，不新增关键词分支。
        self.inspect_run(lambda: self.wait_until(lambda: len(self.interests()) == 2 and all(
            self.learning_goal(item["id"]) for item in self.interests() if item["status"] == "Active"),
            "held-out interest goal was not derived"),
            prefix=self.send(self.message("watercolor", WATERCOLOR)), max_executions=4)
        watercolor = next(item for item in self.interests() if item["topic"] == "水彩画调色")
        goal = self.learning_goal(watercolor["id"])
        self.assertEqual(goal["status"], "Waiting")
        self.assertIn("“我最近迷上了水彩画”", goal["description"])
        self.assertNotIn("模型推断", goal["description"], "没有推断时不编造")
        systems = {request["body"]["messages"][0]["content"] for request in self.kind("interest")}
        self.assertEqual(len(systems), 1)

        self.run_eve([
            *self.send(self.message("forget", f"/forget-interest {watercolor['id']}", expected=WITHDRAWN)),
            *self.send(self.message("forget-again", f"/forget-interest {watercolor['id']}", expected=WITHDRAWN)),
            *self.send(self.message("forget-other", f"/forget-interest {watercolor['id']}", user="user-2",
                                    contains="当前会话没有这条兴趣")),
        ], max_executions=4)
        watercolor = next(item for item in self.interests() if item["topic"] == "水彩画调色")
        self.assertEqual(watercolor["status"], "Withdrawn")
        self.assertEqual(watercolor["statements"][-1]["origin"], "user_command")
        self.assertEqual(self.learning_goal(watercolor["id"])["status"], "Cancelled")
        self.assertEqual(len([s for s in watercolor["statements"] if s["kind"] == "withdrawal"]), 1)

    def test_unquoted_output_and_provider_failure_save_no_interest_and_never_retry(self):
        def paraphrase(batch):
            evidence = batch["evidence"][0]
            return {"updates": [{"target": {"new": {"topic": "模组"}}, "statements": [
                {"kind": "interest", "quote": "用户很喜欢模组创作", "evidence_id": evidence["id"]}]}]}

        self.observe = paraphrase
        self.inspect_run(lambda: self.wait_until(lambda: [job["status"] for job in self.jobs()] == [
            {"Failed": "InvalidOutput"}], "invalid output was not recorded"),
            prefix=self.send(self.message("casual", MINDUSTRY)))
        self.provider_failure = True
        self.inspect_run(lambda: self.wait_until(lambda: len(self.jobs()) == 2 and self.jobs()[1]["status"] == {
            "Failed": "Provider"}, "provider failure was not recorded"),
            prefix=self.send(self.message("again", WATERCOLOR)))
        self.assertEqual(self.interests(), [])
        self.assertEqual(self.goals(), [])
        self.run_eve(self.checkpoint("quiet"), checkpoints={"quiet": self.quiet})
        self.assertEqual(len(self.kind("interest")), 2, "failed batches consume their evidence")

    def test_interrupted_observation_is_interrupted_after_restart_without_replay(self):
        self.response_gates = {("interest", 1): "never-released"}
        # 桥接保持在线，直到观察请求已经到达模型替身，再强制结束进程。
        self.run_eve([*self.send(self.message("casual", MINDUSTRY)),
                      {"wait_file": str(self.gate("never-released"))}], stop_at=self.arrived("interest", 1))
        self.assertEqual([job["status"] for job in self.jobs()], ["Running"])
        self.run_eve(self.checkpoint("quiet"), checkpoints={"quiet": self.quiet})
        self.assertEqual([job["status"] for job in self.jobs()], ["Interrupted"])
        self.assertEqual(self.jobs()[0]["finished_at_ms"], None)
        self.assertEqual(len(self.kind("interest")), 1, "interrupted observation is not replayed")
        self.assertEqual(self.interests(), [])

    def test_disabled_by_default_makes_no_observation_and_commands_say_disabled(self):
        self.run_eve([
            *self.send(self.message("casual", MINDUSTRY)),
            *self.send(self.message("list", "/interests", expected=DISABLED)),
            *self.checkpoint("quiet")], interest=False, memory=True, cognition=True,
            checkpoints={"quiet": self.quiet})
        self.assertEqual([request["kind"] for request in self.requests], ["chat"])
        self.assertIsNone(self.interest_state())
        self.assertEqual(self.goals(), [])


if __name__ == "__main__":
    unittest.main()
