"""隔离 Web 进程的真实模型交流学习观察；仅使用合成对话，不接 QQ。

运行：python3 scripts/conversation-model-acceptance.py --binary <eve-web> --output <报告>
模型从 EVE_OPENAI_* 环境或 --model-config 指定的宿主配置读取。消耗真实模型额度。
保存原始合成输入、实际回复、持久经验及失败；不以关键词计分宣称达到真人水平。
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--model-config", type=Path)
    parser.add_argument("--reasoning-effort", default="low", choices=["none", "low", "medium", "high"])
    args = parser.parse_args()
    binary = args.binary.resolve()
    environment = {k: os.environ[k] for k in ("PATH", "SystemRoot", "TEMP", "TMP") if k in os.environ}
    environment.update({k: v for k, v in os.environ.items() if k.startswith("EVE_OPENAI_")})
    if args.model_config:
        model = json.loads(args.model_config.read_text())["model"]
        environment.update(EVE_OPENAI_API_KEY=model["api_key"], EVE_OPENAI_BASE_URL=model["base_url"],
                           EVE_OPENAI_MODEL=model["name"], EVE_OPENAI_PROTOCOL=model["protocol"])
    if not environment.get("EVE_OPENAI_API_KEY"):
        raise SystemExit("缺少实际模型凭据")
    work = Path(tempfile.mkdtemp(prefix="eve-conversation-model-"))
    state = work / "state"
    token = secrets.token_hex(32)
    environment.update(EVE_WEB_TOKEN=token, EVE_LLM_RESPONSE_MODE="complete",
                       EVE_OPENAI_TIMEOUT_SECONDS="60", EVE_OPENAI_TRANSIENT_RETRIES="false",
                       EVE_MODELS_PRIMARY_TIMEOUT_MS="60000",
                       EVE_TRACE_DIALOGUE_TIMING="true",
                       EVE_OPENAI_MAX_OUTPUT_TOKENS="2048", EVE_OPENAI_REASONING_EFFORT=args.reasoning_effort,
                       EVE_CONTEXT_ADAPTIVE_LIMIT_TOKENS="0", EVE_CONTEXT_COMPACT_AT_TOKENS="0")
    report = {"model": environment.get("EVE_OPENAI_MODEL"), "reasoning_effort":args.reasoning_effort,
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "work": str(work), "dialogue": [], "lessons": [], "errors": [], "model_metrics": [],
              "limits": "合成场景、单次实际模型运行；不是用户满意度或真人相似度评分。"}
    child = None
    log = None
    url = None
    run = 0

    def save():
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf8")

    def start():
        nonlocal child, log, url, run
        run += 1
        log_path = work / f"host-{run}.log"
        log = log_path.open("w")
        child = subprocess.Popen([str(binary), "--listen", "127.0.0.1:0", "--state-dir", str(state),
                                  "--agent", str(ROOT / "AGENT.md")], cwd=work, env=environment,
                                 stdout=subprocess.DEVNULL, stderr=log)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            for line in log_path.read_text().splitlines():
                if line.startswith("EVE_WEB_READY "):
                    url = line.split(" ", 1)[1]
                    return
            if child.poll() is not None:
                raise RuntimeError("Web 启动失败，退出码 " + str(child.returncode))
            time.sleep(.1)
        raise RuntimeError("Web 启动超时")

    def stop():
        nonlocal child, log
        if child and child.poll() is None:
            child.send_signal(signal.SIGINT)
            try:
                child.wait(timeout=20)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
                report["errors"].append("Web 正常退出超时，已终止隔离进程")
        if log:
            log.close()
        child = log = None

    def post(path, body):
        request = urllib.request.Request(url + path, data=json.dumps(body, ensure_ascii=False).encode(),
                                         headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError("Web API 状态 " + str(error.code)) from None

    def lessons():
        path = state / "state.json"
        if not path.exists():
            return []
        entries = json.loads(path.read_text())["entries"].get("eve.training", {})
        document = json.loads(bytes(entries.get("conversation.v1", b"{}")))
        return [lesson for scope in document.get("scopes", [])
                if scope["scope"]["session_id"] == "web:chat:" + "a" * 32 for lesson in scope["lessons"]]

    def metrics(label):
        request = urllib.request.Request(url + "/api/readiness", headers={"Authorization":"Bearer " + token})
        with urllib.request.urlopen(request, timeout=5) as response:
            roles = json.load(response)["roles"]
        report["model_metrics"].append({"label":label,"host_run":run,
            "roles":[{key: role.get(key) for key in ("role","configured_model","requests","failures","last_failure","cache")} for role in roles]})

    def turn(text, label, conversation="a" * 32):
        request = {"conversation_id": conversation, "request_id": secrets.token_hex(16), "text": text}
        began = time.monotonic()
        post("/api/chat/send", request)
        lookup = {k: request[k] for k in ("conversation_id", "request_id")}
        while time.monotonic() - began < 150:
            receipt = post("/api/chat/read", lookup)
            if receipt["status"] in ("completed", "failed", "unknown"):
                report["dialogue"].append({"label": label, "input": text, "status": receipt["status"],
                                           "reply": receipt.get("text"), "failure": receipt.get("failure"), "seconds": round(time.monotonic() - began, 2)})
                report["lessons"] = lessons()
                metrics(label)
                save()
                print(json.dumps({"label": label, "status": receipt["status"], "lessons": len(report["lessons"])}, ensure_ascii=False), flush=True)
                if receipt["status"] != "completed":
                    raise RuntimeError("交流未完成：" + label)
                return
            time.sleep(.25)
        raise RuntimeError("交流超时：" + label)

    try:
        start()
        turn("今天跑了一整天，有点累", "交流起点")
        turn("我说累的时候，别劝我早睡、列计划，也别马上追问原因，就陪我吐槽两句；真想要建议我会直接问", "本人明确纠正")
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline and not any(l["active"] for l in lessons()):
            time.sleep(.5)
        report["active_after_feedback"] = any(l["active"] for l in lessons())
        turn("今天会议从早排到晚，终于关电脑了", "迁移到新话题")
        turn("明早必须九点到公司，地铁要四十分钟，出门准备二十分钟，帮我算最晚几点起床", "明确求助仍需解决")
        turn("嗯，我先去吃饭了", "尊重收尾")
        turn("今天会议从早排到晚，终于关电脑了", "另一会话对照", "c" * 32)
        before = lessons()
        stop()
        start()
        report["restart_preserved_lessons"] = lessons() == before
        turn("今天路上堵了很久，刚到家", "重启后继续交流")
    except Exception as error:
        # 不序列化 HTTP 请求、地址和模型配置，避免把凭据带入公开验收报告。
        report["errors"].append(str(error) if isinstance(error, RuntimeError) else type(error).__name__)
    finally:
        stop()
        report["lessons"] = lessons()
        save()
    if report["errors"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
