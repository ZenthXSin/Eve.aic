"""实际 QQ 宿主的视觉输入：图片经本地 HTTPS 图床真实下载，核对模型请求里收到的就是原图字节。

模型替身从收到的图像里解出 PNG 文本块并回答，证明像素确实送达；像素不写入会话、回执或记忆。
覆盖私聊纯图、图文、群聊、引用图片、未开启、链接过期、类型不符、超过大小、重启后不再附图。
"""
import base64
import http.server
import json
import pathlib
import ssl
import struct
import subprocess
import threading
import unittest
import zlib

import developer_tools_test as developer
from dialogue_tools_test import text_response


def png(secret, padding=0):
    """合成 1x1 PNG，tEXt 块写入口令；padding 用来造超限图片。"""
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    header = struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0)
    text = b"secret\x00" + secret.encode() + b"\x00" * (padding + 1)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"tEXt", text)
            + chunk(b"IDAT", zlib.compress(b"\x00\xff\x00\x00")) + chunk(b"IEND", b""))


def secret_of(data):
    marker = data.index(b"secret\x00") + len(b"secret\x00")
    return data[marker:].split(b"\x00", 1)[0].decode()


def images_in(body):
    found = []
    for message in body["messages"]:
        content = message.get("content")
        if isinstance(content, list):
            for part in content:
                if part.get("type") == "image_url":
                    url = part["image_url"]["url"]
                    prefix, encoded = url.split(",", 1)
                    found.append((prefix, base64.b64decode(encoded)))
    return found


def notes_in(body):
    return "\n".join(m["content"] for m in body["messages"]
                     if m["role"] == "system" and isinstance(m.get("content"), str) and "本条用户消息图片" in m["content"])


class VisionAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        tls = self.work / "tls"
        tls.mkdir()
        run = lambda *args: subprocess.run(["openssl", *args], check=True, capture_output=True, cwd=tls)
        run("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-keyout", "ca.key", "-out", "ca.pem",
            "-subj", "/CN=eve-vision-test-ca", "-addext", "basicConstraints=critical,CA:TRUE",
            "-addext", "keyUsage=critical,keyCertSign")
        run("req", "-newkey", "rsa:2048", "-nodes", "-keyout", "leaf.key", "-out", "leaf.csr", "-subj", "/CN=localhost")
        (tls / "leaf.ext").write_text("subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\n"
                                      "extendedKeyUsage=serverAuth\n")
        run("x509", "-req", "-in", "leaf.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
            "-days", "2", "-out", "leaf.pem", "-extfile", "leaf.ext")
        self.ca = tls / "ca.pem"
        self.files = {}
        self.fetches = []
        outer = self

        class Images(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                outer.fetches.append(self.path)
                entry = outer.files.get(self.path.split("?")[0])
                if entry is None:
                    self.send_error(404)
                    return
                kind, data = entry
                self.send_response(200)
                self.send_header("Content-Type", kind)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.images = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Images)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(tls / "leaf.pem", tls / "leaf.key")
        self.images.socket = context.wrap_socket(self.images.socket, server_side=True)
        self.images.daemon_threads = True
        threading.Thread(target=self.images.serve_forever, daemon=True).start()

    def tearDown(self):
        self.images.shutdown()
        self.images.server_close()
        super().tearDown()

    def url(self, path):
        return f"https://localhost:{self.images.server_address[1]}{path}?sig=synthetic"

    def vision(self, script, enabled=True, **env):
        extra = {"EVE_DEVELOPER_CONFIG": str(self.developer_config), "SSL_CERT_FILE": str(self.ca)}
        if enabled:
            extra["EVE_VISION_ENABLED"] = "true"
        extra.update(env)
        self.run_eve(script, interest=False, memory=True, research=False, practice=False, env_extra=extra)

    def image_message(self, id, text, paths, expected, **route):
        return {**self.message(id, text, expected=expected, **route),
                "attachments": [{"content_type": "image/png", "url": self.url(path)} for path in paths]}

    def answer(self, body):
        found = images_in(body)
        if found:
            return text_response("看到：" + "、".join(secret_of(data) for _, data in found))
        notes = notes_in(body)
        return text_response("没有图像" + ("（有说明）" if notes else ""))

    def stored_text(self):
        return json.dumps(self.documents(), ensure_ascii=False)

    def test_private_group_and_quoted_images_reach_the_model_as_real_pixels_without_being_stored(self):
        self.files = {"/a.png": ("image/png", png("红色方块")), "/b.png": ("image/png", png("蓝色圆形")),
                      "/q.png": ("image/png", png("引用里的星星"))}
        self.callback = self.answer
        quoted = {**self.message("quote", "这张呢", expected="看到：引用里的星星"),
                  "quoted": {"text": "之前那张", "attachments": [{"content_type": "image/png", "url": self.url("/q.png")}]}}
        self.vision([
            *self.send(self.image_message("only", "", ["/a.png"], "看到：红色方块")),
            *self.send(self.image_message("both", "两张图分别是什么", ["/a.png", "/b.png"], "看到：红色方块、蓝色圆形")),
            *self.send(self.image_message("group", "群里这张", ["/b.png"], "看到：蓝色圆形", scope="group")),
            *self.send(quoted),
            *self.send(self.message("text", "刚才说到哪了", expected="没有图像"))])
        bodies = [r["body"] for r in self.requests if r["kind"] == "chat"]
        first = next(b for b in bodies if images_in(b))
        [(prefix, data)] = images_in(first)
        self.assertEqual(prefix, "data:image/png;base64")
        self.assertEqual(data, self.files["/a.png"][1])
        self.assertIn("已把第 1 张图片按顺序作为图像附在这条用户消息上", notes_in(first))
        # 图像只出现在当轮最后一条用户消息上，后续文字轮的历史里没有像素。
        last = bodies[-1]
        self.assertEqual(images_in(last), [])
        self.assertNotIn("base64", json.dumps(last))
        stored = self.stored_text()
        self.assertNotIn("base64", stored)
        self.assertNotIn(base64.b64encode(self.files["/a.png"][1]).decode(), stored)
        self.assertNotIn("红色方块", json.dumps(self.receipt("only")["message"], ensure_ascii=False))

    def test_failures_are_reported_per_image_and_disabled_vision_sends_no_pixels(self):
        self.files = {"/ok.png": ("image/png", png("可用")), "/text.png": ("image/png", b"not an image at all"),
                      "/big.png": ("image/png", png("太大", padding=70 * 1024))}
        self.callback = self.answer
        self.vision([*self.send(self.image_message("mixed", "看看这些", ["/ok.png", "/gone.png", "/text.png", "/big.png"],
                                                   "看到：可用"))],
                    EVE_VISION_MAX_IMAGE_BYTES=str(64 * 1024))
        body = next(r["body"] for r in self.requests if r["kind"] == "chat")
        notes = notes_in(body)
        self.assertIn("第 2 张图片：下载被拒绝（HTTP 404）", notes)
        self.assertIn("第 3 张图片：不是 PNG、JPEG、GIF 或 WebP 图片", notes)
        self.assertIn("第 4 张图片：超过 65536 字节上限", notes)
        self.assertIn("不能声称看见或推测这张图片的内容", notes)
        self.assertEqual(len(images_in(body)), 1)

        # 未开启视觉时不下载、不附图，保留通道原有的如实说明。
        self.requests.clear()
        fetched = len(self.fetches)
        self.vision([*self.send(self.image_message("off", "", ["/ok.png"], "没有图像"))], enabled=False)
        body = next(r["body"] for r in self.requests if r["kind"] == "chat")
        self.assertEqual(images_in(body), [])
        self.assertIn("尚未获取像素或进行视觉解析", body["messages"][-1]["content"])
        self.assertEqual(len(self.fetches), fetched)

    def test_restart_does_not_replay_images_and_history_keeps_only_text(self):
        self.files = {"/a.png": ("image/png", png("重启前"))}
        self.callback = self.answer
        self.vision([*self.send(self.image_message("before", "记住这张", ["/a.png"], "看到：重启前"))])
        fetched = len(self.fetches)
        self.vision([*self.send(self.message("after", "重启后还能看到吗", expected="没有图像"))])
        body = [r["body"] for r in self.requests if r["kind"] == "chat"][-1]
        self.assertEqual(images_in(body), [])
        self.assertEqual(len(self.fetches), fetched)
        users = [m["content"] for m in body["messages"] if m["role"] == "user"]
        self.assertTrue(any("记住这张" in u and "收到 1 个图片附件" in u for u in users))


if __name__ == "__main__":
    unittest.main()
