"""有界 QQ 训练窗口、事实报告与加密状态包；不打印交流正文或密钥。"""
import argparse
import hashlib
import hmac
import io
import json
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
import tarfile
import time

MAGIC = b"EVE-QQ-TRAINING-1\n"
MAX_ARCHIVE = 32 * 1024 * 1024
MAX_RESEARCH_SOURCES = 8
# 全功能体验：已合入 main 的 QQ 能力一次开启；语义召回需要独立 embedding 角色，控制面板只监听本机，均不在此列。
FULL_PROFILE = ["--training", "--cognition", "--memory", "--self-learning", "--memory-recall", "--interest-learning",
                "--skill-learning", "--tool-forging", "--plans", "--outreach", "--outreach-proactive-after-ms", "1800000",
                "--clarify", "--agenda-context", "--reminders", "--background-tasks", "--subagents", "--triggers", "--sleep",
                "--goal-completion", "--learning-stop",
                "--autonomy-budget", "200", "--segmented", "--message-judge", "primary",
                # 查参考资料需要“搜索→读取→作答”多轮工具调用。
                "--max-tool-rounds", "50"]
MAX_REFERENCES = 8
SUMMARY_TITLES = {"training": "QQ 主动提问训练结果", "full": "QQ 全功能体验结果"}
SCOPES = {"training": "主动提问、用户表达统计与交流证据采集；未更新模型权重或已确认长期偏好",
          "full": "全功能体验交流证据采集；计数只覆盖 QQ 回执与会话，后台学习、计划与提醒的记录在加密状态包中"}


def host_command(binary, state_dir, profile="training", browser=None, research_sources=(),
                 mindustry_jar=None, corpus=None, references=()):
    if profile == "training":
        if browser or research_sources or mindustry_jar or corpus or references:
            raise ValueError("training_profile_extra")
        extra = ["--training"]
    elif profile == "full":
        # 实践运行环境二选一：Mindustry 无头服务端或无头浏览器。
        if bool(browser) == bool(mindustry_jar):
            raise ValueError("full_profile_runtime")
        if len(research_sources) > MAX_RESEARCH_SOURCES or len(set(research_sources)) != len(research_sources):
            raise ValueError("full_profile_research_sources")
        # 本地资料与 eve-corpus:// 入口必须同时出现，宿主也会再核对一次。
        if bool(corpus) != any(source.startswith("eve-corpus:") for source in research_sources):
            raise ValueError("full_profile_corpus")
        extra = list(FULL_PROFILE)
        if mindustry_jar:
            extra += ["--practice-mindustry-server", str(mindustry_jar)]
        else:
            extra += ["--practice-browser", str(browser)]
        if corpus:
            extra += ["--research-corpus", str(corpus)]
        for source in research_sources:
            extra += ["--research-source", source]
        # 回复时可查的参考资料：名称=目录，名称不得重复；格式由宿主再核对。
        names = [reference.partition("=")[0] for reference in references]
        if (len(references) > MAX_REFERENCES or len(set(names)) != len(names)
                or any(not name or not value for name, _, value in (r.partition("=") for r in references))):
            raise ValueError("full_profile_references")
        for reference in references:
            extra += ["--reference", reference]
    else:
        raise ValueError("profile")
    return [str(binary), *extra, "--state-dir", str(state_dir)]


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf8")
    temporary.replace(path)


