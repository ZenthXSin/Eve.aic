import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("replacement", Path(__file__).with_name("qqbot-replace-run.py"))
replacement = importlib.util.module_from_spec(spec)
spec.loader.exec_module(replacement)


class Replacement(unittest.TestCase):
    def test_only_explicit_main_training_is_cancelled_and_waited(self):
        calls = []
        responses = ["in_progress", "in_progress", "completed"]
        def request(method, path):
            calls.append((method, path))
            if method == "POST":
                return {}
            return {"path": replacement.WORKFLOW, "head_branch": "main", "status": responses.pop(0), "conclusion": "cancelled"}
        result = replacement.replace_run(request, 123, 456, sleep=lambda _: None)
        self.assertEqual(result["status"], "completed")
        self.assertEqual(calls, [("GET", "/actions/runs/123"), ("POST", "/actions/runs/123/cancel"),
                                 ("GET", "/actions/runs/123"), ("GET", "/actions/runs/123")])

    def test_foreign_workflow_branch_or_self_is_never_cancelled(self):
        for path, branch in [(".github/workflows/ci.yml", "main"), (replacement.WORKFLOW, "codex/other")]:
            calls = []
            def request(method, target):
                calls.append(method)
                return {"path": path, "head_branch": branch, "status": "in_progress"}
            with self.assertRaises(ValueError):
                replacement.replace_run(request, 123, 456)
            self.assertEqual(calls, ["GET"])
        with self.assertRaises(ValueError):
            replacement.replace_run(lambda *_: self.fail("must not request API"), 123, 123)

    def test_completed_run_is_not_cancelled_and_timeout_refuses_new_window(self):
        methods = []
        def completed(method, _):
            methods.append(method)
            return {"path": replacement.WORKFLOW, "head_branch": "main", "status": "completed"}
        self.assertEqual(replacement.replace_run(completed, 123, 456)["status"], "already_completed")
        self.assertEqual(methods, ["GET"])
        def running(*_):
            return {"path": replacement.WORKFLOW, "head_branch": "main", "status": "in_progress"}
        ticks = iter([0, 1])
        with self.assertRaisesRegex(ValueError, "not_stopped"):
            replacement.replace_run(running, 123, 456, timeout=1, clock=lambda: next(ticks))


if __name__ == "__main__":
    unittest.main()
