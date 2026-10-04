import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("review", Path(__file__).resolve().parents[1] / "training-review.py")
review = importlib.util.module_from_spec(spec)
spec.loader.exec_module(review)


def answer(index, question="single", act="training_answer", flags=None, dimensions=None):
    return {"index": index, "question_act": question, "user_act": act,
            "flags": flags or [], "preference_dimensions": dimensions or []}


class Reviews(unittest.TestCase):
    def test_chat_subset_complete_output_and_fenced_json_without_explanations(self):
        from unittest.mock import MagicMock
        response = MagicMock()
        response.__enter__.return_value = response
        def payload(content, finish="stop"):
            return json.dumps({"choices": [{"index": 0, "finish_reason": finish,
                "message": {"role": "assistant", "content": content}}]}).encode()
        response.read.return_value = payload('```json\n{"reviews": []}\n```')
        with patch.dict(os.environ, EVE_OPENAI_API_KEY="test-key"), patch.object(review.urllib.request, "urlopen", return_value=response) as opened:
            self.assertEqual(review.judge([]), {"reviews": []})
            request = opened.call_args.args[0]
            body = json.loads(request.data)
            self.assertFalse(body["stream"])
            self.assertNotIn("response_format", body)
            self.assertNotIn("test-key", request.data.decode())
            response.read.return_value = payload('{"reviews": []}', "length")
            with self.assertRaisesRegex(ValueError, "model_finish_length"):
                review.judge([])
            response.read.return_value = payload('说明\n```json\n{"reviews": []}\n```')
            with self.assertRaisesRegex(ValueError, "model_json"):
                review.judge([])
        with self.assertRaises(ValueError):
            review.strict_json('{"reviews": [], "reviews": []}')

    def test_diagnostics_are_fixed_codes_and_never_exception_text(self):
        error = review.urllib.error.HTTPError('https://example.com', 400, 'private text and key', {}, None)
        self.assertEqual(review.error_code(error), "http_400")
        self.assertEqual(review.error_code(ValueError('private text')), "model_protocol")
        evidence = [{"session": "s", "input": "private", "reply": "private"}]
        def fail(_):
            raise error
        result = review.review(evidence, fail)
        self.assertEqual(result["failure_codes"], {"http_400": 1})
        self.assertNotIn('private', json.dumps(result))

    def test_model_cannot_publish_text_identity_or_inferred_preferences(self):
        for item in [dict(answer(0), raw="private"), dict(answer(0), question_act="private"),
                     dict(answer(0), flags=["private"]), answer(0, dimensions=["private"]),
                     answer(True), answer(9)]:
            with self.assertRaises((ValueError, TypeError)):
                review.validate_reviews({"reviews": [item]}, {0})
        for items in [[], [answer(0), answer(0)]]:
            with self.assertRaises(ValueError):
                review.validate_reviews({"reviews": items}, {0})
        self.assertEqual(review.validate_reviews({"reviews": [answer(0, act="preference_feedback", dimensions=["tone"])]}, {0})[0]["preference_dimensions"], ["tone"])

    def test_answer_and_unrelated_dimension_signals_never_become_explicit_preferences(self):
        evidence = [{"session": "s", "input": "one", "reply": "reply"},
                    {"session": "s", "input": "two", "reply": "reply"},
                    {"session": "s", "input": "three", "reply": "reply"}]
        def evaluate(batch):
            return {"reviews": [answer(r["index"], act=["training_answer", "casual_chat", "preference_feedback"][r["index"]], dimensions=["tone"]) for r in batch]}
        result = review.review(evidence, evaluate)
        self.assertEqual(result["explicit_preference_dimensions"], {"tone": 1})
        self.assertEqual(result["answer_preference_candidates"], {"tone": 1})
        self.assertEqual(result["unconfirmed_preference_signals"], {"tone": 1})
        self.assertTrue(result["review_complete"])

    def test_same_session_history_answer_without_keyword_and_no_route_ids(self):
        evidence = [{"session": "private-a", "message_id": "sensitive-id", "input": "问候", "reply": "每段一句合适吗？"},
                    {"session": "private-b", "input": "其他用户", "reply": "你好。"},
                    {"session": "private-a", "input": "可以", "reply": "称呼有什么偏好？"}]
        seen = []
        def evaluate(batch):
            seen.extend(batch)
            return {"reviews": [answer(r["index"]) for r in batch]}
        result = review.review(evidence, evaluate)
        self.assertEqual(seen[1]["history"], [])
        self.assertEqual(seen[2]["history"], [{"input": "问候", "reply": "每段一句合适吗？"}])
        self.assertNotIn("private-a", json.dumps(seen))
        self.assertNotIn("sensitive-id", json.dumps(seen))
        self.assertEqual(result["user_acts"]["training_answer"], 3)
        self.assertNotIn("可以", json.dumps(result, ensure_ascii=False))

    def test_multi_mark_quoted_or_choice_questions_not_equated_to_multiple(self):
        evidence = [{"session": "s", "input": "hi", "reply": "引用：好吗？行吗？"}]
        result = review.review(evidence, lambda _: {"reviews": [answer(0, question="quoted_only")]})
        self.assertEqual(result["multi_mark_candidate_acts"], {"quoted_only": 1})
        self.assertNotIn("multiple", result["question_acts"])

    def test_request_record_size_and_deadline_bounded_no_retries(self):
        evidence = [{"session": "s", "input": "x", "reply": "y"} for _ in range(100)]
        batches, _, selected = review.evidence_batches(evidence)
        self.assertEqual(selected, 64)
        self.assertEqual(len(batches), 32)
        def failing(_):
            raise ValueError("private model error")
        result = review.review(evidence, failing)
        self.assertEqual((result["requests"], result["failed_batches"], result["unreviewed"]), (32, 32, 64))
        self.assertFalse(result["review_complete"])
        ticks = iter([0, 601])
        result = review.review(evidence, lambda _: self.fail("deadline must stop request"), clock=lambda: next(ticks))
        self.assertEqual(result["requests"], 0)
        huge = [{"session": "s", "input": "x" * review.MAX_REQUEST_BYTES, "reply": "y"}]
        result = review.review(huge, lambda _: self.fail("oversized request"))
        self.assertEqual((result["oversized"], result["unreviewed"]), (1, 1))

    def test_encrypted_real_format_recomputed_and_mismatches_rejected(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, QQBOT_APP_SECRET="test-only"):
            base = Path(directory)
            root, source = base / "root", base / "source"
            (root / "state").mkdir(parents=True)
            source.mkdir()
            state = root / "state/state.json"
            state.write_text(json.dumps({"version": 1, "entries": {}}))
            saved = review.window.collect(state)
            saved["window"] = {"source_sha": "a" * 40}
            review.window.write_json(root / "training-report.json", saved)
            review.window.seal(root, source / "training-state.enc")
            public = {"counts": saved["counts"], "window": saved["window"]}
            review.window.write_json(source / "training-summary.json", public)
            self.assertEqual(review.verify_source(source, base / "restored"), saved)
            public["counts"]["model_sent"] = 1
            review.window.write_json(source / "training-summary.json", public)
            with self.assertRaisesRegex(ValueError, "summary_mismatch"):
                review.verify_source(source, base / "mismatch")


if __name__ == "__main__":
    unittest.main()
