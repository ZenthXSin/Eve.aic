"""表达学习满额时，真实 QQ 宿主仍可重启回复；原状态与历史不丢失。"""
import hashlib
import json
import unittest

import developer_tools_test as developer
from dialogue_tools_test import text_response


class TrainingCapacityAcceptance(developer.DeveloperAcceptance):
    for _name in dir(developer.DeveloperAcceptance):
        if _name.startswith("test_"):
            locals()[_name] = None
    del _name

    def test_full_learning_state_does_not_block_startup_or_reset_existing_samples(self):
        self.callback = lambda body: text_response("仍可回复")
        self.enabled(self.send(self.message("old", "旧回执需要恢复", expected="仍可回复")))
        old_turn = self.turn("旧回执需要恢复")
        document = json.loads(self.state_path.read_text())
        template = json.loads(bytes(document["entries"]["eve.training"]["expression.v1"]))["rows"][0]
        row_bytes = len(json.dumps(template, separators=(",", ":")).encode())
        overhead = len(b'{"version":1,"rows":[]}')
        count = (1048576 - overhead + 1) // (row_bytes + 1)
        state = {"version":1, "rows":[dict(template,
            id=hashlib.sha256(str(i).encode()).hexdigest()) for i in range(count)]}
        full = json.dumps(state, separators=(",", ":")).encode()
        self.assertLessEqual(len(full), 1048576)
        self.assertGreater(len(full) + row_bytes + 1, 1048576)
        document["entries"]["eve.training"]["expression.v1"] = list(full)
        self.state_path.write_text(json.dumps(document, separators=(",", ":")))
        self.enabled(self.send(self.message("new", "满额后依然可以回复", expected="仍可回复")))
        self.assertEqual(self.turn("旧回执需要恢复"), old_turn)
        after = json.loads(self.state_path.read_text())
        self.assertTrue(bytes(after["entries"]["eve.training"]["expression.v1"]) == full)
        self.assertEqual(self.receipt("old")["state"], "Sent")
        self.assertEqual(self.receipt("new")["state"], "Sent")
        self.assertFalse(self.results(self.requests[-1]["body"]), "没有自动重放旧工具")


if __name__ == "__main__":
    unittest.main(verbosity=2)
