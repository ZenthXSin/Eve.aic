#!/usr/bin/env python3
"""本地假上游 + 适配层自测，不打扰真实服务。"""
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

sys.path.insert(0, "imgadapter")
import adapter  # noqa: E402

STATE = {"polls": 0, "submit_model": None, "auth": None}


class Fake(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _json(self, code, body):
        raw = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length) or b"{}")
        STATE["submit_model"] = body.get("model")
        STATE["auth"] = self.headers.get("Authorization")
        if body.get("prompt") == "fail-me":
            self._json(400, {"error": {"message": "bad prompt"}})
            return
        self._json(202, {"id": "task_test_1", "status": "queued", "progress": "0%"})

    def do_GET(self):
        if not self.path.startswith("/v1/images/generations/task_test_1"):
            self._json(404, {"error": {"message": "not found"}})
            return
        STATE["polls"] += 1
        if STATE["polls"] < 2:
            self._json(200, {"id": "task_test_1", "status": "running", "progress": "50%"})
            return
        self._json(200, {
            "id": "task_test_1", "status": "completed", "progress": "100%",
            "result": {"created": 111, "data": [
                {"b64_json": "AAA"},
                {"url": "/images/1.png"},
            ]},
        })


def run():
    srv = HTTPServer(("127.0.0.1", 0), Fake)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    base = "http://127.0.0.1:%d" % srv.server_port
    adapter.UPSTREAM = base
    adapter.UPSTREAM_KEY = "test-key"
    adapter.PUBLIC_BASE = "https://img.example.com"
    adapter.POLL_INTERVAL = 0.05

    code, body = adapter.create_and_wait({"model": "gpt-image-2.5", "prompt": "x", "n": 1})
    assert code == 200, (code, body)
    assert STATE["submit_model"] == "[官渠]gpt-image-2", STATE["submit_model"]
    assert STATE["auth"] == "Bearer test-key"
    assert STATE["polls"] == 2, STATE["polls"]
    assert body["created"] == 111
    assert body["data"][1]["url"] == "https://img.example.com/images/1.png"
    print("ok 轮询完成 + 模型映射 + 相对URL补全")

    STATE["polls"] = 0
    code, body = adapter.create_and_wait({"model": "fail-me-unknown", "prompt": "fail-me"})
    assert code == 400, (code, body)
    print("ok 上游报错透出")

    adapter.MODEL_MAP["sync-model"] = "sync-model"
    code, body = adapter.create_and_wait({"model": "gpt-image-2", "prompt": "x"})
    assert code == 200
    print("ok 多模型")

    # 超时分支：让上游一直返回 running
    adapter.POLL_DEADLINE = 0.15
    adapter.POLL_INTERVAL = 0.05
    STATE["polls"] = 0
    original = Fake.do_GET

    def always_running(self):
        STATE["polls"] += 1
        self._json(200, {"id": "task_test_1", "status": "running", "progress": "10%"})

    Fake.do_GET = always_running
    try:
        code, body = adapter.create_and_wait({"model": "gpt-image-2", "prompt": "x"})
    finally:
        Fake.do_GET = original
    assert code == 504, (code, body)
    print("ok 超时返回 504")

    print("全部通过")


if __name__ == "__main__":
    run()
