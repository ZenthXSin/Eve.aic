"""本机 CPU 语义检索与显式编码器计数，不调用聊天模型、不回传远端文本。"""
import hashlib
import json
import os
from pathlib import Path
import signal
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer
import tiktoken


def main(config_path):
    config = json.loads(Path(config_path).read_text())
    model_dir = Path(config["modelDirectory"])
    manifest = json.loads((model_dir / "manifest.json").read_text())
    for name, saved in manifest["files"].items():
        with (model_dir / name).open("rb") as stream:
            if hashlib.file_digest(stream, "sha256").hexdigest() != saved["sha256"]:
                raise RuntimeError("语义模型校验失败")
    tokenizer = Tokenizer.from_file(str(model_dir / "tokenizer.json"))
    tokenizer.enable_truncation(max_length=512)
    options = ort.SessionOptions()
    options.intra_op_num_threads = 2
    options.inter_op_num_threads = 1
    session = ort.InferenceSession(str(model_dir / "model_quantized.onnx"), options, providers=["CPUExecutionProvider"])
    key = Path(config["keyFile"]).read_text().strip()
    encoding = tiktoken.get_encoding("o200k_base")
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            if self.headers.get("Authorization") != "Bearer " + key or self.headers.get("Origin"):
                self.send_error(403)
                return
            try:
                size = int(self.headers.get("Content-Length", "0"))
                if size <= 0 or size > 1_048_576:
                    raise ValueError()
                args = json.loads(self.rfile.read(size))
                text = args["text"]
                if not isinstance(text, str):
                    raise ValueError()
                if self.path == "/embed":
                    with lock:
                        prefix = "query: " if args.get("purpose") == "query" else "passage: "
                        tokens = tokenizer.encode(prefix + text)
                        feeds = {"input_ids": np.array([tokens.ids], dtype=np.int64), "attention_mask": np.array([tokens.attention_mask], dtype=np.int64)}
                        if any(item.name == "token_type_ids" for item in session.get_inputs()):
                            feeds["token_type_ids"] = np.array([tokens.type_ids], dtype=np.int64)
                        hidden = session.run(None, feeds)[0]
                        mask = feeds["attention_mask"][..., None]
                        vector = (hidden * mask).sum(axis=1) / mask.sum(axis=1)
                        vector = vector[0] / np.linalg.norm(vector[0])
                    result = {"vector": vector.tolist(), "model": manifest["model"], "revision": manifest["revision"], "tokens": len(tokens.ids), "truncated": len(tokens.overflowing) > 0}
                elif self.path == "/count":
                    result = {"tokens": len(encoding.encode(text, disallowed_special=())), "encoding": "o200k_base", "modelMappingVerified": False}
                else:
                    self.send_error(404)
                    return
                body = json.dumps(result).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except (ValueError, KeyError, TypeError):
                self.send_error(400)
            except (BrokenPipeError, ConnectionResetError):
                pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    connection = Path(config["connectionFile"])
    connection.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary = connection.with_suffix(".tmp")
    with temporary.open("w") as stream:
        os.chmod(temporary, 0o600)
        json.dump({"url": f"http://127.0.0.1:{server.server_port}", "model": manifest["model"], "revision": manifest["revision"]}, stream)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, connection)
    for name in (signal.SIGTERM, signal.SIGINT):
        signal.signal(name, lambda *_args: threading.Thread(target=server.shutdown).start())
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main(sys.argv[1])
