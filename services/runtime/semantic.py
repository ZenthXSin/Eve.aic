"""Local quantized E5 search service. Indexing never runs in Eve's model/tool path."""
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

from semantic_index import Index


class Encoder:
    def __init__(self, model_dir):
        model_dir = Path(model_dir)
        manifest = json.loads((model_dir / "manifest.json").read_text())
        for name, saved in manifest["files"].items():
            with (model_dir / name).open("rb") as stream:
                if hashlib.file_digest(stream, "sha256").hexdigest() != saved["sha256"]:
                    raise RuntimeError("Embedding model checksum mismatch")
        self.model, self.revision, self.dimensions = manifest["model"], manifest["revision"], 384
        self.tokenizer = Tokenizer.from_file(str(model_dir / "tokenizer.json"))
        self.tokenizer.no_truncation()
        options = ort.SessionOptions()
        options.intra_op_num_threads = 1
        options.inter_op_num_threads = 1
        options.add_session_config_entry("session.intra_op.allow_spinning", "0")
        options.add_session_config_entry("session.inter_op.allow_spinning", "0")
        self.session = ort.InferenceSession(str(model_dir / "model_quantized.onnx"), options,
                                            providers=["CPUExecutionProvider"])
        self.lock = threading.Lock()
        self.cache = {}

    def chunk(self, text):
        tokens = self.tokenizer.encode(text, add_special_tokens=False)
        if len(tokens.ids) <= 380:
            return text
        end = tokens.offsets[379][1]
        return text[:max(1, end)]

    def embed(self, text, purpose):
        cache_key = (purpose, text)
        with self.lock:
            if cache_key in self.cache:
                return self.cache[cache_key]
            prefix = "query: " if purpose == "query" else "passage: "
            tokens = self.tokenizer.encode(prefix + text)
            if len(tokens.ids) > 512:
                raise ValueError("Embedding input exceeds 512 tokens; split the source")
            feeds = {"input_ids": np.array([tokens.ids], dtype=np.int64),
                     "attention_mask": np.array([tokens.attention_mask], dtype=np.int64)}
            if any(x.name == "token_type_ids" for x in self.session.get_inputs()):
                feeds["token_type_ids"] = np.array([tokens.type_ids], dtype=np.int64)
            hidden = self.session.run(None, feeds)[0]
            mask = feeds["attention_mask"][..., None]
            vector = (hidden * mask).sum(axis=1) / mask.sum(axis=1)
            vector = vector[0] / np.linalg.norm(vector[0])
            encoded = vector.astype("<f4").tobytes()
            if purpose == "query":
                if len(self.cache) >= 64:
                    self.cache.pop(next(iter(self.cache)))
                self.cache[cache_key] = encoded
            return encoded

    def scores(self, query, rows):
        matrix = np.vstack([np.frombuffer(r[1], dtype="<f4") for r in rows])
        result = matrix @ np.frombuffer(query, dtype="<f4")
        return [(row[0], float(score)) for row, score in zip(rows, result)]


def main(config_path):
    os.umask(0o077)
    config = json.loads(Path(config_path).read_text())
    encoder = Encoder(config["modelDirectory"])
    index = Index(config["indexDirectory"], config["backendDirectory"], config.get("phoneDirectory"), encoder)
    key = Path(config["keyFile"]).read_text().strip()
    stopping, searching = threading.Event(), threading.Event()
    requests_lock = threading.Lock()

    def background():
        while not stopping.is_set():
            try:
                index.discover(128)
                for _ in range(12):
                    if searching.is_set() or stopping.is_set() or not index.prepare():
                        break
                if not searching.is_set() and not stopping.is_set():
                    worked = index.embed_one()
                else:
                    worked = False
                stopping.wait(config.get("indexDelaySeconds", 0.3) if worked else 0.1)
            except Exception as e:
                index.last_error = {"error": type(e).__name__}
                stopping.wait(3)

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            if self.headers.get("Authorization") != "Bearer " + key or self.headers.get("Origin"):
                self.send_error(403)
                return
            try:
                size = int(self.headers.get("Content-Length", "0"))
                if not 0 < size <= 65536:
                    raise ValueError("Invalid request size")
                args = json.loads(self.rfile.read(size))
                if self.path == "/status":
                    result = index.status()
                elif self.path == "/search":
                    with requests_lock:
                        searching.set()
                        try:
                            result = index.search(args)
                        finally:
                            searching.clear()
                else:
                    self.send_error(404)
                    return
                body = json.dumps(result, ensure_ascii=False).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except (ValueError, KeyError, TypeError):
                self.send_error(400)
            except (BrokenPipeError, ConnectionResetError):
                pass
            except Exception as e:
                index.last_error = {"error": type(e).__name__}
                self.send_error(503)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    connection = Path(config["connectionFile"])
    connection.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary = connection.with_suffix(".tmp")
    with temporary.open("w") as stream:
        json.dump({"url": f"http://127.0.0.1:{server.server_port}", "model": encoder.model,
                   "revision": encoder.revision, "dimensions": encoder.dimensions}, stream)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, connection)
    worker = threading.Thread(target=background, name="semantic-index", daemon=True)
    worker.start()

    def stop(*_args):
        stopping.set()
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        server.serve_forever()
    finally:
        stopping.set()
        worker.join(timeout=30)
        server.server_close()


if __name__ == "__main__":
    main(sys.argv[1])
