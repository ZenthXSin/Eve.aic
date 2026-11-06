"""适配器入口验收的本地 HTTP/SSE 替身；不调用真实模型或改写生产状态。

复用 MemoryAcceptance 的桥接握手、临时状态和进程退出核验，只替换模型服务。
宿主进程必须来自待验收源码；此夹具不编译二进制，也不把静态检查算作进程通过。
"""
import json
import pathlib
import tempfile
import threading
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import memory_test


MODEL = "adapter-process-fixture"
MODEL_KEY = "adapter-process-fixture-key"
CHAT_PATH = "/v1/chat/completions"
RESPONSES_PATH = "/v1/responses"
MAX_OUTPUT = 192


def items(body):
    return body["messages"] if "messages" in body else body["input"]


def content_text(message):
    content = message.get("content") or ""
    if isinstance(content, str):
        return content
    return "".join(part.get("text", "") for part in content)


def latest_user(body):
    return next(content_text(item) for item in reversed(items(body))
                if item.get("role") == "user")


def tool_definitions(body):
    return {entry.get("function", entry)["name"]: entry.get("function", entry)
            for entry in body.get("tools", [])}


def current_results(body):
    history = items(body)
    start = max(index for index, item in enumerate(history) if item.get("role") == "user")
    return [json.loads(item["content"] if item.get("role") == "tool" else item["output"])
            for item in history[start + 1:]
            if item.get("role") == "tool" or item.get("type") == "function_call_output"]


class Reply:
    def __init__(self, text=None, calls=(), turn_state=None):
        assert (text is not None) != bool(calls), "只回复最终正文或一批工具"
        self.text = text
        self.calls = calls
        self.turn_state = turn_state

    def encode(self, path, number):
        if path == CHAT_PATH:
            message = {"role": "assistant", "content": self.text}
            if self.calls:
                message["tool_calls"] = [
                    {"id": call_id, "type": "function", "function": {
                        "name": name, "arguments": json.dumps(arguments, ensure_ascii=False)}}
                    for call_id, name, arguments in self.calls]
            body = {"choices": [{"index": 0, "finish_reason": "tool_calls" if self.calls else "stop",
                                 "message": message}]}
            return "application/json", json.dumps(body, ensure_ascii=False).encode()
        assert path == RESPONSES_PATH
        if self.calls:
            output = [{"type": "function_call", "id": f"item-{number}-{index}",
                       "status": "completed", "call_id": call_id, "name": name,
                       "arguments": json.dumps(arguments, ensure_ascii=False)}
                      for index, (call_id, name, arguments) in enumerate(self.calls)]
        else:
            output = [{"type": "message", "id": f"message-{number}", "status": "completed",
                       "role": "assistant", "phase": "final_answer", "content": [
                           {"type": "output_text", "text": self.text, "annotations": []}]}]
        events = [{"type": "response.output_item.done", "output_index": index, "item": item}
                  for index, item in enumerate(output)]
        events.append({"type": "response.completed", "response": {
            "id": f"response-{number}", "status": "completed", "output": output}})
        encoded = "".join("data: " + json.dumps(event, ensure_ascii=False) + "\n\n"
                          for event in events).encode()
        return "text/event-stream", encoded


def adapter_call(adapter, protocol, call_id, **extra):
    return (call_id, "set_model_adapter", {
        "role": "primary", "adapter": adapter, "protocol": protocol, **extra})


