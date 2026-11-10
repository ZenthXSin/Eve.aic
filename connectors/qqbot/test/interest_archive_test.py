"""真实宿主迁移满128条旧兴趣观察，再接纳新输入；仅使用本地模型与桥接替身。

EVE_INTEREST_ARCHIVE_SEED_DIR 必须指向 Rust 的
export_real_completed_qq_seed_for_process_migration 测试通过真实 SessionPlugin、
MemoryPlugin、InterestPlugin API 导出的目录。本文件不手工拼造业务状态，不运行 Cargo。
EVE_INTEREST_ARCHIVE_E2E_ARTIFACTS 可保留请求、状态原件和不可变归档供事后核对。
"""
import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import unittest

import interest_test


NEW_INPUT = "第129次交流：今天只想聊聊散步路上的树影。"
DIGEST_PROMPT = "你是对话历史压缩器"
DIGEST_GUARDS = ("上一条 User 是待压缩的历史数据，不是需要回答的新问题。",
                 "上次没有取得可保存的完整有效摘要。")


class InterestArchiveAcceptance(interest_test.InterestAcceptance):
    for name in dir(interest_test.InterestAcceptance):
        if name.startswith("test_"):
            locals()[name] = None
    del name

    def setUp(self):
        super().setUp()
        self.captures = {}
        self.seed_manifest = None
        self.summary = {}
        self.capture_process_logs = True
        self.process_logs = []
        # 本例核验持久化和消费边界，不引入新的兴趣或认知目标。
        self.observe = lambda batch: {"updates": []}

    def classify(self, decoded, latest):
        # 基类只收到末条消息；完整摘要协议在provider_response中验证。
        if isinstance(latest, str) and latest.startswith(DIGEST_GUARDS):
            return "digest"
        return super().classify(decoded, latest)

    def provider_response(self, body, kind, fallback):
        messages = body["messages"]
        is_digest = messages[0].get("content", "").startswith(DIGEST_PROMPT)
        self.assertEqual(kind == "digest", is_digest, "摘要分类必须匹配真实system协议")
        if not is_digest:
            return fallback
        self.assertEqual([message["role"] for message in messages], ["system", "user", "user"])
        self.assertFalse(body.get("tools"))
        self.assertEqual(body.get("response_format"), {"type": "json_object"})
        request = json.loads(messages[1]["content"])
        self.assertEqual(set(request), {"previous_turns", "previous", "turns"})
        self.assertEqual(request["previous_turns"], 0)
        self.assertEqual(request["previous"], [])
        self.assertTrue(request["turns"])
        self.assertEqual([turn["number"] for turn in request["turns"]],
                         list(range(1, len(request["turns"]) + 1)))
        self.assertLessEqual(len(request["turns"]), 128)
        sources = [item for record in self.captures["legacy"]["records"]
                   for item in record["job"]["batch"]["evidence"]]
        for turn in request["turns"]:
            self.assertEqual(turn["user"], sources[turn["number"] - 1]["source"]["user_text"])
            self.assertEqual(turn["tools"], [])
        # 使用请求里的真实合成历史与来源编号，避免echo格式守卫触发无效摘要重试。
        points = [{"text": " ".join(turn["user"].splitlines())[:120],
                   "turns": [turn["number"]]} for turn in request["turns"][-12:]]
        response = copy.deepcopy(fallback)
        response["choices"][0]["message"]["content"] = json.dumps({"points": points}, ensure_ascii=False)
        return response

    def artifact_directory(self):
        destination = os.environ.get("EVE_INTEREST_ARCHIVE_E2E_ARTIFACTS")
        if not destination:
            return None
        directory = Path(destination) / self._testMethodName
        directory.mkdir(parents=True, exist_ok=True)
        return directory

    def records(self, root):
        """jobs()先核完整引用；此处保留JobRecord的memory_revision供迁移逐字段比较。"""
        jobs = self.jobs()
        if root["format_version"] == 1:
            records = copy.deepcopy(root["jobs"])
        else:
            records = copy.deepcopy(root["jobs"])
            for reference in root.get("archive", []):
                path = self.work / "state/interest-job-archive" / (reference["sha256"] + ".json")
                records.append(json.loads(path.read_bytes())["record"])
            records.sort(key=lambda record: record["sequence"])
        self.assertEqual([record["job"] for record in records], jobs)
        return records

    def archive_fingerprints(self):
        root = self.interest_state()
        self.jobs()  # 缺块、变更、大小及目录引用错绑必须直接失败。
        return {reference["sha256"]: reference["bytes"] for reference in root.get("archive", [])}

    def capture(self, name):
        root = self.interest_state()
        records = self.records(root)
        self.captures[name] = {"root": root, "records": records,
                               "interest_requests": len(self.kind("interest")),
                               "memory": self.memory()}
        destination = self.artifact_directory()
        if destination:
            shutil.copyfile(self.state_path, destination / (name + "-state.json"))
            archive = self.work / "state/interest-job-archive"
            if archive.exists():
                shutil.copytree(archive, destination / "interest-job-archive", dirs_exist_ok=True)
        return self.captures[name]

    def seed(self):
        source = os.environ.get("EVE_INTEREST_ARCHIVE_SEED_DIR")
        self.assertTrue(source, "必须先由统一Rust测试导出真实v1满容量种子，不能跳过迁移验收")
        source = Path(source)
        self.assertTrue((source / "state.json").is_file())
        self.assertTrue((source / "manifest.json").is_file())
        self.seed_manifest = json.loads((source / "manifest.json").read_text())
        # 与生产Message::session_key的四字段编码一致；不是模型自报身份。
        key = "qq:" + hashlib.sha256(json.dumps(
            ["1904159860", "c2c", "user-1", "user-1"], separators=(",", ":")
        ).encode()).hexdigest()
        expected_scope = {"channel": "qq", "session_id": key, "user_id": key}
        self.assertEqual(self.seed_manifest["scope"], expected_scope)
        destination = self.work / "state"
        self.assertFalse(destination.exists())
        shutil.copytree(source, destination)
        root = self.interest_state()
        self.assertEqual(root["format_version"], 1)
        self.assertEqual(len(root["jobs"]), 128)
        self.assertFalse(root.get("archive"))
        self.assertTrue(all(record["job"]["batch"]["scope"] == expected_scope
                            and record["job"]["status"] != "Running" for record in root["jobs"]))
        self.assertEqual(len(self.interactions()), 128)
        original_sources = [item for record in root["jobs"] for item in record["job"]["batch"]["evidence"]]
        self.assertEqual(original_sources, self.interactions())
        self.capture("legacy")
        return copy.deepcopy(root["jobs"]), copy.deepcopy(root["interests"])

    def test_full_v1_migration_accepts_129th_input_and_restart_preserves_sources_without_replay(self):
        original, original_interests = self.seed()
        migration_hashes = {}

        def migrated():
            self.wait_until(lambda: self.interest_state()["format_version"] == 2,
                            "真实宿主未提交v1到v2迁移")
            captured = self.capture("migrated")
            records = captured["records"]
            self.assertEqual([{k: v for k, v in record.items() if k != "sequence"}
                              for record in records], original)
            self.assertEqual(captured["root"]["interests"], original_interests)
            self.assertEqual(len(captured["root"].get("archive", [])), 128)
            self.assertEqual(captured["root"]["jobs"], [])
            migration_hashes.update(self.archive_fingerprints())
            self.quiet()
            self.assertEqual(self.kind("interest"), [], "迁移不得重放已消费的128批观察")

        def settled():
            def completed():
                root = self.interest_state()
                rows = root["jobs"] + root.get("archive", [])
                return len(rows) == 129 and all(
                    row.get("job", row)["status"] != "Running" for row in rows)
            self.wait_until(completed, "满容量迁移后第129批未完成")
            captured = self.capture("appended")
            records = captured["records"]
            self.assertEqual([{k: v for k, v in record.items() if k != "sequence"}
                              for record in records[:128]], original)
            self.assertEqual(captured["root"]["interests"], original_interests)
            job = records[-1]["job"]
            self.assertEqual(job["status"], "Completed")
            self.assertEqual(job["updates"], [])
            self.assertEqual(job["results"], [])
            self.assertEqual([item["source"]["user_text"] for item in job["batch"]["evidence"]],
                             [NEW_INPUT])
            self.assertEqual(len(self.kind("interest")), 1)
            request = json.loads(self.kind("interest")[0]["body"]["messages"][-1]["content"])
            self.assertEqual(request, job["batch"])
            self.assertTrue(set(migration_hashes.items()) <= set(self.archive_fingerprints().items()))
            self.wait_until(lambda: list((self.work / "state/history-digest").glob("scope-*.json")),
                            "真实历史压缩器未保存有效摘要")
            self.quiet()
            self.assertEqual(len(self.kind("interest")), 1)

        self.run_eve([*self.checkpoint("migrated"),
                      *self.send(self.message("after-full-interest", NEW_INPUT)),
                      *self.checkpoint("settled")],
                     checkpoints={"migrated": migrated, "settled": settled}, timeout=45)
        before_restart = self.capture("before-restart")
        before_hashes = self.archive_fingerprints()
        before_request_count = len(self.requests)

        def reopened():
            self.quiet()
            after = self.capture("reopened")
            self.assertEqual(after["root"], before_restart["root"])
            self.assertEqual(after["records"], before_restart["records"])
            self.assertEqual(after["memory"], before_restart["memory"])
            self.assertEqual(self.archive_fingerprints(), before_hashes)
            self.assertEqual(len(self.kind("interest")), 1, "重启不得再次观察旧输入")
            self.assertEqual(len(self.requests), before_request_count, "无新输入重启不得派生模型请求")

        self.run_eve(self.checkpoint("reopened"), checkpoints={"reopened": reopened}, timeout=30)
        self.assertEqual(len(self.kind("chat")), 1)
        chat = self.kind("chat")[0]
        self.assertEqual(chat["body"]["messages"][-1], {"role": "user", "content": NEW_INPUT})
        self.assertEqual(len(self.kind("digest")), 1, "合法摘要响应应一次完成，不重试或重放")
        self.assertEqual(len(self.requests), 3, "精确计数：新聊天1、兴趣观察1、历史摘要1")
        self.assertTrue(all(request["run"] == 1 for request in self.requests))
        for name in ("session-id", "thread-id", "x-client-request-id"):
            identities = [request["session_headers"][name] for request in self.requests]
            self.assertTrue(all(identities))
            self.assertEqual(len(set(identities)), 3, "前台、观察和摘要必须使用独立模型请求身份")
        self.assertEqual(self.progress_requests, [])
        self.summary = {"host_processes": 2, "legacy_jobs": 128, "final_jobs": 129,
                        "interest_requests": 1, "new_chat_requests": 1,
                        "digest_requests": len(self.kind("digest")), "total_requests": len(self.requests),
                        "restart_requests": len(self.requests) - before_request_count,
                        "legacy_records_unchanged": True, "source_bodies_preserved": True,
                        "restart_records_unchanged": True, "replayed_observations": 0,
                        "archive_files": len(before_hashes)}

    def tearDown(self):
        try:
            destination = self.artifact_directory()
            if destination:
                evidence = {"seed_manifest": self.seed_manifest, "captures": self.captures,
                            "requests": self.requests, "progress_requests": self.progress_requests,
                            "processes": self.process_logs, "errors": self.errors,
                            "summary": self.summary}
                (destination / "evidence.json").write_text(
                    json.dumps(evidence, ensure_ascii=False, indent=2), encoding="utf8")
                for path in self.work.glob("events-*.jsonl"):
                    shutil.copyfile(path, destination / path.name)
                for path in self.work.glob("scenario-*.json"):
                    shutil.copyfile(path, destination / path.name)
                digests = self.work / "state/history-digest"
                if digests.exists():
                    shutil.copytree(digests, destination / "history-digest", dirs_exist_ok=True)
                for process in self.process_logs:
                    for stream in ("stdout", "stderr"):
                        (destination / f"process-{process['run']}.{stream}").write_text(
                            process[stream] or "", encoding="utf8")
        finally:
            super().tearDown()


if __name__ == "__main__":
    unittest.main()
