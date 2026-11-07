import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("training", Path(__file__).resolve().parents[1] / "training-window.py")
training = importlib.util.module_from_spec(spec)
spec.loader.exec_module(training)


class ReplyAnchorReports(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="eve-reply-anchor-training-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.path = self.source / "state.json"
        message = dict(id="original", scope="group", target_id="group-1", user_id="user-1", text="继续读源码")
        self.parent = dict(app_id="app", message=message, state="Handled", reply=None,
                           control_turn_id=1, progress=[])
        self.followup = dict(app_id="app", message={**message, "id": "later", "text": "然后呢"},
                             state="Merged", reply=None, merged_into="original")
        self.ledger = dict(version=10, entries=[self.parent, self.followup])
        routing = json.dumps(["app", "group", "group-1", "user-1"], separators=(",", ":")).encode()
        self.identity = "qq:" + hashlib.sha256(routing).hexdigest()
        self.sessions = dict(format_version=1, sessions={self.identity: dict(
            key=dict(session_id=self.identity, user_id=self.identity), turns=[dict(
                id=1, input=message["text"], status=dict(state="Completed", messages=[
                    dict(role="User", text=message["text"]), dict(role="Assistant", text="仅供内部收尾")]))])})

    def part(self, index, anchor=None, state="Sent", **extra):
        part = dict(text="实际发言 " + str(index), state=state, decision_id="call-" + str(index))
        if state == "Sent":
            part["platform_message_id"] = "platform-" + str(index)
        if anchor is not None:
            part["reply_anchor"] = anchor
        return {**part, **extra}

    def save(self):
        training.write_json(self.path, dict(version=1, entries={
            "eve.channel.qqbot": {"receipts.v1": list(json.dumps(self.ledger).encode())},
            "eve.session": {"sessions.v1": list(json.dumps(self.sessions).encode())}}))

    def archive(self, entries):
        database = self.source / "qq-receipts.sqlite3"
        with sqlite3.connect(database) as db:
            db.executescript("CREATE TABLE IF NOT EXISTS metadata(version INTEGER,identity TEXT); "
                            "CREATE TABLE IF NOT EXISTS receipts(seq INTEGER PRIMARY KEY,app TEXT,message_id TEXT,payload BLOB);")
            db.execute("DELETE FROM metadata")
            db.execute("INSERT INTO metadata VALUES(1,?)", ("a" * 32,))
            db.execute("DELETE FROM receipts")
            for index, entry in enumerate(entries, 1):
                db.execute("INSERT INTO receipts VALUES(?,?,?,?)",
                           (index, entry["app_id"], entry["message"]["id"], json.dumps(entry).encode()))
        self.ledger["archive"] = dict(identity="a" * 32, records=len(entries))

    def assert_rejected_without_write(self, code):
        self.save()
        before = {path: path.read_bytes() for path in self.source.rglob("*") if path.is_file()}
        with self.assertRaisesRegex(ValueError, code):
            training.collect(self.path)
        self.assertEqual({path: path.read_bytes() for path in self.source.rglob("*") if path.is_file()}, before)

    def test_report_preserves_each_actual_anchor_and_proactive_has_no_passive_window(self):
        self.parent["progress"] = [self.part(i) for i in range(3)] + [self.part(i, "later") for i in range(3, 6)]
        self.parent["progress"].append(self.part(6, proactive=True))
        self.save()
        before = self.path.read_bytes()
        report = training.collect(self.path)
        self.assertEqual(report["counts"]["received_records"], 2)
        self.assertEqual(report["counts"]["model_sent"], 1)
        self.assertEqual(report["counts"]["unconfirmed"], 0)
        evidence = report["evidence"][0]
        self.assertEqual(evidence["control_turn_id"], 1)
        self.assertNotIn("仅供内部收尾", evidence["reply"])
        self.assertEqual([p["reply_anchor"] for p in evidence["deliveries"]],
                         ["original"] * 3 + ["later"] * 3 + [None])
        self.assertEqual([p["proactive"] for p in evidence["deliveries"]], [False] * 6 + [True])
        self.assertEqual(self.path.read_bytes(), before)

    def test_cold_whole_group_retains_v10_and_seed_copies_unmodified_history(self):
        self.parent["progress"] = [self.part(1, "later"), self.part(2, state="Unknown")]
        self.archive(self.ledger["entries"])
        self.ledger["entries"] = []
        self.save()
        before = {p.name: p.read_bytes() for p in self.source.iterdir()}
        receipts, _ = training.evidence_documents(self.path)
        self.assertEqual(receipts["version"], 10)
        self.assertEqual(len(receipts["entries"]), 2)
        report = training.collect(self.path)
        self.assertEqual(report["counts"]["model_sent"], 1)
        self.assertEqual(report["counts"]["unconfirmed"], 1)
        self.assertEqual(report["evidence"][0]["deliveries"][0]["reply_anchor"], "later")
        destination = self.root / "next"
        destination.mkdir()
        training.seed_round(destination, self.source)
        self.assertEqual(training.collect(destination / "state/state.json")["counts"]["received_records"], 0)
        self.assertEqual({p.name: p.read_bytes() for p in self.source.iterdir()}, before)
        for name, payload in before.items():
            self.assertEqual((destination / "state" / name).read_bytes(), payload)

    def test_hot_and_cold_cannot_complete_each_others_anchor_group(self):
        self.parent["progress"] = [self.part(1, "later")]
        cases = ([self.parent], [self.followup]), ([self.followup], [self.parent]), ([], [self.parent]), ([], [self.followup])
        for hot, cold in cases:
            with self.subTest(hot=[r["message"]["id"] for r in hot]):
                self.archive(cold)
                self.ledger["entries"] = hot
                self.assert_rejected_without_write("invalid_reply_anchor_relation|invalid_merged_receipt")

    def test_cold_anchors_require_the_persistent_v10_format_marker(self):
        self.parent["progress"] = [self.part(1, "later")]
        self.archive(self.ledger["entries"])
        self.ledger["entries"] = []
        for version in (6, 9):
            with self.subTest(version=version):
                self.ledger["version"] = version
                self.assert_rejected_without_write("invalid_reply_anchor_version")

    def test_invalid_ids_relations_and_overdrawn_windows_are_rejected_in_both_partitions(self):
        self.parent["progress"] = [self.part(1, "later")]
        original = copy.deepcopy(self.ledger["entries"])
        cases = (
            lambda: self.followup["message"].update(user_id="other-user"),
            lambda: self.followup["message"].update(target_id="other-group"),
            lambda: self.followup.update(app_id="other-app"),
            lambda: self.followup.update(merged_into="missing"),
            lambda: self.parent["progress"][0].update(reply_anchor="later\n"),
            lambda: self.parent["progress"][0].update(proactive=True),
            lambda: self.parent["progress"].extend(self.part(i, "later") for i in range(2, 5)),
            lambda: self.parent["progress"].extend(self.part(i) for i in range(2, 6)),
        )
        for cold in (False, True):
            for index, mutate in enumerate(cases):
                with self.subTest(cold=cold, case=index):
                    self.ledger = dict(version=10, entries=copy.deepcopy(original))
                    self.parent, self.followup = self.ledger["entries"]
                    mutate()
                    if cold:
                        self.archive(self.ledger["entries"])
                        self.ledger["entries"] = []
                    self.assert_rejected_without_write("invalid_merged_receipt|invalid_reply_anchor|invalid_passive_progress_budget")

    def test_v10_silent_and_unconfirmed_turns_do_not_become_training_successes(self):
        for state in ("Sending", "Unknown", "Failed"):
            with self.subTest(state=state):
                self.parent["progress"] = [self.part(1, "later", state)]
                self.save()
                report = training.collect(self.path)
                self.assertEqual(report["evidence"], [])
                self.assertEqual(report["counts"]["model_sent"], 0)
                self.assertEqual(report["counts"]["failed" if state == "Failed" else "unconfirmed"], 1)
        self.parent["progress"] = []
        self.save()
        self.assertEqual(training.collect(self.path)["counts"]["silent_turns"], 1)
        self.parent["progress"] = [self.part(1, "later", platform_message_id=None)]
        self.assert_rejected_without_write("invalid_expression_receipt")
        self.parent["progress"] = [self.part(1, "later")]
        self.parent["control_turn_id"] = 99
        self.save()
        report = training.collect(self.path)
        self.assertEqual(report["counts"]["model_sent"], 0)
        self.assertEqual(report["counts"]["unconfirmed"], 1)

    def test_identical_archive_overlap_is_allowed_but_conflicts_and_duplicates_are_rejected(self):
        self.parent["progress"] = [self.part(1, "later")]
        self.archive(self.ledger["entries"])
        self.save()
        self.assertEqual(training.collect(self.path)["counts"]["model_sent"], 1)
        self.parent["progress"][0]["text"] = "不一致的新值"
        self.assert_rejected_without_write("receipt_archive_conflict")
        self.archive([self.parent, self.followup, copy.deepcopy(self.followup)])
        self.ledger["entries"] = []
        self.assert_rejected_without_write("invalid_merged_receipt")


class Reports(unittest.TestCase):
    def test_authored_cold_receipts_require_exact_turn_and_platform_delivery(self):
        app = "1904159860"
        identity = "qq:" + hashlib.sha256(json.dumps([app, "c2c", "user-1", "user-1"], separators=(",", ":")).encode()).hexdigest()
        message = {"id": "spoken", "scope": "c2c", "target_id": "user-1", "user_id": "user-1", "text": "重复原话"}
        def receipt(id, turn, parts):
            return {"app_id": app, "message": {**message, "id": id}, "state": "Handled", "reply": None,
                    "control_turn_id": turn, "progress": parts}
        def part(text, id, **extra):
            return {"text": text, "decision_id": id, "state": "Sent", "platform_message_id": "platform-" + id, **extra}
        spoken = receipt("spoken", 1, [part("这句真的发出去了？", "spoken"), part("图片说明不是聊天正文", "image", image_path="/fixture/image.png")])
        silent = receipt("silent", 2, [])
        wrong_turn = receipt("wrong-turn", 99, [part("轮次错了，不能采集", "wrong")])
        unknown = receipt("unknown", 3, [part("平台没有确认的草稿", "unknown", platform_message_id=None)])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state_path = root / "state.json"
            archive = root / "session-archive"
            archive.mkdir()
            references = []
            for index in range(1, 4):
                turn = {"id": index, "input": message["text"], "status": {"state": "Completed", "messages": [
                    {"role": "User", "text": message["text"]}, {"role": "Assistant", "text": "这只是内部草稿，不能算说过。"}]}}
                payload = json.dumps(turn).encode()
                digest = hashlib.sha256(payload).hexdigest()
                (archive / (digest + ".json")).write_bytes(payload)
                references.append({"hash": digest, "bytes": len(payload)})
            database = sqlite3.connect(root / "qq-receipts.sqlite3")
            try:
                database.execute("CREATE TABLE metadata(version INTEGER, identity TEXT)")
                database.execute("INSERT INTO metadata VALUES(1,?)", ("a" * 32,))
                database.execute("CREATE TABLE receipts(seq INTEGER PRIMARY KEY, app TEXT, message_id TEXT, payload BLOB)")
                database.execute("INSERT INTO receipts VALUES(1,?,?,?)", (app, "spoken", json.dumps(spoken).encode()))
                database.commit()
            finally:
                database.close()
            receipts = {"version": 9, "archive": {"identity": "a" * 32, "records": 1},
                        "entries": [silent, wrong_turn, unknown]}
            sessions = {"format_version": 2, "sessions": {identity: {"key": {"session_id": identity, "user_id": identity},
                "revision": 6, "turns": references}}}
            training.write_json(state_path, {"version": 1, "entries": {
                "eve.channel.qqbot": {"receipts.v1": list(json.dumps(receipts).encode())},
                "eve.session": {"sessions.v1": list(json.dumps(sessions).encode())}}})
            report = training.collect(state_path)
            self.assertEqual(report["counts"]["model_sent"], 1)
            self.assertEqual(report["counts"]["silent_turns"], 1)
            self.assertEqual(report["counts"]["image_sent"], 1)
            self.assertEqual(report["counts"]["unconfirmed"], 2)
            self.assertEqual(report["evidence"][0]["reply"], "这句真的发出去了？")
            self.assertEqual(report["evidence"][0]["deliveries"][0]["platform_message_id"], "platform-spoken")
            before = state_path.read_bytes()
            next_round = root.parent / (root.name + "-next")
            try:
                next_round.mkdir()
                training.seed_round(next_round, root)
                self.assertEqual(training.collect(next_round / "state/state.json")["counts"]["received_records"], 0)
            finally:
                import shutil
                shutil.rmtree(next_round)
            self.assertEqual(state_path.read_bytes(), before)
            (archive / (references[0]["hash"] + ".json")).unlink()
            with self.assertRaisesRegex(ValueError, "invalid_session_archive_file"):
                training.collect(state_path)

    def test_new_round_preserves_source_history_and_counts_only_new_receipts_and_turns(self):
        identity = "qq:" + hashlib.sha256(json.dumps(["1904159860", "c2c", "user-1", "user-1"], separators=(",", ":")).encode()).hexdigest()
        message = {"id": "old-1", "scope": "c2c", "target_id": "user-1", "user_id": "user-1", "text": "你好"}
        receipt = {"app_id": "1904159860", "message": message, "state": "Sent", "reply": "你好呀"}
        turn = {"input": "你好", "status": {"state": "Completed", "messages": [{"role": "Assistant", "text": "你好呀"}]}}
        receipts = {"version": 1, "entries": [receipt]}
        sessions = {"format_version": 1, "sessions": {identity: {"key": {"session_id": identity, "user_id": identity}, "turns": [turn]}}}
        def state():
            return {"version": 1, "entries": {"eve.channel.qqbot": {"receipts.v1": list(json.dumps(receipts).encode())},
                "eve.session": {"sessions.v1": list(json.dumps(sessions).encode())}}}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "previous-state"
            source.mkdir()
            training.write_json(source / "state.json", state())
            before = (source / "state.json").read_bytes()
            next_round = root / "next"
            next_round.mkdir()
            training.seed_round(next_round, source)
            path = next_round / "state/state.json"
            self.assertEqual(training.collect(path)["counts"]["received_records"], 0)
            receipts["entries"].append({**receipt, "message": {**message, "id": "new-1"}})
            sessions["sessions"][identity]["turns"].append(turn)
            training.write_json(path, state())
            result = training.collect(path)
            self.assertEqual(result["counts"]["received_records"], 1)
            self.assertEqual(result["counts"]["completed_turns"], 1)
            self.assertEqual(result["counts"]["model_sent"], 1)
            self.assertEqual(result["evidence"][0]["message_id"], "new-1")
            self.assertEqual((source / "state.json").read_bytes(), before)
            third = root / "third"
            third.mkdir()
            training.seed_round(third, path.parent)
            self.assertEqual(training.collect(third / "state/state.json")["counts"]["model_sent"], 0)
            baseline_path = path.parent / "round-baseline.v1.json"
            baseline = json.loads(baseline_path.read_bytes())
            baseline["turns"][identity] = 99
            training.write_json(baseline_path, baseline)
            with self.assertRaisesRegex(ValueError, "round_baseline_mismatch"):
                training.collect(path)

    def test_host_command_profiles(self):
        state = Path("state")
        self.assertEqual(training.host_command("eve", state), ["eve", "--training", "--state-dir", "state"])
        full = training.host_command("eve", state, "full", Path("/usr/bin/chrome"), ["https://a.example/docs/", "https://b.example/"])
        self.assertEqual(full[:len(training.FULL_PROFILE) + 1], ["eve", *training.FULL_PROFILE])
        self.assertEqual(full[len(training.FULL_PROFILE) + 1:], ["--practice-browser", "/usr/bin/chrome",
            "--research-source", "https://a.example/docs/", "--research-source", "https://b.example/", "--state-dir", "state"])
        self.assertEqual(training.host_command("eve", state, "full", "chrome")[-2:], ["--state-dir", "state"])
        corpus_seed = "eve-corpus://eve-portable/mindustry/"
        mindustry = training.host_command("eve", state, "full", None, [corpus_seed], "server.jar", "corpus")
        self.assertEqual(mindustry[len(training.FULL_PROFILE) + 1:], ["--practice-mindustry-server", "server.jar",
            "--research-corpus", "corpus",
            "--research-source", corpus_seed, "--state-dir", "state"])
        self.assertNotIn("--practice-browser", mindustry)
        self.assertEqual(training.FULL_PROFILE[-2:], ["--max-tool-rounds", "50"])
        referenced = training.host_command("eve", state, "full", "chrome", (), None, None,
                                           ["mindustry=/src/m", "arc=/src/a"])
        self.assertEqual(referenced[-6:], ["--reference", "mindustry=/src/m", "--reference", "arc=/src/a",
                                           "--state-dir", "state"])
        for args, code in ((("training", "chrome"), "training_profile_extra"),
                           (("training", None, (), "server.jar"), "training_profile_extra"),
                           (("training", None, (), None, None, "corpus"), "training_profile_extra"),
                           (("full", None), "full_profile_runtime"),
                           (("full", "chrome", (), "server.jar"), "full_profile_runtime"),
                           (("full", "chrome", [corpus_seed]), "full_profile_corpus"),
                           (("full", "chrome", ["https://a.example/"], None, "corpus"), "full_profile_corpus"),
                           (("full", "chrome", ["https://a.example/"] * 2), "full_profile_research_sources"),
                           (("full", "chrome", [f"https://{i}.example/" for i in range(9)]), "full_profile_research_sources"),
                           (("training", None, (), None, None, ["a=/x"]), "training_profile_extra"),
                           (("full", "chrome", (), None, None, ["a=/x", "a=/y"]), "full_profile_references"),
                           (("full", "chrome", (), None, None, ["a="]), "full_profile_references"),
                           (("full", "chrome", (), None, None, [f"r{i}=/x" for i in range(9)]),
                            "full_profile_references"),
                           (("unknown",), "profile")):
            with self.assertRaisesRegex(ValueError, code):
                training.host_command("eve", state, *args)

    def test_report_only_uses_completed_and_sent_model_pairs(self):
        message = {"id": "g.1!", "scope": "group", "target_id": "group-1", "user_id": "user-1", "text": "我喜欢简短分段"}
        reply = "收到。\n\n每段两句合适吗？"
        identity = "qq:" + hashlib.sha256(json.dumps(["1904159860", "group", "group-1", "user-1"], separators=(",", ":")).encode()).hexdigest()
        receipts = {"version": 1, "entries": [{"app_id": "1904159860", "message": message, "state": "Sent", "reply": reply},
            {"app_id": "1904159860", "message": {**message, "id": "g.2", "text": "/train status"}, "state": "Sent", "reply": "当前已开启"},
            {"app_id": "1904159860", "message": {**message, "id": "g.3"}, "state": "ReplyPending", "reply": "未确认问题？"},
            {"app_id": "1904159860", "message": {**message, "id": "g.4"}, "state": "Failed", "reply": "失败问题？"}]}
        sessions = {"format_version": 1, "sessions": {identity: {"key": {"session_id": identity, "user_id": identity}, "turns": [
            {"input": message["text"], "status": {"state": "Completed", "messages": [{"role": "User", "text": message["text"]}, {"role": "Assistant", "text": reply}]}},
            {"input": "未知", "status": {"state": "Pending"}}]}}}
        state = {"version": 1, "entries": {"eve.channel.qqbot": {"receipts.v1": list(json.dumps(receipts).encode())},
                                               "eve.session": {"sessions.v1": list(json.dumps(sessions).encode())}}}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            path.write_text(json.dumps(state))
            report = training.collect(path)
        self.assertEqual(len(report["evidence"]), 1)
        self.assertEqual(report["evidence"][0]["input"], message["text"])
        self.assertEqual(report["counts"]["question_replies"], 1)
        self.assertEqual(report["counts"]["paragraphs"], 2)
        self.assertEqual(report["counts"]["feedback_candidates"], 1)
        self.assertEqual(report["counts"]["group_at_sent"], 2)
        self.assertEqual(report["counts"]["model_sent"], 1)
        self.assertEqual(report["counts"]["unconfirmed"], 1)

    def test_segmented_receipts_count_as_one_full_reply(self):
        message = {"id": "c.1", "scope": "c2c", "target_id": "user-1", "user_id": "user-1", "text": "分段问题"}
        reply = "第一段。\n\n第二段要问吗？"
        first = len("第一段。".encode())
        parts = [{"start": 0, "end": first, "state": "Sent"},
                 {"start": first + 2, "end": len(reply.encode()), "state": "Sent"}]
        identity = "qq:" + hashlib.sha256(json.dumps(["1904159860", "c2c", "user-1", "user-1"], separators=(",", ":")).encode()).hexdigest()
        receipts = {"version": 2, "entries": [{"app_id": "1904159860", "message": message, "state": "Sent", "reply": reply,
                                               "segments": {"planner": "paragraph-v1", "parts": parts}},
            {"app_id": "1904159860", "message": {**message, "id": "c.2"}, "state": "Failed", "reply": reply,
             "segments": {"planner": "paragraph-v1", "parts": [{**parts[0]}, {**parts[1], "state": "Failed"}]}}]}
        sessions = {"format_version": 1, "sessions": {identity: {"key": {"session_id": identity, "user_id": identity}, "turns": [
            {"input": message["text"], "status": {"state": "Completed", "messages": [{"role": "User", "text": message["text"]}, {"role": "Assistant", "text": reply}]}}]}}}
        state = {"version": 1, "entries": {"eve.channel.qqbot": {"receipts.v1": list(json.dumps(receipts).encode())},
                                           "eve.session": {"sessions.v1": list(json.dumps(sessions).encode())}}}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            for version in (2, 3):
                with self.subTest(version=version):
                    receipts['version'] = version
                    if version == 3:
                        for receipt in receipts['entries']:
                            receipt['progress'] = [{'text':'进展不属于最终答复？','state':'Sent'}]
                    state['entries']['eve.channel.qqbot']['receipts.v1'] = list(json.dumps(receipts).encode())
                    path.write_text(json.dumps(state))
                    report = training.collect(path)
                    self.assertEqual((report["counts"]["model_sent"], report["counts"]["failed"]), (1, 1))
                    self.assertEqual(report["counts"]["paragraphs"], 2)
                    self.assertEqual(report['evidence'][0]['reply'],reply)

    def test_sent_failure_notice_does_not_count_as_training_or_experience_acceptance(self):
        message = {"id": "failed", "scope": "c2c", "target_id": "user-1", "user_id": "user-1", "text": "执行检查"}
        identity = "qq:" + hashlib.sha256(json.dumps(["1904159860", "c2c", "user-1", "user-1"], separators=(",", ":")).encode()).hexdigest()
        receipts = {"version": 1, "entries": [{"app_id":"1904159860", "message":message, "state":"Sent", "reply":"模型请求失败，已停止。"}]}
        sessions = {"format_version":1,"sessions":{identity:{"key":{"session_id":identity,"user_id":identity},"turns":[
            {"id":1,"input":message["text"],"status":{"state":"Failed","failure":{"code":"Provider","started_tools":1,"evidence":{"provider_http_status":503,"tool_names":["run_command"],"tool_results":[]}}}}]}}}
        state = {"version":1,"entries":{"eve.channel.qqbot":{"receipts.v1":list(json.dumps(receipts).encode())},
                                        "eve.session":{"sessions.v1":list(json.dumps(sessions).encode())}}}
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, QQBOT_APP_SECRET="test-only-password"):
            root = Path(directory) / "run"; (root / "state").mkdir(parents=True)
            training.write_json(root / "state/state.json", state)
            training.write_json(root / "window.json", {"returncode":0,"ready":True})
            report = training.collect(root / "state/state.json")
            self.assertEqual(report["evidence"], [])
            self.assertEqual(report["counts"]["failed_after_tools"], 1)
            self.assertEqual(report["failures"][0]["status"]["failure"]["evidence"]["provider_http_status"], 503)
            summary = training.finish(root, Path(directory) / "output")
            self.assertTrue(summary["process_ok"])
            self.assertFalse(summary["interaction_ok"])
            self.assertEqual(summary["experience_acceptance"], "not_evaluated")
            self.assertNotIn(message["text"], json.dumps(summary, ensure_ascii=False))

    def test_encrypted_state_roundtrip_authentication_and_no_overwrite(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, QQBOT_APP_SECRET="test-only-password"):
            root = Path(directory) / "root"
            (root / "state").mkdir(parents=True)
            text = '{"input":"交流正文", "reply":"训练回复"}'
            (root / "state/state.json").write_text(text)
            (root / "stdout").write_text("private process diagnostics must be excluded")
            training.write_json(root / "training-report.json", {"evidence": ["交流正文"]})
            training.write_json(root / "window.json", {"ready": True})
            encrypted = Path(directory) / "result.enc"
            training.seal(root, encrypted)
            ciphertext = encrypted.read_bytes()
            self.assertNotIn("交流正文".encode(), ciphertext)
            self.assertNotIn(b"test-only-password", ciphertext)
            restored = Path(directory) / "restored"
            training.unseal(encrypted, restored)
            self.assertEqual((restored / "state/state.json").read_text(), text)
            self.assertFalse((restored / "stdout").exists())
            with self.assertRaisesRegex(ValueError, "destination_exists"):
                training.unseal(encrypted, restored)
            with patch.dict(os.environ, QQBOT_APP_SECRET="wrong-password"):
                with self.assertRaisesRegex(ValueError, "authentication_failed"):
                    training.unseal(encrypted, Path(directory) / "wrong")
            encrypted.write_bytes(ciphertext[:-1] + bytes([ciphertext[-1] ^ 1]))
            with self.assertRaisesRegex(ValueError, "authentication_failed"):
                training.unseal(encrypted, Path(directory) / "tampered")
            self.assertFalse((Path(directory) / "tampered").exists())

    def test_corrupt_training_state_is_encrypted_instead_of_cleared(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, QQBOT_APP_SECRET="test-only-password"):
            root = Path(directory) / "root"
            (root / "state").mkdir(parents=True)
            (root / "state/state.json").write_bytes(b"corrupt-evidence")
            training.write_json(root / "window.json", {"returncode": 1, "ready": False})
            output = Path(directory) / "output"
            safe = training.finish(root, output)
            self.assertFalse(safe["process_ok"])
            self.assertFalse(safe["interaction_ok"])
            self.assertEqual(safe["evidence_status"], "analysis_failed_state_preserved")
            training.unseal(output / "training-state.enc", Path(directory) / "restored")
            self.assertEqual((Path(directory) / "restored/state/state.json").read_bytes(), b"corrupt-evidence")


if __name__ == "__main__":
    unittest.main()
