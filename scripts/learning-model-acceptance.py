"""首个自主学习目标的真实模型验收：真实模型 + 真实 Mindustry 服务端，QQ 平台仍为替身。

用户只在私聊里提一次兴趣，之后依次闲聊一次、提出一个想法。脚本观察持久状态，记录每个里程碑
是否达成、用时多少，以及模型实际写出的知识、草稿结果、邀请正文与回应识别结论，输出一份报告。

真实模型的结果不可预知：里程碑没有达成不算失败，报告如实记录；进程异常退出、状态无法读取
或违反宿主不变量（例如没有已验证实践却出现邀请）才算失败。只在主线上手动触发，消耗模型额度。

用法：python3 scripts/learning-model-acceptance.py <eve-qqbot> <报告路径>
需要环境变量 EVE_OPENAI_API_KEY、EVE_MINDUSTRY_SERVER_JAR；可选 EVE_OPENAI_BASE_URL、
EVE_OPENAI_MODEL、EVE_OPENAI_PROTOCOL、EVE_RESEARCH_SOURCE、EVE_LEARNING_BUDGET_SECONDS。
"""
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
BRIDGE = ROOT / "connectors/qqbot/test/fake-bridge.mjs"
INTEREST = "我喜欢 Mindustry 这个游戏的模组，但是不知道怎么创作。"
CHAT = "今天天气不错，随便聊聊。"
IDEA = "那就加一个更结实的墙吧，生命值高一点。"
DEFAULT_SOURCE = "https://mindustrygame.github.io/wiki/modding/1-modding/"


def message(id, text):
    return {"id": id, "text": text, "scope": "c2c", "user_id": "user-1", "target_id": "user-1",
            "expected_type": "reply", "accept_any": True}


def documents(state):
    path = state / "state.json"
    for _ in range(20):
        try:
            document = json.loads(path.read_text(encoding="utf-8"))
            break
        except FileNotFoundError:
            return {}
        except (PermissionError, json.JSONDecodeError):
            time.sleep(0.1)
    else:
        raise RuntimeError("状态文件无法读取")
    return {owner: {key: json.loads(bytes(value)) for key, value in entries.items()}
            for owner, entries in document["entries"].items()}


def ledger(docs, owner, key, default):
    return docs.get(owner, {}).get(key) or default


def invitations(docs):
    return ledger(docs, "eve.outreach", "outreach.v1", {"invitations": []})["invitations"]


def practice_runs(docs):
    return ledger(docs, "eve.practice", "practice.v1", {"runs": []})["runs"]


def status_name(status):
    return next(iter(status)) if isinstance(status, dict) else status


def wait(predicate, seconds, child):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if child.poll() is not None:
            return False
        if predicate():
            return True
        time.sleep(1)
    return False


