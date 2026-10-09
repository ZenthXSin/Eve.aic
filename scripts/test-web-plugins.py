"""实际 Eve 子进程 + 环回 HTTP + QQ 替身验收插件管理与配置页。

用法：python scripts/test-web-plugins.py target/debug/eve-qqbot [--browser]
浏览器选项需 playwright-core；EVE_PLAYWRIGHT_MODULE 可指定其入口文件。
只使用临时目录和合成凭据，不访问正式 QQ 或外部模型。
"""
import json
import os
import pathlib
import queue
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOKEN = "web-plugin-test-token-synthetic-1234567890"
PLUGIN = "eve.segment.preferences"
ROOT = pathlib.Path(__file__).resolve().parents[1]


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def eventually(action, predicate, message):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        value = action()
        if predicate(value):
            return value
        time.sleep(0.03)
    raise RuntimeError(message)


def main():
    binary = pathlib.Path(sys.argv[1]).resolve()
    requests = []

    class Model(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            body = json.dumps({"choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant", "content": "你好。\n\n我在这里。"}}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="Eve 插件验收 ") as directory:
            work = pathlib.Path(directory)
            state = work / "state"
            state_file = state / "state.json"
            gates = [work / f"gate-{i}" for i in range(4)]
            script = []
            for index in range(3):
                message = {"id": f"web-chat-{index}", "scope": "c2c", "target_id": "u1", "user_id": "u1", "text": f"你好 {index}"}
                if index == 1:
                    message["expected"] = "你好。\n\n我在这里。"
                else:
                    message["expected_segments"] = ["你好。", "我在这里。"]
                script += [{"wait_file": str(gates[index])}, {"send": message},
                           {"wait_receipt": {"path": str(state_file), "id": message["id"], "state": "Sent"}}]
            script += [{"wait_file": str(gates[3])}]
            scenario = work / "scenario.json"
            scenario.write_text(json.dumps({"script": script, "wait_timeout_ms": 90000}), encoding="utf-8")
            env = {key: value for key, value in os.environ.items() if not key.startswith(("EVE_", "QQBOT_"))}
            env.update({"QQBOT_APP_ID": "100000000", "QQBOT_APP_SECRET": "test-app-secret",
                        "QQBOT_SANDBOX": "false", "EVE_OPENAI_API_KEY": "test-model-secret",
                        "EVE_OPENAI_BASE_URL": f"http://127.0.0.1:{server.server_port}",
                        "EVE_OPENAI_MODEL": "initial-model", "EVE_OPENAI_PROTOCOL": "chat", "EVE_WEB_TOKEN": TOKEN})

            def launch(scenario_path):
                child = subprocess.Popen([str(binary), "--state-dir", str(state), "--self-learning",
                                          "--learning-cooldown-ms", "86400000", "--web-listen", "127.0.0.1:0",
                                          "--bridge-script", str(ROOT / "connectors/qqbot/test/fake-bridge.mjs"),
                                          "--bridge-arg", str(scenario_path)], cwd=ROOT, env=env,
                                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8")
                ready = queue.Queue()
                logs = []

                def read(stream):
                    for line in stream:
                        logs.append(line)
                        if "EVE_WEB_READY " in line:
                            ready.put(line.split("EVE_WEB_READY ", 1)[1].strip())

                readers = [threading.Thread(target=read, args=(s,), daemon=True) for s in (child.stdout, child.stderr)]
                for reader in readers:
                    reader.start()
                try:
                    url = ready.get(timeout=20)
                except queue.Empty:
                    child.terminate()
                    child.wait(timeout=15)
                    raise RuntimeError("Web 未就绪：" + "".join(logs)) from None
                return child, url, logs, readers

            def api(url, path, body=None, token=TOKEN, raw=None):
                headers = {"Content-Type": "application/json"}
                if token:
                    headers["Authorization"] = "Bearer " + token
                data = raw if raw is not None else None if body is None else json.dumps(body).encode()
                request = urllib.request.Request(url + path, data=data, headers=headers)
                try:
                    response = urllib.request.urlopen(request, timeout=6)
                except urllib.error.HTTPError as error:
                    response = error
                with response:
                    return response.status, json.loads(response.read())

            def finish(child, logs, readers):
                child.wait(timeout=20)
                for reader in readers:
                    reader.join(timeout=3)
                require(child.returncode == 0, "宿主退出失败：" + "".join(logs))
                require(all(secret not in "".join(logs) for secret in [TOKEN, "test-app-secret", "test-model-secret"]), "日志泄漏合成凭据")
                for stream in (child.stdout, child.stderr):
                    stream.close()

            child, url, logs, readers = launch(scenario)
            try:
                for path in ["/api/plugins", "/api/plugins/operations", "/api/plugin-pages"]:
                    require(api(url, path, token=None)[0] == 401, "未登录插件接口没有被拒绝")
                require(api(url, "/api/plugins")[0] == 200, "无法读取真实插件")
                links = api(url, "/api/plugin-pages")[1]
                require(len(links) == 5 and all(link["plugin_id"] == "eve.config" for link in links), "配置插件没有注册全部页面")
                if "--browser" in sys.argv:
                    subprocess.run(["node", str(ROOT / "scripts/test-web-plugins-browser.mjs"), url, TOKEN], check=True,
                                   timeout=60, env=os.environ.copy())

                def page(page_id):
                    code, value = api(url, "/api/plugin-pages/read", {"plugin_id": "eve.config", "page_id": page_id})
                    require(code == 200, "页面读取失败")
                    return value

                def save(p, values, expected=200):
                    body = {"plugin_id": "eve.config", "page_id": p["descriptor"]["id"], "instance": p["instance"],
                            "expected_revision": p["revision"], "values": values}
                    code, result = api(url, "/api/plugin-pages/save", body)
                    require(code == expected, f"保存状态错误：{code}，应为 {expected}")
                    return result

                primary = page("provider.openai")
                save(primary, {"protocol": "invalid-protocol"}, 400)
                require(page("provider.openai")["revision"] == primary["revision"], "无效配置改变了修订")
                save(primary, {"model": "next-model"})
                save(primary, {"model": "stale-model"}, 409)
                runtime = page("runtime.llm")
                config_file = state / "configuration" / "config.json"
                before_invalid = config_file.read_bytes()
                save(runtime, {"response_mode": "stream"}, 400)
                require(config_file.read_bytes() == before_invalid and page("runtime.llm") == runtime,
                        "宿主不支持的配置改变了磁盘或内存")
                old_parallel = next(f for f in runtime["fields"] if f["id"] == "max_parallel_tool_calls")["value"]
                new_parallel = old_parallel + 1
                result = save(runtime, {"max_parallel_tool_calls": new_parallel})
                require("runtime.llm.max_parallel_tool_calls" in result["restart_required"], "没有标记待重启字段")
                runtime = page("runtime.llm")
                require(next(f for f in runtime["fields"] if f["id"] == "max_parallel_tool_calls")["value"] == old_parallel,
                        "启动时捕获字段错误地热更新")
                duplicate = json.dumps({"plugin_id": "eve.config", "page_id": "provider.openai", "instance": primary["instance"], "expected_revision": 0})[:-1] + ',"values":{"model":null,"model":"bad"}}'
                require(api(url, "/api/plugin-pages/save", raw=duplicate.encode())[0] == 400, "重复字段未拒绝")

                def received(message_id):
                    # 后端以原子替换写入状态；Windows 上读取恰逢替换时会暂时拒绝打开，视为尚未写入，稍后再读。
                    try:
                        state_data = json.loads(state_file.read_text(encoding="utf-8"))
                    except (FileNotFoundError, PermissionError, json.JSONDecodeError):
                        return False
                    receipts = json.loads(bytes(state_data["entries"].get("eve.channel.qqbot", {}).get("receipts.v1", [])) or b'{}')
                    return any(entry["message"]["id"] == message_id and entry["state"] == "Sent" for entry in receipts.get("entries", []))

                def operation(action):
                    listing = api(url, "/api/plugins")[1]
                    target = next(p for p in listing["items"] if p["id"] == PLUGIN)
                    code, receipt = api(url, "/api/plugins/action", {"instance": listing["instance"], "plugin_id": PLUGIN,
                                                                   "expected_state": target["state"], "action": action})
                    require(code == 202, "插件操作没有准入")
                    terminal = eventually(lambda: api(url, "/api/plugins/operations")[1],
                                          lambda records: any(r["id"] == receipt["id"] and r["state"] != "running" for r in records), "插件操作没有终止")
                    require(next(r for r in terminal if r["id"] == receipt["id"])["state"] == "completed", "插件操作失败")
                    require(api(url, "/api/plugins/ack", {"id": receipt["id"]}) == (200, {"removed": True}), "终态记录无法移除")

                for index in range(3):
                    if index == 1:
                        operation("stop")
                    if index == 2:
                        operation("start")
                    gates[index].touch()
                    eventually(lambda: received(f"web-chat-{index}"), bool, "真实 QQ 回复没有正确保存回执")
                require(len(requests) >= 3 and all(request["model"] == "next-model" for request in requests), "新请求没有采用配置页保存的模型")
                old = page("provider.openai")
                gates[3].touch()
                finish(child, logs, readers)
            finally:
                gates[3].touch()
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=20)

            # 独立进程恢复文件覆盖；旧页面实例不能写入新服务。
            restart_gate = work / "restart-stop"
            scenario.write_text(json.dumps({"script": [{"wait_file": str(restart_gate)}]}), encoding="utf-8")
            child, url, logs, readers = launch(scenario)
            try:
                recovered = api(url, "/api/plugin-pages/read", {"plugin_id": "eve.config", "page_id": "provider.openai"})[1]
                require(recovered["revision"] == old["revision"] and recovered["instance"] != old["instance"], "独立进程配置恢复失败")
                require(next(f for f in recovered["fields"] if f["id"] == "model")["value"] == "next-model", "文件覆盖没有优先于启动环境")
                recovered_runtime = api(url, "/api/plugin-pages/read", {"plugin_id": "eve.config", "page_id": "runtime.llm"})[1]
                require(next(f for f in recovered_runtime["fields"] if f["id"] == "max_parallel_tool_calls")["value"] == new_parallel,
                        "重启后没有应用保存的并行数")
                require(api(url, "/api/plugin-pages/save", {"plugin_id": "eve.config", "page_id": "provider.openai", "instance": old["instance"],
                        "expected_revision": recovered["revision"], "values": {"model": "old-instance"}})[0] == 409, "旧实例页面能够修改新服务")
                restart_gate.touch()
                finish(child, logs, readers)
            finally:
                restart_gate.touch()
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=20)
        print(json.dumps({"plugin_pages": True, "revision_conflict": True, "restart_recovery": True,
                          "actual_qq_stop_start": True, "browser": "--browser" in sys.argv,
                          "external_model_requests": 0, "production_qq_connections": 0}))
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)


if __name__ == "__main__":
    main()