def collect(state_path):
    if not state_path.exists():
        return {"evidence_status": "no_state", "counts": {}, "evidence": []}
    state = json.loads(state_path.read_bytes())
    if state.get("version") != 1:
        raise ValueError("state_version")
    entries = state["entries"]
    receipts = json.loads(bytes(entries.get("eve.channel.qqbot", {}).get("receipts.v1", b'{"version":1,"entries":[]}')))
    sessions = json.loads(bytes(entries.get("eve.session", {}).get("sessions.v1", b'{"format_version":1,"sessions":{}}')))
    # 格式 2 增加分段、3 增加工具进展；均只按最终 state / reply 统计训练证据。
    if receipts.get("version") not in (1, 2, 3) or sessions.get("format_version") != 1:
        raise ValueError("evidence_version")
    excluded, prior_turns = set(), {}
    baseline_path = state_path.parent / "round-baseline.v1.json"
    if baseline_path.exists():
        baseline = json.loads(baseline_path.read_bytes())
        if set(baseline) != {"version", "receipts", "turns"} or baseline["version"] != 1 or type(baseline["receipts"]) is not list or type(baseline["turns"]) is not dict:
            raise ValueError("round_baseline_invalid")
        if any(type(v) is not str or not re.fullmatch(r"[a-f0-9]{64}", v) for v in baseline["receipts"]) or len(set(baseline["receipts"])) != len(baseline["receipts"]):
            raise ValueError("round_baseline_invalid")
        excluded, prior_turns = set(baseline["receipts"]), baseline["turns"]
        if not excluded.issubset({receipt_identity(r) for r in receipts["entries"]}) or any(
                type(count) is not int or count < 0 or key not in sessions["sessions"] or count > len(sessions["sessions"][key]["turns"])
                for key, count in prior_turns.items()):
            raise ValueError("round_baseline_mismatch")
    current_receipts = [r for r in receipts["entries"] if receipt_identity(r) not in excluded]
    counts = {"received_records": len(current_receipts), "sent": 0, "failed": 0,
              "unconfirmed": 0, "c2c_sent": 0, "group_at_sent": 0,
              "completed_turns": 0, "model_sent": 0, "failed_turns": 0, "question_replies": 0,
              "multi_question_replies": 0, "paragraphs": 0, "reply_characters": 0,
              "feedback_candidates": 0, "failed_after_tools": 0, "image_received": 0}
    evidence, failures = [], []
    completed_inputs = {}
    for session in sessions["sessions"].values():
        identity = session["key"]["session_id"]
        for turn in session["turns"][prior_turns.get(identity, 0):]:
            if turn["status"]["state"] != "Completed":
                counts["failed_turns"] += 1
                failure = turn["status"].get("failure", {})
                started = failure.get("started_tools")
                counts["failed_after_tools"] += int(type(started) is int and started > 0)
                failures.append({"session": identity, "turn_id": turn.get("id"), "input": turn["input"],
                                 "status": turn["status"]})
                continue
            counts["completed_turns"] += 1
            completed_inputs.setdefault(identity, set()).add((turn["input"], turn["status"]["messages"][-1]["text"]))
    for receipt in current_receipts:
        counts["image_received"] += len(receipt["message"].get("attachments", []))
        status = receipt["state"]
        if status != "Sent":
            counts["failed" if status == "Failed" else "unconfirmed"] += 1
            continue
        counts["sent"] += 1
        msg, reply = receipt["message"], receipt["reply"]
        counts[msg["scope"] + "_sent" if msg["scope"] == "c2c" else "group_at_sent"] += 1
        # 与 Rust Message::session_key 的 JSON 数组和 SHA256 映射一致。
        routing = json.dumps([receipt["app_id"], msg["scope"], msg["target_id"], msg["user_id"]], ensure_ascii=False, separators=(",", ":")).encode()
        identity = "qq:" + hashlib.sha256(routing).hexdigest()
        if (msg["text"], reply) not in completed_inputs.get(identity, set()):
            continue  # 控制确认不冒充模型训练问答。
        counts["model_sent"] += 1
        questions = len(re.findall(r"[?？]+", reply))
        counts["question_replies"] += int(questions > 0)
        counts["multi_question_replies"] += int(questions > 1)
        counts["paragraphs"] += len([p for p in re.split(r"\n\s*\n", reply) if p.strip()])
        counts["reply_characters"] += len(reply)
        feedback = bool(re.search(r"喜欢|不喜欢|希望|不要|太长|太短|分段|语气|称呼|自然|简短|啰嗦", msg["text"]))
        counts["feedback_candidates"] += int(feedback)
        evidence.append({"session": identity, "scope": msg["scope"], "message_id": msg["id"],
                         "input": msg["text"], "reply": reply, "feedback_candidate": feedback})
    return {"evidence_status": "saved", "counts": counts, "evidence": evidence, "failures": failures,
            "limitations": ["问号数量和反馈关键词仅是可复核指标，不代表语义判断或已学会偏好。",
                            "只把 Session Completed 且 QQ Sent 的交互列为训练证据。",
                            "本地表达统计是弱示范，未更新模型权重或已确认长期偏好。",
                            "失败、中断、未确认发送保留在状态包中，不作为成功训练样本。"]}


