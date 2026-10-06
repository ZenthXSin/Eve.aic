"""评估入口、结果脱敏和完成门控的边界检查；不调用真实模型。"""

import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("cognition_evaluate", Path(__file__).with_name("evaluate.py"))
evaluate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evaluate)


class EvaluationEntryTests(unittest.TestCase):
    def parse(self, *extra):
        return evaluate.argument_parser().parse_args([
            "--binary", "fixture-binary", "--output", "fixture-report.json", *extra,
        ])

    def test_default_runs_all_registered_scenarios_with_dimensions(self):
        args = self.parse()
        selected = evaluate.selected_scenarios(args.scenario)
        self.assertEqual(selected, evaluate.SCENARIOS)
        names = [name for name, _ in selected]
        self.assertEqual(len(names), len(set(names)))
        self.assertTrue(all(evaluate.DIMENSIONS[name] for name in names))
        self.assertTrue({
            "file_changes_replan_and_restart_deduplicates",
            "file_observation_preserves_user_constraints",
            "file_sources_and_owner_isolation",
        }.issubset(names))

    def test_repeatable_selection_preserves_order_without_repeating_processes(self):
        first = "file_sources_and_owner_isolation"
        second = "file_observation_preserves_user_constraints"
        args = self.parse("--scenario", first, "--scenario", second, "--scenario", first)
        self.assertEqual([name for name, _ in evaluate.selected_scenarios(args.scenario)],
                         [first, second])

    def test_unknown_scenario_is_rejected_by_argument_parser(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as failure:
            self.parse("--scenario", "not-a-registered-scenario")
        self.assertEqual(failure.exception.code, 2)

    def test_declared_revision_accepts_only_normalized_commit_digests(self):
        for source, normalized in [
            ("", ""), ("ABCDEF0", "abcdef0"), ("A" * 40, "a" * 40), ("B" * 64, "b" * 64),
        ]:
            with self.subTest(source=source):
                self.assertEqual(self.parse("--source-sha", source).source_sha, normalized)
        for source in ["f" * 6, "f" * 65, "../private/source", "main", "abcdefg", "abcdef0\n"]:
            with self.subTest(source=source):
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as failure:
                    self.parse("--source-sha", source)
                self.assertEqual(failure.exception.code, 2)

    def test_prompt_lookup_finds_real_envelope_in_nested_http_body(self):
        envelope = {"unverified_waiting_input": {"parent_id": "fixture"},
                    "untrusted_file_observation": {"text_excerpt": "中文事实"}}
        body = {"input": [{"content": [{"text": "说明\n" + json.dumps(envelope)}]}]}
        self.assertEqual(evaluate.prompt_data(body), envelope)
        self.assertIsNone(evaluate.prompt_data({"input": [{"text": '{"unrelated":true}'}]}))


class EvaluationReportTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="private-cognition-harness-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / "private-binary-fixture"
        self.binary.write_bytes(b"public harness executable fixture\n")
        self.binary.chmod(0o700)
        self.output = self.root / "private-output" / "report.json"
        self.cases = []
        cases = self.cases

        class FakeScenario:
            def __init__(self, binary, root):
                self.binary = binary
                self.root = root
                self.provider = SimpleNamespace(requests=[], errors=[])
                self.processes = 0
                self.runs = []
                self.observations = {}
                self.closed = False
                cases.append(self)

            def close(self):
                self.closed = True

        self.scenario_factory = FakeScenario

    def run_harness(self, scenarios, *extra):
        stdout = io.StringIO()
        with mock.patch.object(evaluate, "Scenario", self.scenario_factory), \
                mock.patch.object(evaluate, "SCENARIOS", scenarios), \
                contextlib.redirect_stdout(stdout):
            exit_code = evaluate.main([
                "--binary", str(self.binary), "--output", str(self.output), *extra,
            ])
        return exit_code, json.loads(self.output.read_text(encoding="utf-8")), stdout.getvalue()

    def test_filtered_report_counts_selected_scenarios_and_has_no_local_paths(self):
        executed = []

        def selected(case):
            executed.append("selected")
            case.processes = 2
            case.observations["public_check"] = True

        def excluded(_case):
            self.fail("未选中的场景不能执行")

        selected_name = "file_sources_and_owner_isolation"
        code, report, stdout = self.run_harness([
            ("durable_reflection", excluded), (selected_name, selected),
        ], "--scenario", selected_name, "--scenario", selected_name, "--source-sha", "ABCDEF0")
        self.assertEqual(code, 0)
        self.assertEqual(executed, ["selected"])
        self.assertEqual(report["format_version"], 2)
        self.assertEqual(report["selected_scenarios"], [selected_name])
        self.assertEqual(report["declared_source_sha"], "abcdef0")
        self.assertEqual(report["binary_sha256"], hashlib.sha256(self.binary.read_bytes()).hexdigest())
        self.assertEqual(report["summary"]["scenarios"], 1)
        self.assertEqual(report["summary"]["processes"], 2)
        self.assertTrue(report["functional_ok"])
        self.assertFalse(report["real_model_evaluated"])
        self.assertFalse(report["semantic_quality_evaluated"])
        self.assertEqual(report["scenarios"][0]["dimensions"], evaluate.DIMENSIONS[selected_name])
        serialized = json.dumps(report) + stdout
        self.assertNotIn(str(self.root), serialized)
        self.assertNotIn(self.binary.name, serialized)
        self.assertTrue(all(case.closed for case in self.cases))

    def test_unexpected_exception_reports_type_without_sensitive_message_and_continues(self):
        private_message = str(self.root / "private-source.txt") + " PRIVATE_FILE_CONTENT PRIVATE_KEY"

        def failed(_case):
            raise RuntimeError(private_message)

        def later(case):
            case.observations["later_scenario_executed"] = True

        code, report, stdout = self.run_harness([
            ("durable_reflection", failed), ("idle_without_credentials", later),
        ])
        self.assertEqual(code, 1)
        self.assertFalse(report["functional_ok"])
        self.assertEqual(report["summary"]["failed"], 1)
        self.assertEqual(report["summary"]["passed"], 1)
        first = report["scenarios"][0]
        self.assertEqual(first["failed_check"], "harness-or-report-error")
        self.assertEqual(first["error_type"], "RuntimeError")
        self.assertTrue(report["scenarios"][1]["observations"]["later_scenario_executed"])
        serialized = json.dumps(report) + stdout
        for sensitive in [str(self.root), "private-source.txt", "PRIVATE_FILE_CONTENT", "PRIVATE_KEY"]:
            self.assertNotIn(sensitive, serialized)
        self.assertTrue(all(case.closed for case in self.cases))

    def test_expected_failure_preserves_only_public_check_identifier(self):
        def failed(_case):
            raise evaluate.EvaluationFailure("observation:actual-file-text")

        code, report, _stdout = self.run_harness([("durable_reflection", failed)])
        self.assertEqual(code, 1)
        self.assertEqual(report["scenarios"][0]["failed_check"], "observation:actual-file-text")
        self.assertNotIn("error_type", report["scenarios"][0])
        self.assertTrue(self.cases[0].closed)

    def test_missing_binary_rejected_before_scenario_creation(self):
        self.binary.unlink()
        with mock.patch.object(evaluate, "Scenario") as constructor, \
                contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as failure:
            evaluate.main(["--binary", str(self.binary), "--output", str(self.output)])
        self.assertEqual(failure.exception.code, 2)
        constructor.assert_not_called()
        self.assertFalse(self.output.exists())


class CompletionGateTests(unittest.TestCase):
    def snapshot(self, status, revision):
        return {"state": {"goals": {
            "parent": {"source": {"kind": "User", "reference": "input"}, "revision": revision},
            "child": {"id": "child", "source": {"kind": "Inference", "reference": "parent"},
                      "status": status, "feedback": {"verification_met": status == "Completed"}},
        }}}

    def test_gate_waits_for_persisted_verified_completion_at_requested_revision(self):
        process = mock.Mock()
        process.poll.return_value = None
        snapshots = [self.snapshot("Executing", 2), self.snapshot("Completed", 2),
                     self.snapshot("Completed", 3)]
        case = SimpleNamespace(snapshot=mock.Mock(side_effect=snapshots))
        with mock.patch.object(evaluate.time, "sleep") as poll_interval:
            final = evaluate.Scenario.wait_completed(case, process, "parent", 1, 3)
        self.assertEqual(final["state"]["goals"]["parent"]["revision"], 3)
        self.assertEqual(case.snapshot.call_count, 3)
        self.assertEqual(poll_interval.call_count, 2)

    def test_gate_rejects_process_exit_instead_of_accepting_preexisting_completion(self):
        process = mock.Mock()
        process.poll.return_value = 0
        case = SimpleNamespace(snapshot=mock.Mock(return_value=self.snapshot("Completed", 2)))
        with self.assertRaisesRegex(evaluate.EvaluationFailure, "process-alive-until-durable-completion"):
            evaluate.Scenario.wait_completed(case, process, "parent", 1, 2)
        case.snapshot.assert_not_called()


if __name__ == "__main__":
    unittest.main()
