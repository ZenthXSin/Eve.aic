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
import subprocess
import tarfile
import time

MAGIC = b"EVE-QQ-TRAINING-1\n"
MAX_ARCHIVE = 32 * 1024 * 1024


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
    if receipts.get("version") != 1 or sessions.get("format_version") != 1:
        raise ValueError("evidence_version")
    counts = {"received_records": len(receipts["entries"]), "sent": 0, "failed": 0,
              "unconfirmed": 0, "c2c_sent": 0, "group_at_sent": 0,
              "completed_turns": 0, "failed_turns": 0, "question_replies": 0,
              "multi_question_replies": 0, "paragraphs": 0, "reply_characters": 0,
              "feedback_candidates": 0}
    evidence = []
    completed_inputs = {}
    for session in sessions["sessions"].values():
        identity = session["key"]["session_id"]
        for turn in session["turns"]:
            if turn["status"]["state"] != "Completed":
                counts["failed_turns"] += 1
                continue
            counts["completed_turns"] += 1
            completed_inputs.setdefault(identity, set()).add((turn["input"], turn["status"]["messages"][-1]["text"]))
    for receipt in receipts["entries"]:
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
        questions = len(re.findall(r"[?？]+", reply))
        counts["question_replies"] += int(questions > 0)
        counts["multi_question_replies"] += int(questions > 1)
        counts["paragraphs"] += len([p for p in re.split(r"\n\s*\n", reply) if p.strip()])
        counts["reply_characters"] += len(reply)
        feedback = bool(re.search(r"喜欢|不喜欢|希望|不要|太长|太短|分段|语气|称呼|自然|简短|啰嗦", msg["text"]))
        counts["feedback_candidates"] += int(feedback)
        evidence.append({"session": identity, "scope": msg["scope"], "message_id": msg["id"],
                         "input": msg["text"], "reply": reply, "feedback_candidate": feedback})
    return {"evidence_status": "saved", "counts": counts, "evidence": evidence,
            "limitations": ["问号数量和反馈关键词仅是可复核指标，不代表语义判断或已学会偏好。",
                            "只把 Session Completed 且 QQ Sent 的交互列为训练证据。",
                            "本模式未更新模型权重或长期表达偏好；调整需依据本轮证据另行实现。",
                            "失败、中断、未确认发送保留在状态包中，不作为成功训练样本。"]}


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


def run_window(root, seconds, binary):
    if not 60 <= seconds <= 21300:
        raise ValueError("window_range")
    root.mkdir(parents=True, exist_ok=False)
    started = time.time()
    window = {"version": 1, "requested_seconds": seconds, "ready": False, "source_sha": os.environ.get("GITHUB_SHA", "local")}
    write_json(root / "window.json", window)
    child = None
    with (root / "stdout").open("w") as out, (root / "stderr").open("w") as err:
        child = subprocess.Popen([str(binary), "--training", "--state-dir", str(root / "state")], stdout=out, stderr=err)
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
                    print("QQBot 网关已就绪：可私聊或在已授权群 @ 机器人；/train start 开始主动提问。", flush=True)
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


def finish(root, output):
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
            "interaction_ok": report["counts"].get("completed_turns", 0) > 0 and report["counts"].get("sent", 0) > 0,
            "training_scope": "主动提问和交流证据采集；未更新模型权重或长期偏好"}
    write_json(output / "training-summary.json", safe)
    print(json.dumps(safe, ensure_ascii=False), flush=True)
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        with open(summary_path, "a", encoding="utf8") as summary:
            summary.write("## QQ 主动提问训练结果\n\n```json\n" + json.dumps(safe, ensure_ascii=False, indent=2) + "\n```\n\n交流记录和完整报告在加密状态包中。依据实际证据进行下一轮调整。\n")
    return safe


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seconds", type=int, default=21300)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/eve-qqbot").resolve())
    parser.add_argument("--decrypt", type=Path)
    parser.add_argument("--wait-ready", action="store_true")
    parser.add_argument("--wait-result", action="store_true")
    args = parser.parse_args()
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
                                step.write("## QQ 主动提问训练结果\n\n```json\n" + content + "\n```\n\n完整交流证据已加密保存，供下一轮调整。\n")
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
        run_window(args.root, args.seconds, args.binary)
        safe = finish(args.root, args.output)
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