def receipt_identity(receipt):
    value = json.dumps([receipt["app_id"], receipt["message"]["id"]], ensure_ascii=False, separators=(",", ":"))
    return hashlib.sha256(value.encode()).hexdigest()


def seed_round(root, source):
    source = source.resolve()
    if not source.is_dir() or root.resolve().is_relative_to(source):
        raise ValueError("seed_directory_invalid")
    paths = list(source.rglob("*"))
    if any(p.is_symlink() or not (p.is_dir() or p.is_file()) for p in paths) or sum(p.stat().st_size for p in paths if p.is_file()) > MAX_ARCHIVE:
        raise ValueError("seed_files_invalid")
    state_path = source / "state.json"
    if collect(state_path)["evidence_status"] != "saved":
        raise ValueError("seed_state_invalid")
    state = json.loads(state_path.read_bytes())["entries"]
    receipts = json.loads(bytes(state.get("eve.channel.qqbot", {}).get("receipts.v1", b'{"version":1,"entries":[]}')))
    sessions = json.loads(bytes(state.get("eve.session", {}).get("sessions.v1", b'{"format_version":1,"sessions":{}}')))
    destination = root / "state"
    shutil.copytree(source, destination)
    write_json(destination / "round-baseline.v1.json", {"version": 1,
        "receipts": [receipt_identity(r) for r in receipts["entries"]],
        "turns": {key: len(s["turns"]) for key, s in sessions["sessions"].items()}})


def password():
    value = os.environ.get("QQBOT_APP_SECRET", "")
    if not value.strip():
        raise ValueError("missing_archive_password")
    return value.encode()


def crypt(data, decrypt=False):
    env = {"PATH": os.environ.get("PATH", ""), "QQBOT_APP_SECRET": password().decode()}
    command = ["openssl", "enc", "-aes-256-cbc", "-pbkdf2", "-iter", "200000", "-pass", "env:QQBOT_APP_SECRET"]
    if decrypt:
        command.append("-d")
    result = subprocess.run(command, input=data, capture_output=True, env=env, timeout=30)
    if result.returncode:
        raise ValueError("archive_crypto_failed")
    return result.stdout


def seal(root, output):
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for name in ("state", "training-report.json", "window.json"):
            path = root / name
            if path.exists():
                archive.add(path, arcname=name)
    if len(buffer.getvalue()) > MAX_ARCHIVE:
        raise ValueError("archive_limit")
    ciphertext = crypt(buffer.getvalue())
    salt = os.urandom(32)
    key = hashlib.pbkdf2_hmac("sha256", password(), b"eve.training.authentication.v1" + salt, 200000)
    tag = hmac.digest(key, MAGIC + salt + ciphertext, "sha256")
    temporary = output.with_suffix(".tmp")
    temporary.write_bytes(MAGIC + salt + tag + ciphertext)
    temporary.replace(output)


def unseal(source, output):
    data = source.read_bytes()
    if not data.startswith(MAGIC) or not len(MAGIC) + 64 < len(data) <= MAX_ARCHIVE + 1024:
        raise ValueError("archive_format")
    offset = len(MAGIC)
    salt, tag, ciphertext = data[offset:offset+32], data[offset+32:offset+64], data[offset+64:]
    key = hashlib.pbkdf2_hmac("sha256", password(), b"eve.training.authentication.v1" + salt, 200000)
    if not hmac.compare_digest(tag, hmac.digest(key, MAGIC + salt + ciphertext, "sha256")):
        raise ValueError("archive_authentication_failed")
    if output.exists():
        raise ValueError("restore_destination_exists")
    plaintext = crypt(ciphertext, decrypt=True)
    with tarfile.open(fileobj=io.BytesIO(plaintext), mode="r:gz") as archive:
        members = archive.getmembers()
        if sum(m.size for m in members) > MAX_ARCHIVE or any(
                m.issym() or m.islnk() or m.name.startswith("/") or ".." in Path(m.name).parts or
                not (m.name == "state" or m.name.startswith("state/") or m.name in ("training-report.json", "window.json"))
                for m in members):
            raise ValueError("archive_members")
        archive.extractall(output, filter="data")


