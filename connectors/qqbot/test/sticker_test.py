"""实际 QQ 宿主的表情包：从用户直接发来的图片收集候选，视觉模型审看后收下或删除，
对话中 send_sticker 按心情挑一张，回复文字发完后作为图片跟着发出（回执格式 5）。

图片经本地 HTTPS 图床真实下载；模型与 QQ 都是本地替身。验证引用里的图片不收、
不是表情包的图片字节被删除、没有对应心情时不发、发送失败不重发、重启不补发，
以及不开 --stickers 时不收集、不注册工具。
"""
import json
import time
import unittest

import vision_test as vision
from dialogue_tools_test import text_response, tool_response

REVIEW_MARK = "适合以后在聊天里发的表情包"


class StickerAcceptance(vision.VisionAcceptance):
    for _name in dir(vision.VisionAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        self.reviews = []
        self.verdicts = {}
        self.chat = lambda body: text_response("好")

    def respond(self, body):
        system = "\n".join(str(m.get("content", "")) for m in body["messages"] if m["role"] == "system")
        if REVIEW_MARK in system:
            self.assertEqual(body.get("response_format"), {"type": "json_object"})
            self.assertIn('"sticker"', body["messages"][-1]["content"][0]["text"])
            [(_, data)] = vision.images_in(body)
            secret = vision.secret_of(data)
            self.reviews.append(secret)
            return text_response(json.dumps(self.verdicts[secret], ensure_ascii=False))
        return self.chat(body)

    def stickers(self, script, enabled=True, **options):
        env = {"EVE_DEVELOPER_CONFIG": str(self.developer_config), "SSL_CERT_FILE": str(self.ca),
               "EVE_VISION_ENABLED": "true", **options.pop("env", {})}
        self.callback = self.respond
        self.run_eve(script, interest=False, memory=True, research=False, practice=False, env_extra=env,
                     more=["--stickers"] if enabled else [], timeout=60, **options)

    def index(self):
        path = self.work / "state" / "stickers" / "index.json"
        return json.loads(path.read_text()) if path.exists() else None

    def wait_for(self, predicate, message, timeout=20):
        deadline = time.monotonic() + timeout
        while not predicate():
            self.assertLess(time.monotonic(), deadline, message)
            time.sleep(0.05)

    def receipt(self, id):
        entries = self.documents()["eve.channel.qqbot"]["receipts.v1"]["entries"]
        return next(e for e in entries if e["message"]["id"] == id)

    def test_reviewed_sticker_follows_the_reply_and_is_never_resent(self):
        self.files = {"/cat.png": ("image/png", vision.png("猫猫点头")),
                      "/photo.png": ("image/png", vision.png("自拍照片")),
                      "/quoted.png": ("image/png", vision.png("引用里的图"))}
        self.verdicts = {"猫猫点头": {"sticker": True, "description": "猫猫点头表示认同", "moods": ["赞同", "开心"]},
                         "自拍照片": {"sticker": False, "description": "", "moods": []}}
        quoted = {**self.message("quote", "这张呢", expected="好"),
                  "quoted": {"text": "之前", "attachments": [{"content_type": "image/png", "url": self.url("/quoted.png")}]}}

        def reviewed():
            self.wait_for(lambda: (self.index() or {}).get("stickers") and
                          all(s["state"] != "pending" for s in self.index()["stickers"]), "表情包没有审看完")
        self.stickers([*self.send(self.image_message("cat", "", ["/cat.png"], "好")),
                       *self.send(self.image_message("photo", "我的照片", ["/photo.png"], "好")),
                       *self.send(quoted), *self.checkpoint("reviewed")],
                      checkpoints={"reviewed": reviewed})
        stickers = {s["description"] or s["state"]: s for s in self.index()["stickers"]}
        self.assertEqual(sorted(self.reviews), ["猫猫点头", "自拍照片"], "引用里的图片不收")
        self.assertEqual(stickers["猫猫点头表示认同"]["state"], "approved")
        rejected = next(s for s in self.index()["stickers"] if s["state"] == "rejected")
        blobs = self.work / "state" / "stickers" / "blobs"
        self.assertFalse((blobs / f"{rejected['sha256']}.png").exists(), "不是表情包的图片字节已删除")
        self.assertTrue((blobs / f"{stickers['猫猫点头表示认同']['sha256']}.png").exists())

        def status(body):
            if not self.results(body):
                return tool_response([("send_sticker", {"action":"status"})])
            [saved] = self.results(body)
            self.assertTrue(saved["collect_enabled"] and saved["send_enabled"])
            self.assertEqual((saved["approved"], saved["pending"], saved["rejected"]), (1, 0, 1))
            return text_response("库状态已核实")
        self.chat = status
        self.stickers(self.send(self.message("status", "看看表情库", expected="库状态已核实")))
        self.assertNotIn("sticker", self.receipt("status"), "查询状态不能发送图片")

        def chat(body):
            results = self.results(body)
            latest = body["messages"][-1]
            if not results and latest.get("content") in ("哈哈太好了", "我好难过"):
                return tool_response([("send_sticker", {"mood": "开心" if latest["content"] == "哈哈太好了" else "难过"})])
            if results:
                return text_response("是吧！" if results[0]["chosen"] else "抱抱你")
            return text_response("好")
        self.chat = chat
        happy = {**self.message("happy", "哈哈太好了", expected="是吧！")}
        sad = {**self.message("sad", "我好难过", expected="抱抱你"), "expected_sticker": False}
        self.stickers([*self.send(happy), {"wait_command": {"id": "happy", "type": "sticker"}},
                       *self.checkpoint("sent"), *self.send(sad)],
                      checkpoints={"sent": lambda: self.wait_for(
                          lambda: self.receipt("happy").get("sticker", {}).get("state") == "Sent", "表情包没有发出")})
        receipt = self.receipt("happy")
        self.assertEqual(receipt["state"], "Sent")
        self.assertTrue(receipt["sticker"]["path"].endswith(".png"))
        self.assertEqual(self.documents()["eve.channel.qqbot"]["receipts.v1"]["version"], 5)
        self.assertNotIn("sticker", self.receipt("sad"))
        self.assertEqual(stickers["猫猫点头表示认同"]["uses"] + 1,
                         next(s for s in self.index()["stickers"] if s["state"] == "approved")["uses"])
        # 重启后不补发、不重新审看。
        reviews = len(self.reviews)
        self.chat = lambda body: text_response("好")
        self.stickers(self.send(self.message("after", "在吗", expected="好")))
        self.assertEqual(len(self.reviews), reviews)
        events = [json.loads(line) for line in (self.work / f"events-{self.runs}.jsonl").read_text().splitlines()]
        self.assertFalse([e for e in events if e.get("type") == "sticker"])

    def test_failed_sticker_is_not_retried_and_disabled_flag_collects_nothing(self):
        self.files = {"/cat.png": ("image/png", vision.png("猫猫"))}
        self.verdicts = {"猫猫": {"sticker": True, "description": "开心的猫", "moods": ["开心"]}}
        self.stickers([*self.send(self.image_message("cat", "", ["/cat.png"], "好")), *self.checkpoint("reviewed")],
                      checkpoints={"reviewed": lambda: self.wait_for(
                          lambda: any(s["state"] == "approved" for s in (self.index() or {}).get("stickers", [])),
                          "表情包没有审看完")})

        def chat(body):
            if self.results(body):
                return text_response("嘿嘿")
            return tool_response([("send_sticker", {"mood": "开心"})])
        self.chat = chat
        failing = {**self.message("fail", "开心吗", expected="嘿嘿"), "fail_sticker": True}
        self.stickers([*self.send(failing), {"wait_command": {"id": "fail", "type": "sticker"}},
                       *self.checkpoint("failed")],
                      checkpoints={"failed": lambda: self.wait_for(
                          lambda: self.receipt("fail").get("sticker", {}).get("state") == "Failed", "失败没有记录")})
        self.assertEqual(self.receipt("fail")["state"], "Sent", "表情包失败不影响已送达的文字")
        # 不开 --stickers：不注册工具、不收集新图。
        before = self.index()
        self.files["/new.png"] = ("image/png", vision.png("新图"))

        def plain(body):
            self.assertNotIn("send_sticker", self.definitions(body))
            return text_response("好")
        self.chat = plain
        self.stickers(self.send(self.image_message("new", "", ["/new.png"], "好")), enabled=False)
        self.assertEqual(self.index(), before)


if __name__ == "__main__":
    unittest.main()
