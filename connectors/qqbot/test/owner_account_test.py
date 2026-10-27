"""实际 QQ 宿主验证跨群账号授权、命名空间隔离、热撤销和旧状态恢复。"""
import hashlib
import json
import os
from pathlib import Path
import unittest

from dialogue_tools_test import text_response, tool_response
from management_access_test import ManagementAccessAcceptance, principal

APP = "1904159860"

def account(user, scope="group", app=APP):
    raw = json.dumps([app, scope, user], separators=(",", ":")).encode()
    return "qq-account:" + hashlib.sha256(raw).hexdigest()

class OwnerAccountAcceptance(ManagementAccessAcceptance):
    for _name in dir(ManagementAccessAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def setUp(self):
        super().setUp()
        config = json.loads(self.developer_config.read_text())
        config["operators"] = ["unrelated-administrator"]
        self.developer_config.write_text(json.dumps(config))

    def save(self, **values):
        code, page = self.panel("/api/plugin-pages/read", {"plugin_id":"eve.config", "page_id":"runtime.people"})
        self.assertEqual(code, 200)
        code, result = self.panel("/api/plugin-pages/save", {"plugin_id":"eve.config", "page_id":"runtime.people",
            "instance":page["instance"], "expected_revision":page["revision"], "values":values})
        return code, result

    def bind(self, **values):
        code, _ = self.save(**values)
        self.assertEqual(code, 200)

    def query(self, mid, expected, user="same-user", scope="group", target="group-a"):
        return self.send(self.message(mid, "/admin", user=user, scope=scope, target=target,
                                      contains=expected))

    def test_account_is_cross_group_survives_restart_and_never_merges_chat_history(self):
        captured = []
        def respond(body):
            text = "\n".join(str(m.get("content") or "") for m in body["messages"])
            self.assertIn('"current_is_owner":true', text)
            self.assertIn('"owner":true', text)
            self.assertIn('"role":"owner"', text)
            captured.append(body)
            return text_response("主人已核对")
        self.callback = respond
        def say(mid, text, target):
            return self.send(self.message(mid, text, user="same-user", scope="group", target=target,
                                          expected="主人已核对"))
        self.enabled([*self.checkpoint("bind"),
            *self.query("a", "主人", target="group-a"),
            *self.query("b", "主人", target="group-b"),
            *self.query("other", "普通用户", user="other-user"),
            *self.query("private", "普通用户", scope="c2c", target="same-user"),
            *say("chat-a", "ONLY_GROUP_A_HISTORY", "group-a"),
            *say("chat-b", "ONLY_GROUP_B_HISTORY", "group-b"),
            *self.send(self.message("identity", "/help identity", user="same-user", scope="group", target="group-b",
                contains=[account("same-user"), principal("same-user", "group", "group-b")]))],
            panel=True, checkpoints={"bind":lambda:self.bind(owner_accounts=account("same-user"))})
        self.assertNotIn("ONLY_GROUP_A_HISTORY", json.dumps(captured[-1]))
        self.enabled([*self.query("new-after-restart", "主人", target="never-seen-before"),
                      *self.query("other-after-restart", "普通用户", user="other-user")])
        sessions = self.documents()["eve.session"]["sessions.v1"]["sessions"]
        a = sessions[principal("same-user", "group", "group-a")]
        b = sessions[principal("same-user", "group", "group-b")]
        self.assertIn("ONLY_GROUP_A_HISTORY", json.dumps(a))
        self.assertNotIn("ONLY_GROUP_A_HISTORY", json.dumps(b))

    def test_account_does_not_cross_app_and_revocation_is_immediate(self):
        self.enabled([*self.checkpoint("bind"), *self.query("yes", "主人"),
                      *self.checkpoint("revoke"), *self.query("no", "普通用户", target="group-b")],
            panel=True, checkpoints={"bind":lambda:self.bind(owner_accounts=account("same-user")),
                                     "revoke":lambda:self.bind(owner_accounts="")})
        self.enabled([*self.checkpoint("bind-again"), *self.query("yes-again", "主人")],
            panel=True, checkpoints={"bind-again":lambda:self.bind(owner_accounts=account("same-user"))})
        self.run_eve(self.query("other-app", "普通用户"), interest=False, memory=True, research=False, practice=False,
            env_extra={"EVE_DEVELOPER_CONFIG":str(self.developer_config), "QQBOT_APP_ID":"different-app"})

    @unittest.skipUnless(os.name == "posix" and Path("/usr/bin/bwrap").is_file(), "needs Linux bubblewrap")
    def test_owner_commands_reach_host_and_revoked_account_returns_to_member_sandbox(self):
        marker = self.work / "host-marker.txt"
        marker.write_text("SYNTHETIC_HOST_MARKER")
        def respond(body):
            text = self.text(body)
            results = self.results(body)
            if not results:
                return tool_response([("run_command", {"program":"/usr/bin/python3", "args":["-c",
                    "import json,os;from pathlib import Path;print(json.dumps({'uid':os.getuid(),'host_visible':Path(" + repr(str(marker)) + ").is_file()}))"]})])
            self.assertTrue(results[0]["success"])
            out = json.loads(results[0]["stdout"])
            self.assertEqual(out["host_visible"], text == "owner-probe")
            if text == "owner-probe": self.assertEqual(out["uid"], os.getuid())
            return text_response("执行边界已核对")
        self.callback = respond
        def probe(mid, text):
            return self.send(self.message(mid, text, user="same-user", scope="group", target="new-group", expected="执行边界已核对"))
        self.enabled([*self.checkpoint("bind"), *probe("host", "owner-probe"),
            *self.checkpoint("revoke"), *probe("member", "member-probe")], panel=True,
            checkpoints={"bind":lambda:self.bind(owner_accounts=account("same-user")),
                         "revoke":lambda:self.bind(owner_accounts="")})

    def test_legacy_binding_keeps_original_scope_and_malformed_accounts_do_not_replace_it(self):
        old = principal("same-user", "group", "group-a")
        def invalid():
            for value in ["same-user", "qq-account:bad", "qq-account:" + "A"*64,
                          ",".join("qq-account:" + f"{n:064x}" for n in range(33))]:
                code, _ = self.save(owner_accounts=value)
                self.assertNotEqual(code, 200)
        self.enabled([*self.checkpoint("legacy"), *self.query("legacy-yes", "主人"),
                      *self.query("legacy-other-group", "普通用户", target="group-b"),
                      *self.checkpoint("invalid"), *self.query("legacy-preserved", "主人")],
            panel=True, checkpoints={"legacy":lambda:self.bind(owner_principals=old), "invalid":invalid})

if __name__ == "__main__":
    unittest.main(verbosity=2)