def run_window(root, seconds, binary, seed_state=None, command=None):
    if not 60 <= seconds <= 21300:
        raise ValueError("window_range")
    root.mkdir(parents=True, exist_ok=False)
    started = time.time()
    window = {"version": 1, "requested_seconds": seconds, "ready": False,
              "sandbox": os.environ.get("QQBOT_SANDBOX", "") == "true",
              "source_sha": os.environ.get("GITHUB_SHA", "local"), "continued": seed_state is not None}
    write_json(root / "window.json", window)
    if seed_state is not None:
        seed_round(root, seed_state)
    child = None
    with (root / "stdout").open("w") as out, (root / "stderr").open("w") as err:
        command = command or host_command(binary, root / "state")
        child = subprocess.Popen(command, stdout=out, stderr=err)
        interrupted = False
        def stopping(_signal, _frame):
            nonlocal interrupted
            interrupted = True
        old_handlers = {s: signal.signal(s, stopping) for s in (signal.SIGINT, signal.SIGTERM)}
        try:
            while child.poll() is None and not interrupted:
                if not window["ready"] and "EVE_QQBOT_READY" in (root / "stderr").read_text():
                    ready_at = time.time()
                    window.update(ready=True, ready_at=ready_at, deadline=min(ready_at + seconds, started + 21300))
                    write_json(root / "window.json", window)
                    print("QQBot 网关已就绪：可私聊或在已授权群 @ 机器人；/train start 开始主动提问，/help tools 查看已装配的工具。", flush=True)
                    print(f"窗口预计结束 UTC：{time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime(window['deadline']))}", flush=True)
                if time.time() >= window.get("deadline", started + 60):
                    break
                time.sleep(1)
        finally:
            if child.poll() is None:
                child.send_signal(signal.SIGINT)
                try:
                    child.wait(timeout=120)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=10)
            for s, handler in old_handlers.items():
                signal.signal(s, handler)
    window.update(returncode=child.returncode, ended_at=time.time(), interrupted=interrupted,
                  elapsed_seconds=round(time.time() - window.get("ready_at", started)))
    write_json(root / "window.json", window)
    return window


