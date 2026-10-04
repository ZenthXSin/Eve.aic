import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("compare", Path(__file__).resolve().parents[1] / "training-compare.py")
compare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare)


def evidence(count=16):
    return [{"session": "s-" + str(i % 2), "input": "private-input-" + str(i),
             "reply": "one？two？" if i == 5 else "old-reply-" + str(i)} for i in range(count)]


def evaluator(records):
    return {"reviews": [{"index": r["index"], "question_act": "multiple" if r["index"] % 2 == 0 else "single",
                         "user_act": "training_answer", "flags": [], "preference_dimensions": []} for r in records]}


class Comparisons(unittest.TestCase):
    def test_public_cli_never_reads_private_source_secret_or_persists_generated_text(self):
        result = {"complete": True, "advisory_non_regression": True}
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "summary.json"
            with patch.dict(os.environ, {}, clear=True), \
                 patch("sys.argv", ["training-compare.py", "--synthetic", "--output", str(output)]), \
                 patch.object(compare.review, "verify_source", side_effect=AssertionError("private source read")), \
                 patch.object(compare.review.window, "seal", side_effect=AssertionError("QQ secret read")), \
                 patch.object(compare, "compare", return_value=(result, [{"reply": "unpublished generated text"}])) as run, \
                 contextlib.redirect_stdout(io.StringIO()) as printed:
                compare.main()
            report = json.loads(output.read_text())
            self.assertEqual(report["evidence_kind"], "public_synthetic")
            self.assertEqual(len(run.call_args.args[0]), 12)
            self.assertEqual(len(report["candidate_prompt_sha256"]), 64)
            self.assertEqual(len(report["fixture_sha256"]), 64)
            self.assertNotIn("unpublished generated text", output.read_text() + printed.getvalue())
            self.assertEqual(list(Path(directory).iterdir()), [output])

    def test_candidate_pairs_same_real_history_bounded_and_no_private_output(self):
        seen = []
        def generate(prompt, identity, record, timeout):
            seen.append((prompt, identity, record, timeout))
            return "baseline？extra？" if prompt == "old" else "candidate？"
        result, rows = compare.compare(evidence(), "old", "new", "same-identity", generate, evaluator)
        self.assertTrue(result["complete"])
        self.assertTrue(result["advisory_non_regression"])
        self.assertTrue(result["advisory_improved"])
        self.assertEqual(result["selected_pairs"], 12)
        self.assertEqual(result["model_requests"], 36)
        for i in range(0, len(seen), 2):
            self.assertEqual(seen[i][1:3], seen[i + 1][1:3])
            self.assertLessEqual(seen[i][3], 30)
        self.assertNotIn("private-input", json.dumps(result))
        self.assertNotIn("candidate？", json.dumps(result, ensure_ascii=False))
        self.assertIn("private-input-5", [r["input"] for r in rows])
        for row in rows:
            for previous in row["history"]:
                self.assertEqual(int(previous["input"].split('-')[-1]) % 2, int(row["input"].split('-')[-1]) % 2)

    def test_failed_generation_or_invalid_review_never_passes(self):
        def failing(*_, **__):
            raise ValueError("private error")
        result, _ = compare.compare(evidence(1), "old", "new", "identity", failing, evaluator)
        self.assertFalse(result["complete"])
        self.assertFalse(result["advisory_non_regression"])
        self.assertEqual(result["model_requests"], 1)
        self.assertNotIn("private error", json.dumps(result))
        result, _ = compare.compare(evidence(1), "old", "new", "identity", lambda *a, **k: "reply", lambda _: {"raw": "private"})
        self.assertFalse(result["advisory_non_regression"])
        self.assertEqual(result["reviewed_pairs"], 0)

    def test_regression_empty_evidence_and_deadline_refuse_pass(self):
        def worse(records):
            result = evaluator(records)
            for item in result["reviews"]:
                item["question_act"] = "single"
                item["flags"] = ["unnecessary_followup"] if item["index"] % 2 else []
            return result
        result, _ = compare.compare(evidence(1), "old", "new", "identity", lambda *a, **k: "reply", worse)
        self.assertTrue(result["complete"])
        self.assertFalse(result["advisory_non_regression"])
        def uncertain(records):
            result = evaluator(records)
            for item in result["reviews"]:
                item["question_act"] = "uncertain"
            return result
        result, _ = compare.compare(evidence(1), "old", "new", "identity", lambda *a, **k: "reply", uncertain)
        self.assertFalse(result["advisory_non_regression"])
        self.assertFalse(result["question_assessment_resolved"])
        result, _ = compare.compare([], "old", "new", "identity")
        self.assertFalse(result["complete"])
        self.assertEqual(result["model_requests"], 0)
        ticks = iter([0, 601])
        result, _ = compare.compare(evidence(1), "old", "new", "identity", clock=lambda: next(ticks))
        self.assertEqual(result["model_requests"], 0)
        self.assertFalse(result["complete"])


if __name__ == "__main__":
    unittest.main()
