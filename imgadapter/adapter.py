#!/usr/bin/env python3
"""把 22codex 的异步生图接口适配成 OpenAI 同步生图接口。

上游协议:
  POST {UPSTREAM}/v1/images/generations          -> 202 {"id": "task_xxx", "status": "queued"}
  GET  {UPSTREAM}/v1/images/generations/{task}   -> {"status": "completed", "result": {"data": [...]}}

本服务暴露:
  POST /v1/images/generations  同步返回 OpenAI 风格结果
  GET  /v1/models
  GET  /healthz
"""
from __future__ import annotations

import json
import os
import threading
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

UPSTREAM = os.environ.get("UPSTREAM_BASE", "https://app.22codex.xyz").rstrip("/")
UPSTREAM_KEY = os.environ.get("UPSTREAM_KEY", "")
PUBLIC_BASE = os.environ.get("PUBLIC_BASE", "").rstrip("/")
MODEL_PREFIX = os.environ.get("UPSTREAM_MODEL_PREFIX", "[官渠]")
POLL_INTERVAL = float(os.environ.get("POLL_INTERVAL", "2"))
POLL_DEADLINE = float(os.environ.get("POLL_DEADLINE", "280"))
PORT = int(os.environ.get("PORT", "8010"))

MODEL_MAP = {
    "gpt-image-2": MODEL_PREFIX + "gpt-image-2",
    "gpt-image-2.5-flare": MODEL_PREFIX + "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst": MODEL_PREFIX + "gpt-image-2.5-sunburst",
    "gpt-image-2.5": MODEL_PREFIX + "gpt-image-2",
}

_httpd = None


def _headers() -> dict:
    return {
        "Authorization": "Bearer " + UPSTREAM_KEY,
        "Content-Type": "application/json",
        # 上游前置 Cloudflare，默认 urllib UA 会被挡成 403/1010
        "User-Agent": "curl/8.5.0",
        "Accept": "application/json",
    }


def _request(method: str, path: str, payload: dict | None = None, timeout: float = 120):
    data = json.dumps(payload).encode() if payload is not None else None
    req = Request(UPSTREAM + path, data=data, headers=_headers(), method=method)
    try:
        with urlopen(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read() or b"{}")
    except HTTPError as exc:
        body = exc.read()
        try:
            parsed = json.loads(body or b"{}")
        except json.JSONDecodeError:
            parsed = {"raw": body[:500].decode("utf-8", "replace")}
        return exc.code, parsed


def create_and_wait(payload: dict) -> tuple[int, dict]:
    model = str(payload.get("model") or "gpt-image-2")
    body = dict(payload)
    body["model"] = MODEL_MAP.get(model, model)

    status, created = _request("POST", "/v1/images/generations", body)
    if status not in (200, 201, 202):
        return status, created

    if isinstance(created.get("data"), list):
        return 200, created

    task_id = created.get("id") or ""
    if not task_id:
        return 502, {"error": {"message": "upstream returned no task id", "type": "upstream_error"}}

    deadline = time.time() + POLL_DEADLINE
    while time.time() < deadline:
        time.sleep(POLL_INTERVAL)
        st, info = _request("GET", "/v1/images/generations/" + task_id, None, timeout=60)
        if st != 200:
            continue
        state = str(info.get("status") or "").lower()
        if state in ("completed", "succeeded", "success"):
            result = info.get("result") or info
            items = result.get("data") or []
            fixed = []
            for item in items:
                entry = dict(item)
                url = entry.get("url")
                if url and url.startswith("/") and PUBLIC_BASE:
                    entry["url"] = PUBLIC_BASE + url
                fixed.append(entry)
            if not fixed:
                return 502, {"error": {"message": "empty result", "type": "upstream_error"}}
            return 200, {"created": result.get("created") or int(time.time()), "data": fixed}
        if state in ("failed", "error", "cancelled"):
            return 502, {"error": {"message": str(info.get("error") or state), "type": "upstream_error"}}
    return 504, {"error": {"message": "image generation timed out", "type": "timeout"}}


class Handler:
    def __call__(self, environ, start_response):
        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        def send(code: int, body: dict):
            raw = json.dumps(body).encode()
            start_response(str(code) + " X", [("Content-Type", "application/json"),
                                              ("Content-Length", str(len(raw)))])
            return [raw]

        if path == "/healthz":
            return send(200, {"status": "ok"})
        if path == "/v1/models" and method == "GET":
            return send(200, {"object": "list",
                              "data": [{"id": k, "object": "model", "owned_by": "22codex"} for k in MODEL_MAP]})
        if path == "/v1/images/generations" and method == "POST":
            try:
                length = int(environ.get("CONTENT_LENGTH") or 0)
                payload = json.loads(environ["wsgi.input"].read(length) or b"{}")
            except (ValueError, json.JSONDecodeError):
                return send(400, {"error": {"message": "invalid json", "type": "invalid_request_error"}})
            code, body = create_and_wait(payload)
            return send(code, body)
        return send(404, {"error": {"message": "not found", "type": "invalid_request_error"}})


def start(upstream: str | None = None, key: str | None = None) -> str:
    global _httpd, UPSTREAM, UPSTREAM_KEY
    if upstream:
        UPSTREAM = upstream.rstrip("/")
    if key is not None:
        UPSTREAM_KEY = key
    from wsgiref.simple_server import WSGIRequestHandler, make_server

    class QuietHandler(WSGIRequestHandler):
        def log_message(self, fmt, *args):  # 静默访问日志，开销留给业务
            pass

    # ThreadingWSGIServer：生图同步等待最长 280s，单线程会堵死并发请求
    from socketserver import ThreadingMixIn
    from wsgiref.simple_server import WSGIServer

    class ThreadingWSGIServer(ThreadingMixIn, WSGIServer):
        daemon_threads = True

    _httpd = make_server("127.0.0.1", PORT, Handler(), server_class=ThreadingWSGIServer,
                         handler_class=QuietHandler)
    threading.Thread(target=_httpd.serve_forever, daemon=True).start()
    return "http://127.0.0.1:%d" % PORT


def stop() -> None:
    global _httpd
    if _httpd is not None:
        _httpd.shutdown()
        _httpd.server_close()
        _httpd = None


if __name__ == "__main__":
    print("listening on", start())
    while True:
        time.sleep(3600)
