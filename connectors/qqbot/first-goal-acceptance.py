"""显式运行真实模型、真实实践运行器的内部交互验收；QQ 只使用本地协议替身。

只接受新输出目录；原始模型文本可选保存在本地，stdout 只输出阶段与结论。
不使用正式 QQ 密钥，不启动 QQ SDK，不重放先前的消息或未知副作用。
"""
import argparse
from collections import Counter
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import signal
import secrets
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
BRIDGE = Path(__file__).with_name("test") / "fake-bridge.mjs"
APP = "1904159860"
USER = "internal-owner"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def purpose(body):
    text = body["messages"][-1].get("content") or ""
    try:
        value = json.loads(text)
    except (ValueError, TypeError):
        value = {}
    if isinstance(value, dict):
        for key in ("observer_version", "selector_version", "extractor_version", "drafter_version",
                    "distillation_id", "selection_id", "invitation_id", "judge_version"):
            if key in value:
                return key
    return "dialogue_or_reflection"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


class Trial:
    def __init__(self, args, key):
        self.args, self.key = args, key
        self.local_token = secrets.token_urlsafe(32)
        self.root = args.output.resolve()
        self.root.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.lock = threading.Lock()
        self.records = []
        self.phase = 0
        self.child = None
        self.handles = []
        self.report = {"transport": "local_protocol_fixture", "official_qq_tested": False,
                       "binary_sha256": digest(args.binary), "model": args.model,
                       "runtime_program_or_jar_sha256": digest(args.mindustry_server or args.browser)}
        try:
            self.report["source_commit"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, stderr=subprocess.DEVNULL, text=True).strip()
            self.report["source_diff_sha256"] = hashlib.sha256(subprocess.check_output(["git", "diff", "HEAD", "--"], cwd=ROOT)).hexdigest()
            self.report["source_checkout_dirty"] = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT).strip())
        except subprocess.CalledProcessError:
            self.report["source_commit"] = None
        self.opener = urllib.request.build_opener(NoRedirect())
        trial = self

        class Proxy(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                if self.headers.get("Authorization") != "Bearer " + trial.local_token:
                    self.send_error(401)
                    return
                size = int(self.headers.get("Content-Length", "0"))
                if self.path != "/v1/chat/completions" or not 0 < size <= 2 * 1024 * 1024:
                    self.send_error(400)
                    return
                body = self.rfile.read(size)
                value = json.loads(body)
                started = time.monotonic()
                with trial.lock:
                    record = {"index": len(trial.records) + 1, "phase": trial.phase,
                              "purpose": purpose(value)}
                    if trial.args.record_text:
                        record["request"] = json.loads(json.dumps(value).replace(trial.key, "[redacted]"))
                    trial.records.append(record)
                status, media = 502, "application/json"
                data = b'{"error":{"message":"acceptance_upstream_failed"}}'
                try:
                    if record["index"] > trial.args.max_requests:
                        raise ValueError("request_limit")
                    request = urllib.request.Request(trial.args.endpoint, data=body,
                        headers={"Content-Type": "application/json", "Authorization": "Bearer " + trial.key})
                    with trial.opener.open(request, timeout=90) as response:
                        status = response.status
                        media = response.headers.get("Content-Type", "application/json")
                        data = response.read(2 * 1024 * 1024 + 1)
                    if len(data) > 2 * 1024 * 1024:
                        raise ValueError("response_limit")
                    # 原样传输，不补字段、不改模型结果，不把 SSE 或不完整输出伪装为成功 JSON。
                    if trial.args.record_text:
                        try:
                            record["response"] = json.loads(data.decode().replace(trial.key, "[redacted]"))
                        except ValueError:
                            record["response_sha256"] = hashlib.sha256(data).hexdigest()
                except (ValueError, OSError) as error:
                    status = error.code if isinstance(error, urllib.error.HTTPError) else 502
                    record["error"] = "http_" + str(status) if isinstance(error, urllib.error.HTTPError) else type(error).__name__
                    data = b'{"error":{"message":"acceptance_upstream_failed"}}'
                    media = "application/json"
                finally:
                    with trial.lock:
                        record.update(status=status, duration_ms=round((time.monotonic() - started) * 1000))
                        trial.save("requests.json", trial.records)
                try:
                    self.send_response(status)
                    self.send_header("Content-Type", media)
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        source = self.root / "source"
        source.mkdir()
        (source / "README.md").write_text("Independent internal acceptance workspace.\n", encoding="utf8")
        scope = "qq:" + hashlib.sha256(json.dumps([APP, "c2c", USER, USER], separators=(",", ":")).encode()).hexdigest()
        self.save("developer.json", {"workspace": str(source), "storage_directory": str(self.root / "developer-state"),
            "operators": [scope], "commands": False, "github": False, "max_tool_rounds": 8})

    def save(self, name, value):
        target = self.root / name
        temporary = target.with_suffix(target.suffix + ".tmp")
        temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf8")
        temporary.replace(target)

    def state(self):
        path = self.root / "state/state.json"
        if not path.exists():
            return {}
        return {plugin: {key: json.loads(bytes(raw)) for key, raw in rows.items()}
                for plugin, rows in json.loads(path.read_bytes())["entries"].items()}

    def rows(self, plugin, key, field):
        return self.state().get(plugin, {}).get(key, {}).get(field, [])

    def wait(self, predicate, label):
        self.report["last_stage"] = label
        print(json.dumps({"phase": self.phase, "waiting": label}), flush=True)
        deadline = time.monotonic() + self.args.stage_timeout
        while not predicate():
            if self.child.poll() is not None:
                raise RuntimeError("child_exited_" + label)
            if time.monotonic() >= deadline:
                raise RuntimeError("stage_timeout_" + label)
            if label == "user_message_delivered":
                receipts = self.rows("eve.channel.qqbot", "receipts.v1", "entries")
                if any(x["message"]["id"].startswith(f"phase-{self.phase}-") and x["state"] == "Failed" for x in receipts):
                    raise RuntimeError("user_message_failed")
            time.sleep(0.2)

    def boot(self, messages):
        self.phase += 1
        gate = self.root / f"continue-{self.phase}"
        checkpoint = self.root / f"checkpoint-{self.phase}"
        script = []
        for index, text in enumerate(messages):
            message_id = f"phase-{self.phase}-message-{index + 1}"
            script += [{"send": {"id": message_id, "scope": "c2c", "user_id": USER, "target_id": USER,
                                  "text": text, "accept_any": True}},
                       {"wait_receipt": {"path": str(self.root / "state/state.json"), "id": message_id, "state": "Sent"}}]
        script += [{"touch": str(checkpoint)}, {"wait_file": str(gate)}]
        self.save(f"scenario-{self.phase}.json", {"script": script,
            "wait_timeout_ms": (self.args.stage_timeout + 30) * 1000,
            "events_file": str(self.root / f"events-{self.phase}.jsonl"),
            "error_file": str(self.root / f"bridge-error-{self.phase}")})
        env = {k: v for k, v in os.environ.items() if not k.startswith(("EVE_", "QQBOT_"))}
        env.update(QQBOT_APP_ID=APP, QQBOT_APP_SECRET="internal-protocol-fixture",
            EVE_OPENAI_API_KEY=self.local_token, EVE_OPENAI_MODEL=self.args.model,
            EVE_OPENAI_BASE_URL=f"http://127.0.0.1:{self.server.server_port}", EVE_OPENAI_PROTOCOL="chat",
            EVE_OPENAI_MAX_OUTPUT_TOKENS="8192", EVE_OPENAI_TIMEOUT_SECONDS="90", EVE_LLM_RESPONSE_MODE="complete",
            EVE_DEVELOPER_CONFIG=str(self.root / "developer.json"))
        command = [str(self.args.binary), "--state-dir", str(self.root / "state"), "--agent", str(self.args.agent),
            "--bridge-script", str(BRIDGE), "--bridge-arg", str(self.root / f"scenario-{self.phase}.json"),
            "--interest-learning", "--interest-cooldown-ms", "0", "--cognition-max-executions", "8",
            "--memory-recall", "--segmented", "--skill-learning", "--tool-forging", "--agenda-context", "--clarify",
            "--outreach", "--outreach-cooldown-ms", "1000", "--outreach-proactive-after-ms", "5000",
            "--autonomy-budget", str(self.args.max_requests)]
        for source in self.args.source:
            command += ["--research-source", source]
        if self.args.mindustry_server:
            command += ["--practice-mindustry-server", str(self.args.mindustry_server)]
        else:
            command += ["--practice-browser", str(self.args.browser)]
        self.handles = [(self.root / f"stdout-{self.phase}").open("wb"), (self.root / f"stderr-{self.phase}").open("wb")]
        self.child = subprocess.Popen(command, env=env, cwd=ROOT, stdin=subprocess.DEVNULL,
                                      stdout=self.handles[0], stderr=self.handles[1])
        self.save("active.json", {"pid": self.child.pid, "phase": self.phase, "command": command})
        self.wait(checkpoint.exists, "user_message_delivered")
        return gate

    def stop(self, gate):
        gate.touch()
        self.child.wait(timeout=60)
        for handle in self.handles:
            handle.close()
        if self.child.returncode:
            raise RuntimeError("child_exit_nonzero")

    def learned(self):
        quoted = self.rows("eve.knowledge", "knowledge.v1", "entries")
        skills = self.rows("eve.skill", "skill.v1", "skills")
        invitations = self.rows("eve.outreach", "outreach.v1", "invitations")
        return (any(x["status"] == "SourceQuoted" for x in quoted) and any(x["enabled"] for x in skills)
                and any(x["status"] == "Delivered" for x in invitations))

    def reused(self, old_calls, old_selections):
        skills = self.state().get("eve.skill", {}).get("skill.v1", {})
        accepted = [x["arguments"] for x in skills.get("tool_calls", []) if x["id"] not in old_calls and x["outcome"] == "Verified"]
        accepted += [x["choice"]["arguments"] for x in skills.get("selections", [])
                     if x["id"] not in old_selections and x["outcome"] == "Verified" and x["choice"]]
        return any(all(value in values.values() for value in self.args.expect_argument) for values in accepted)

    def run(self):
        gate = self.boot([self.args.opening])
        self.wait(self.learned, "independent_learning_skill_and_invitation")
        state = self.state()
        documents = {x["id"]: x for x in state["eve.knowledge"]["knowledge.v1"]["documents"]}
        quoted = [x for x in state["eve.knowledge"]["knowledge.v1"]["entries"] if x["status"] == "SourceQuoted"]
        assert all(x["source"]["quote"] in documents[x["source"]["document_id"]]["text"] for x in quoted)
        practices = state["eve.practice"]["practice.v1"]["runs"]
        verified = [x for x in practices if x["status"] == "Verified"]
        assert verified and all(x["attempts"][-1]["evidence"]["loaded"] and not x["attempts"][-1]["evidence"]["warnings"]
                                and all(p["passed"] for p in x["attempts"][-1]["evidence"]["probes"]) for x in verified)
        assert any(x["attempts"][-1]["draft"]["notes_used"] for x in verified)
        skills = state["eve.skill"]["skill.v1"]
        old_calls = {x["id"] for x in skills["tool_calls"]}
        old_selections = {x["id"] for x in skills["selections"]}
        self.report.update(quoted_entries=len(quoted), original_practice_verified=True,
                           skill_enabled=True, invitation_delivered=True)
        self.stop(gate)
        self.save("progress.json", self.report)
        gate = self.boot([self.args.follow_up])
        self.wait(lambda: self.reused(old_calls, old_selections), "independent_skill_reuse")
        self.wait(lambda: sum(x["status"] == "Delivered" for x in self.rows("eve.outreach", "outreach.v1", "invitations")) >= 2,
                  "follow_up_delivered")
        self.stop(gate)
        self.report.update(independent_skill_reuse_verified=True, follow_up_delivered=True)
        before = self.state()
        gate = self.boot([])
        time.sleep(3)
        after = self.state()
        assert all(after[k] == before[k] for k in ["eve.knowledge", "eve.practice", "eve.skill", "eve.outreach"])
        self.stop(gate)
        self.report["restart_without_operation_replay"] = True
        gate = self.boot([self.args.withdrawal])
        self.wait(lambda: bool(self.rows("eve.interest", "interests.v1", "interests"))
                  and all(x["status"] == "Withdrawn" for x in self.rows("eve.interest", "interests.v1", "interests")), "withdrawal")
        self.stop(gate)
        self.report.update(withdrawal_observed=True, passed=True)

    def close(self):
        if self.child and self.child.poll() is None:
            self.child.send_signal(signal.SIGINT)
            try:
                self.child.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.child.kill()
                self.child.wait(timeout=10)
        for handle in self.handles:
            handle.close()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        self.report.update(requests=len(self.records), purposes=dict(Counter(x["purpose"] for x in self.records)))
        self.save("result.json", self.report)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--real-model", action="store_true", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--source", action="append", required=True)
    parser.add_argument("--opening", required=True)
    parser.add_argument("--follow-up", required=True)
    parser.add_argument("--withdrawal", required=True)
    parser.add_argument("--expect-argument", action="append", default=[], help="后续技能调用必须实际采用的参数值，可重复")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/eve-qqbot")
    parser.add_argument("--agent", type=Path, default=ROOT / "AGENT.md")
    runtime = parser.add_mutually_exclusive_group(required=True)
    runtime.add_argument("--mindustry-server", type=Path)
    runtime.add_argument("--browser", type=Path)
    parser.add_argument("--record-text", action="store_true")
    parser.add_argument("--max-requests", type=int, default=100)
    parser.add_argument("--stage-timeout", type=int, default=360)
    args = parser.parse_args()
    url = urllib.parse.urlsplit(args.endpoint)
    if url.scheme != "https" or not url.hostname or url.username or url.password or url.query or url.fragment:
        parser.error("endpoint 必须是没有用户信息、查询或片段的完整 HTTPS Chat Completions 地址")
    if not 1 <= args.max_requests <= 200 or not 30 <= args.stage_timeout <= 600:
        parser.error("请求上限须为1至200，阶段期限须为30至600秒")
    key = os.environ.get("EVE_OPENAI_API_KEY", "")
    if not key.strip():
        parser.error("须在私有环境中设置 EVE_OPENAI_API_KEY")
    os.umask(0o077)
    trial = Trial(args, key)
    try:
        trial.run()
    except (AssertionError, RuntimeError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        # 不输出远端错误正文、用户正文、路径或凭据；原始状态保留以供私下核对。
        trial.report.update(passed=False, error_code=type(error).__name__)
    finally:
        trial.close()
    print(json.dumps(trial.report, ensure_ascii=False))
    return 0 if trial.report.get("passed") else 1


if __name__ == "__main__":
    raise SystemExit(main())