class WireAcceptance(memory_test.MemoryAcceptance):
    for _name in dir(memory_test.MemoryAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    fixture_admin_users = ("user-1",)

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.work = pathlib.Path(self.directory.name)
        self.state_path = self.work / "state/state.json"
        self.requests = []
        self.errors = []
        self.lock = threading.Lock()
        self.runs = 0
        self.run_release = threading.Event()
        self.events = []
        self.callback = lambda _: self.fail("未安排的模型调用")
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                outer.errors.append("适配器切换不应探测模型目录：" + self.path)
                self.send_error(404)

            def do_POST(self):
                try:
                    length = int(self.headers["Content-Length"])
                    assert 0 < length <= 8 * 1024 * 1024
                    captured = {"path": self.path,
                                "headers": {name.lower(): value for name, value in self.headers.items()},
                                "body": json.loads(self.rfile.read(length))}
                    with outer.lock:
                        outer.requests.append(captured)
                        number = len(outer.requests)
                    outer.assert_model_wire(captured)
                    reply = outer.callback(captured)
                    content_type, encoded = reply.encode(self.path, number)
                    self.send_response(200)
                    self.send_header("Content-Type", content_type)
                    self.send_header("Content-Length", str(len(encoded)))
                    if reply.turn_state:
                        self.send_header("x-codex-turn-state", reply.turn_state)
                    self.end_headers()
                    # UTF-8 和 SSE 边界跨 write；真实 parser 必须按字节流重组。
                    for offset in range(0, len(encoded), 17):
                        self.wfile.write(encoded[offset:offset + 17])
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception:
                    outer.errors.append(traceback.format_exc())
                    self.send_error(500)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def model_environment(self):
        return {
            "EVE_OPENAI_API_KEY": MODEL_KEY,
            "EVE_OPENAI_BASE_URL": f"http://127.0.0.1:{self.server.server_port}/v1",
            "EVE_OPENAI_PROTOCOL": "chat",
            "EVE_OPENAI_MODEL": "unused-direct-model",
            "EVE_OPENAI_MODEL_ROLE": "primary",
            "EVE_MODELS_PRIMARY_ENABLED": "true",
            "EVE_MODELS_PRIMARY_PROVIDER": "openai",
            "EVE_MODELS_PRIMARY_MODEL": MODEL,
            "EVE_MODELS_PRIMARY_MAX_OUTPUT_TOKENS": str(MAX_OUTPUT),
            "EVE_MODELS_PRIMARY_CREDENTIAL_REF": "env:EVE_OPENAI_API_KEY",
            "EVE_OPENAI_REASONING_EFFORT": "high",
            "EVE_LLM_RESPONSE_MODE": "complete",
            "EVE_CONTEXT_ADAPTIVE_LIMIT_TOKENS": "0",
            "EVE_CONTEXT_COMPACT_AT_TOKENS": "0",
        }

    def assert_model_wire(self, captured):
        body, headers, path = captured["body"], captured["headers"], captured["path"]
        self.assertIn(path, (CHAT_PATH, RESPONSES_PATH))
        self.assertEqual(headers.get("authorization"), "Bearer " + MODEL_KEY)
        self.assertEqual(body["model"], MODEL)
        self.assertNotIn("temperature", body)
        if path == CHAT_PATH:
            self.assertEqual(body["reasoning_effort"], "high")
            self.assertEqual(body["max_tokens"], MAX_OUTPUT)
            self.assertFalse(body["stream"])
        else:
            self.assertEqual(body["reasoning"]["effort"], "high")
            self.assertEqual(body["max_output_tokens"], MAX_OUTPUT)
            self.assertTrue(body["stream"])
        definitions = tool_definitions(body)
        self.assertIn("set_model_adapter", definitions, "真实用户入口必须把工具交给模型")
        self.assertFalse(definitions["set_model_adapter"]["parameters"]["additionalProperties"])
        for name in ("session-id", "thread-id", "x-client-request-id"):
            self.assertTrue(headers.get(name), "缺少模型会话头：" + name)
        self.assertTrue(body.get("prompt_cache_key"))

    def assert_saved(self, result, adapter, protocol):
        self.assertTrue(result["saved"], result)
        self.assertEqual((result["role"], result["adapter"], result["protocol"], result["model"]),
                         ("primary", adapter, protocol, MODEL))
        self.assertTrue(result["inflight_unchanged"])
        self.assertFalse(result["provider_availability_verified"])

    def configuration(self):
        return json.loads((self.work / "state/configuration/config.json").read_text(encoding="utf8"))

    def assert_adapter_config(self, adapter, protocol):
        current = self.configuration()["current"]
        self.assertEqual(current["namespaces"]["runtime.models"]["values"],
                         {"primary_provider": adapter})
        self.assertEqual(current["namespaces"]["provider.openai"]["values"],
                         {"model_role": "primary", "protocol": protocol})
        return current

    def assert_same_session(self, records):
        self.assertGreater(len(records), 1)
        for name in ("session-id", "thread-id"):
            self.assertEqual(len({record["headers"][name] for record in records}), 1)
        self.assertEqual(len({record["body"]["prompt_cache_key"] for record in records}), 1)
        self.assertEqual(len({record["headers"]["x-client-request-id"] for record in records}),
                         len(records), "每次真实调用独立标识，切换和重启不复用请求 ID")

    def tearDown(self):
        super().tearDown()
        self.assertFalse(self.errors, "\n".join(self.errors))