def finish(root, output, profile="training"):
    try:
        report = collect(root / "state/state.json")
    except (ValueError, KeyError, TypeError, OSError):
        # 无法分析的状态仍加密保留；不能伪装为零交互成功。
        report = {"evidence_status": "analysis_failed_state_preserved", "counts": {}, "evidence": []}
    report["window"] = json.loads((root / "window.json").read_bytes())
    write_json(root / "training-report.json", report)
    output.mkdir(parents=True, exist_ok=True)
    seal(root, output / "training-state.enc")
    # 公开 artifact 和 Actions 日志只显示计数，不含正文、路由 ID 或诊断。
    safe = {"window": report["window"], "evidence_status": report["evidence_status"], "counts": report["counts"],
            "process_ok": report["window"]["returncode"] == 0,
            "interaction_ok": (report["counts"].get("model_sent", 0) > 0
                               and all(report["counts"].get(key, 0) == 0
                                       for key in ("failed", "failed_turns", "unconfirmed"))),
            "experience_acceptance": "not_evaluated",
            "training_scope": SCOPES[profile]}
    write_json(output / "training-summary.json", safe)
    print(json.dumps(safe, ensure_ascii=False), flush=True)
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        with open(summary_path, "a", encoding="utf8") as summary:
            summary.write("## " + SUMMARY_TITLES[profile] + "\n\n```json\n" + json.dumps(safe, ensure_ascii=False, indent=2) + "\n```\n\n交流记录和完整报告在加密状态包中。依据实际证据进行下一轮调整。\n")
    return safe


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seconds", type=int, default=21300)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/eve-qqbot").resolve())
    parser.add_argument("--seed-state", type=Path, help="从已停止窗口复制状态开始新轮；原文件保留，本轮计数排除旧记录")
    parser.add_argument("--decrypt", type=Path)
    parser.add_argument("--wait-ready", action="store_true")
    parser.add_argument("--wait-result", action="store_true")
    parser.add_argument("--profile", choices=sorted(SUMMARY_TITLES), default="training")
    parser.add_argument("--practice-browser", type=Path, help="全功能体验的无头 Chromium/Chrome 程序")
    parser.add_argument("--practice-mindustry-server", type=Path, help="全功能体验的 Mindustry 无头服务端 jar")
    parser.add_argument("--research-corpus", type=Path, help="随仓库提供的本地资料目录")
    parser.add_argument("--research-source", action="append", default=[], help="全功能体验的受控研究入口页，可重复至多 8 个")
    parser.add_argument("--reference", action="append", default=[], help="全功能体验回复时可查的参考资料，名称=目录，可重复至多 8 个")
    args = parser.parse_args()
    title = SUMMARY_TITLES[args.profile]
    controller = not (args.decrypt or args.wait_ready or args.wait_result)
    exit_code = 0
    try:
        if args.wait_ready:
            if not args.root:
                raise ValueError("missing_root")
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                path = args.root / "window.json"
                window = json.loads(path.read_bytes()) if path.exists() else {}
                if window.get("ready") and "returncode" not in window:
                    args.output.mkdir(parents=True, exist_ok=True)
                    write_json(args.output / "ready.json", window)
                    print("EVE_QQBOT_READY：真实 SDK 网关已连接，可以开始交流训练。", flush=True)
                    return
                if (args.output / "controller-result.json").exists():
                    raise ValueError("qq_closed_before_checkpoint")
                time.sleep(1)
            raise ValueError("qq_ready_timeout")
        if args.wait_result:
            deadline = time.monotonic() + args.seconds + 300
            path = args.output / "controller-result.json"
            while time.monotonic() < deadline:
                if path.exists():
                    result = json.loads(path.read_bytes())
                    summary = args.output / "training-summary.json"
                    if summary.exists():
                        content = summary.read_text()
                        print(content, flush=True)
                        if os.environ.get("GITHUB_STEP_SUMMARY"):
                            with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf8") as step:
                                step.write("## " + title + "\n\n```json\n" + content + "\n```\n\n完整交流证据已加密保存，供下一轮调整。\n")
                    if result["exit_code"]:
                        raise ValueError("controller_failed_state_preserved")
                    return
                time.sleep(5)
            raise ValueError("controller_result_timeout")
        if args.decrypt:
            unseal(args.decrypt, args.output)
            print("训练状态包已验证并解密。")
            return
        if not args.root:
            raise ValueError("missing_root")
        password()  # 启动前确认能保存训练结果。
        command = host_command(args.binary, args.root / "state", args.profile, args.practice_browser, args.research_source,
                               args.practice_mindustry_server, args.research_corpus,
                               args.reference)
        run_window(args.root, args.seconds, args.binary, args.seed_state, command)
        safe = finish(args.root, args.output, args.profile)
        if not safe["process_ok"] or not safe["window"]["ready"] or safe["evidence_status"] != "saved":
            raise ValueError("qq_process_failed_state_preserved")
    except (ValueError, OSError, subprocess.SubprocessError, tarfile.TarError) as error:
        # 不打印可能包含正文的异常内容。
        code = str(error) if type(error) is ValueError and str(error).isascii() else type(error).__name__
        print("训练窗口失败（" + code + "）；保留已有文件，未清空状态。", flush=True)
        exit_code = 1
    finally:
        if controller:
            args.output.mkdir(parents=True, exist_ok=True)
            write_json(args.output / "controller-result.json", {"exit_code": exit_code})
    raise SystemExit(exit_code)


if __name__ == "__main__":
    main()
