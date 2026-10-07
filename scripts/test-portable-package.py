"""原生解压便携包，实际运行内置程序、Node、平台启动器和 Web。

只使用合成凭据、回环模型与 QQ 替身，停止本脚本创建的进程。
"""
import hashlib
import json
import os
import pathlib
import queue
import subprocess
import sys
import tempfile
import tarfile
import threading
import time
import urllib.error
import urllib.request
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def run(root, config, bridge, scenario, stop_gate, requests, expected_requests):
    config_path = root / "config.json"
    before = config_path.read_bytes()
    if os.name == "nt":
        command = ["powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive",
                   "-ExecutionPolicy", "Bypass", "-File", str(root / "Launch.ps1"),
                   "-NoPrompt", "-BridgeScript", str(bridge), "-BridgeArg", str(scenario)]
    else:
        command = [str(root / "runtime/bin/node"), str(root / "Launch.mjs"), "qq",
                   "--no-prompt", "--bridge-script", str(bridge), "--bridge-arg", str(scenario)]
    child = subprocess.Popen(command, cwd=root.parent, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
    lines = []
    ready = queue.Queue()

    def read(stream):
        for line in stream:
            lines.append(line)
            marker = "EVE_WEB_READY "
            if marker in line:
                # Windows PowerShell 5.1 can format a native stderr record.
                url = line.split(marker, 1)[1].strip()
                if url.startswith("http://"):
                    ready.put(url)

    readers = [threading.Thread(target=read, args=(stream,), daemon=True)
               for stream in (child.stdout, child.stderr)]
    for reader in readers:
        reader.start()
    try:
        try:
            url = ready.get(timeout=30)
        except queue.Empty:
            raise RuntimeError("原生启动器没有使 Web 就绪：" + "".join(lines)) from None

        def api(path, body=None, token=None):
            headers = {"Content-Type": "application/json"}
            if token:
                headers["Authorization"] = "Bearer " + token
            data = None if body is None else json.dumps(body).encode()
            request = urllib.request.Request(url + path, data=data, headers=headers)
            try:
                response = urllib.request.urlopen(request, timeout=5)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                raw = response.read()
                return response.status, raw

        require(api("/")[0] == 200, "内嵌 Web 页面不可访问")
        require(api("/api/status")[0] == 401, "面板未拒绝无令牌访问")
        token = config["web"]["token"]
        require(api("/api/status", token=token)[0] == 200, "有效令牌无法查看状态")
        require(api("/api/goals", token=token)[0] == 200, "认知目标页没有开启")
        require(api("/api/judgments", token=token)[0] == 200, "判断诊断页没有开启")
        state_file = root / "data/qq/state.json"
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            if not state_file.exists():
                require(child.poll() is None, "宿主在首次保存前退出：" + "".join(lines))
                time.sleep(0.02)
                continue
            document = json.loads(state_file.read_text(encoding="utf-8"))
            receipts = json.loads(bytes(document["entries"].get("eve.channel.qqbot", {}).get("receipts.v1", [])) or b'{}')
            if any(item["message"]["id"] == "package-chat" and item["state"] == "Sent"
                   for item in receipts.get("entries", [])):
                break
            time.sleep(0.02)
        else:
            raise RuntimeError("发行 exe 没有实际确认完成 QQ 消息")
        code, raw = api("/api/memory/scopes", {}, token)
        scopes = json.loads(raw)
        require(code == 200 and len(scopes["items"]) == 1, "记忆作用域页没有实际读取交互")
        scope = scopes["items"][0]["scope"]
        require(api("/api/memory/scope", {"scope": scope}, token)[0] == 200, "记忆详情无法读取")
        code, raw = api("/api/memory/learning", {"scope": scope}, token)
        require(code == 200 and json.loads(raw)["autonomous"], "学习候选页没有启用自主模式")
        require(len(requests) == expected_requests, "恢复重放了旧请求或出现额外模型调用")
        stop_gate.touch()
        child.wait(timeout=30)
        for reader in readers:
            reader.join(timeout=3)
        require(child.returncode == 0, "原生启动器退出失败：" + "".join(lines))
        require(config_path.read_bytes() == before, "启动器重写了现有配置")
        output = "".join(lines)
        for secret in (config["qq"]["app_secret"], config["model"]["api_key"], token):
            require(secret not in output, "启动日志泄漏了合成凭据")
        sessions = json.loads(bytes(json.loads(state_file.read_text(encoding="utf-8"))["entries"]["eve.session"]["sessions.v1"]))
        turns = [turn for session in sessions["sessions"].values() for turn in session["turns"]]
        require(len(turns) == 1 and turns[0]["status"]["state"] == "Completed", "完成历史未保存或旧消息被重放")
        receipt = next(item for item in receipts["entries"] if item["message"]["id"] == "package-chat")
        require(len(receipt["segments"]["parts"]) == 2, "回复未分成两条 QQ 消息")
        require(all(part["state"] == "Sent" for part in receipt["segments"]["parts"]), "分段发送回执不完整")
    finally:
        stop_gate.touch()
        if child.poll() is None:
            try:
                child.wait(timeout=20)
            except subprocess.TimeoutExpired:
                if os.name == "nt":
                    subprocess.run(["taskkill", "/PID", str(child.pid), "/T", "/F"],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
                else:
                    child.terminate()
                    try:
                        child.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        child.kill()
                child.wait(timeout=10)
        for stream in (child.stdout, child.stderr):
            stream.close()


def main():
    dist = pathlib.Path(sys.argv[1]).resolve()
    archives = list(dist.glob("Eve-*.zip")) + list(dist.glob("Eve-*.tar.gz"))
    require(len(archives) == 1, "必须有一个待验收发行 ZIP")
    archive = archives[0]
    checksums = list(dist.glob("SHA256SUMS-*.txt"))
    require(len(checksums) == 1, "必须有一个对应平台的发行校验清单")
    expected = checksums[0].read_text().split()[0]
    require(hashlib.sha256(archive.read_bytes()).hexdigest() == expected, "发行 ZIP 校验值不符")
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
        with tempfile.TemporaryDirectory(prefix="Eve 验收 空格 ") as temporary:
            work = pathlib.Path(temporary)
            if archive.suffix == ".zip":
                with zipfile.ZipFile(archive) as zipped:
                    zipped.extractall(work)
            else:
                with tarfile.open(archive) as tar:
                    tar.extractall(work, filter="data")
            root = next(work.glob("Eve-*"))
            manifest = json.loads((root / "build-info.json").read_text(encoding="utf-8"))
            require(("windows" in manifest["target"]) == (os.name == "nt"), "构建目标错误")
            for path, digest in manifest["files_sha256"].items():
                require(hashlib.sha256((root / path).read_bytes()).hexdigest() == digest, "解压文件校验失败：" + path)
            require(not (root / "config.json").exists() and not (root / "data").exists(), "发行包混入用户密钥或状态")
            config = json.loads((root / "config.example.json").read_text(encoding="utf-8"))
            require(config["model"]["api_key"] == config["qq"]["app_secret"] == config["jev"]["api_key"] == "", "模板含密钥")
            config["model"]["api_key"] = "test-model-secret"
            config["model"]["base_url"] = f"http://127.0.0.1:{server.server_port}"
            config["qq"]["app_secret"] = "test-app-secret"
            config["web"] = {"listen": "127.0.0.1:0", "token": "synthetic-package-panel-token-12345678901234567890"}
            (root / "config.json").write_text(json.dumps(config), encoding="utf-8")
            if os.name == "nt":
                validation_command = ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                                      "-File", str(root / "Launch.ps1"), "-ValidateOnly"]
            else:
                validation_command = [str(root / "runtime/bin/node"), str(root / "Launch.mjs"), "qq", "--validate-only"]
                script_extension = "command" if sys.platform == "darwin" else "sh"
                for name in ("Start-Eve", "Start-Console", "Open-Panel"):
                    require(os.access(root / f"{name}.{script_extension}", os.X_OK), "解压丢失启动脚本执行权限")
            validation = subprocess.run(validation_command, capture_output=True, timeout=20)
            require(validation.returncode == 0, "启动器配置预检失败：" + validation.stderr.decode(errors="replace"))
            bridge = work / "离线 桥接.mjs"
            bridge.write_bytes((pathlib.Path(__file__).resolve().parents[1] / "connectors/qqbot/test/fake-bridge.mjs").read_bytes())
            message = {"id": "package-chat", "scope": "c2c", "target_id": "user-1", "user_id": "user-1",
                       "text": "你好", "expected_segments": ["你好。", "我在这里。"]}
            for restart in (False, True):
                stop_gate = work / f"stop-{restart}"
                scenario = work / f"scenario-{restart}.json"
                steps = [{"send": message}]
                if restart:
                    steps += [{"wait_command": {"id": message["id"], "type": "finish"}}]
                else:
                    steps += [{"wait_receipt": {"path": str(root / "data/qq/state.json"), "id": message["id"], "state": "Sent"}}]
                steps += [{"wait_file": str(stop_gate)}]
                scenario.write_text(json.dumps({"script": steps}), encoding="utf-8")
                run(root, config, bridge, scenario, stop_gate, requests, 1)
            require(len(requests) == 1, "恢复产生了新的模型请求")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
    report = {"format_version": 2, "platform": sys.platform, "platform_label": manifest["platform_label"],
              "target": manifest["target"], "archive": archive.name,
              "source_commit": manifest["source_commit"], "archive_sha256": expected,
              "powershell_5_1": os.name == "nt", "posix_launcher": os.name != "nt",
              "unicode_and_space_path": True, "web_http": True,
              "memory_and_learning_views": True, "qq_segmented_delivery": True,
              "recovery_without_replay": True, "external_model_requests": 0,
              "production_qq_connections": 0}
    (dist / f"acceptance-{manifest['platform_label']}.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    main()