def main():
    binary, report_path = pathlib.Path(sys.argv[1]).resolve(), pathlib.Path(sys.argv[2])
    budget = int(os.environ.get("EVE_LEARNING_BUDGET_SECONDS", "1500"))
    for name in ("EVE_OPENAI_API_KEY", "EVE_MINDUSTRY_SERVER_JAR"):
        if not os.environ.get(name):
            sys.exit(f"缺少环境变量 {name}")
    work = pathlib.Path(tempfile.mkdtemp(prefix="eve-learning-"))
    state = work / "state"
    gate = lambda name: work / f"gate-{name}"
    steps = []
    for index, (id, text) in enumerate((("interest", INTEREST), ("chat", CHAT), ("idea", IDEA)), 1):
        steps += [{"send": message(id, text)}, {"touch": str(gate(f"sent-{index}"))},
                  {"wait_file": str(gate(f"go-{index}"))}]
    scenario = work / "scenario.json"
    scenario.write_text(json.dumps({"script": steps, "events_file": str(work / "events.jsonl"),
                                    "error_file": str(work / "bridge-error.txt")}), encoding="utf8")
    env = {key: os.environ[key] for key in ("PATH", "SystemRoot", "TEMP", "TMP", "HOME", "JAVA_HOME")
           if key in os.environ}
    env.update({key: os.environ[key] for key in os.environ if key.startswith("EVE_OPENAI_")})
    env.update(QQBOT_APP_SECRET="acceptance-placeholder", QQBOT_APP_ID="1")
    command = [str(binary), "--state-dir", str(state), "--agent", str(ROOT / "AGENT.md"),
               "--bridge-script", str(BRIDGE), "--bridge-arg", str(scenario),
               "--interest-learning", "--interest-cooldown-ms", "0",
               "--research-source", os.environ.get("EVE_RESEARCH_SOURCE", DEFAULT_SOURCE),
               "--practice-mindustry-server", os.environ["EVE_MINDUSTRY_SERVER_JAR"],
               "--skill-learning", "--outreach", "--outreach-cooldown-ms", "1000"]
    started = time.monotonic()
    milestones = {}
    child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def mark(name, reached):
        milestones[name] = {"reached": reached, "seconds": round(time.monotonic() - started, 1)}

    def remaining():
        return max(budget - (time.monotonic() - started), 30)

    try:
        wait(lambda: gate("sent-1").exists(), 60, child)
        # 阶段 1：只提一次兴趣；等邀请撰写完成，或实践全部结束仍没有邀请。
        def learned():
            docs = documents(state)
            runs = practice_runs(docs)
            settled = runs and all(run["status"] != "Running" for run in runs)
            return any(status_name(item["status"]) != "Composing" for item in invitations(docs)) or (
                settled and not any(run["status"] == "Verified" for run in runs))
        mark("learned", wait(learned, remaining(), child))
        gate("go-1").touch()
        # 阶段 2：闲聊一次；等这条消息的时机判断结束。
        wait(lambda: gate("sent-2").exists(), 60, child)
        mark("invitation_decided", wait(lambda: any(
            any(judgement["message_id"] == "chat" and judgement["outcome"] for judgement in item["judgements"])
            for item in invitations(documents(state))), 180, child))
        gate("go-2").touch()
        # 阶段 3：提出想法；等回应识别，以及（若识别为想法）后续创作结束。
        wait(lambda: gate("sent-3").exists(), 60, child)

        def followed():
            docs = documents(state)
            answered = any(response["outcome"] for item in invitations(docs)
                           for response in item.get("responses", []))
            follow = [run for run in practice_runs(docs) if run["task"]["goal_id"].startswith("eve.outreach.request.")]
            return answered and (not follow or all(run["status"] != "Running" for run in follow))
        mark("idea_handled", wait(followed, min(remaining(), 900), child))
        gate("go-3").touch()
        stdout, stderr = child.communicate(timeout=120)
    finally:
        if child.poll() is None:
            child.kill()
            stdout, stderr = child.communicate()
    docs = documents(state)
    interests = ledger(docs, "eve.interest", "interests.v1", {"interests": []})["interests"]
    knowledge = ledger(docs, "eve.knowledge", "knowledge.v1", {"entries": []})["entries"]
    skills = ledger(docs, "eve.skill", "skill.v1", {"skills": [], "distillations": [], "selections": []})
    runs = practice_runs(docs)
    invites = invitations(docs)
    violations = []
    verified = {run["id"] for run in runs if run["status"] == "Verified"}
    for item in invites:
        if item["milestone"]["practice_run_id"] not in verified:
            violations.append(f"邀请 {item['id']} 指向未验证的实践")
    if child.returncode != 0:
        violations.append(f"进程退出码 {child.returncode}")
    if (work / "bridge-error.txt").exists():
        violations.append("桥接替身报错：" + (work / "bridge-error.txt").read_text(encoding="utf8"))
    report = {
        "model": os.environ.get("EVE_OPENAI_MODEL"),
        "milestones": milestones,
        "interests": [{"topic": item["topic"], "quotes": [s["quote"] for s in item["statements"]]}
                      for item in interests],
        "knowledge": {"entries": len(knowledge),
                      "source_quoted": sum(1 for entry in knowledge if entry["status"] == "SourceQuoted")},
        "practice": [{"goal": run["task"]["goal_id"], "status": run["status"],
                      "attempts": [{"outcome": attempt.get("outcome"),
                                    "runtime": (attempt.get("evidence") or {}).get("runtime_version"),
                                    "warnings": (attempt.get("evidence") or {}).get("warnings", [])[:3]}
                                   for attempt in run["attempts"]]} for run in runs],
        "skills": [{"name": skill["name"], "enabled": skill.get("enabled")} for skill in skills["skills"]],
        "distillations": [entry["status"] for entry in skills["distillations"]],
        "invitations": [{"goal": item["goal_id"], "status": item["status"], "text": item.get("text"),
                         "judgements": [j["outcome"] for j in item["judgements"]],
                         "responses": [r["outcome"] for r in item.get("responses", [])]} for item in invites],
        "violations": violations,
        "stderr_tail": stderr.splitlines()[-20:],
    }
    report_path.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf8")
    print(json.dumps({"milestones": milestones, "violations": violations}, ensure_ascii=False))
    sys.exit(1 if violations else 0)


if __name__ == "__main__":
    main()
