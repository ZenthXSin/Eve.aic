"""离线分析器的合成验收：不联网、不运行宿主、不使用真实模型。"""
import copy
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SCRIPT = Path(__file__).with_name("analyze-model-prefix.py")
SPEC = importlib.util.spec_from_file_location("model_prefix_analysis", SCRIPT)
analysis = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(analysis)
SECRET = "fixture-private-secret-do-not-output"
NONCE = "a" * 32


def sha(value):
    return hashlib.sha256(value.encode()).hexdigest()


def identifier(label):
    return {"status": "present", "bytes": len(label), "sha256": sha(label)}


def summary(label, size=40):
    return {"hmac_sha256": sha(label), "bytes": size}


def base(request=1, call=None, attempt=1, role="dialogue", nonce=NONCE, protocol="chat"):
    return {"schema_version": 1, "domain": analysis.DOMAIN, "process_nonce": nonce,
            "call": request if call is None else call, "request": request, "attempt": attempt,
            "role": role, "protocol": protocol}


def final(request=1, messages=None, session="session-a", **kwargs):
    if messages is None:
        messages = [("system", "stable-system", 80), ("user", "question", 30)]
    return {**base(request, **kwargs), "event": "request", "body": summary("body"),
            "destination": summary("destination"),
            "config": summary("config"), "input": {"status": "present", **summary("input")},
            "message_count": len(messages), "messages_truncated": len(messages) > 512,
            "messages": [{**summary(text, size), "index": index, "role": role, "type": "message"}
                         for index, (role, text, size) in enumerate(messages[:512])],
            "tools": {"status": "present", **summary("tools"), "items": [], "count": 0, "truncated": False},
            "identifiers": {"session_id": identifier(session), "thread_id": identifier(session + "-thread"),
                            "client_request_id": identifier("client-" + str(request)),
                            "prompt_cache_key": identifier(session + "-cache")}}


def headers(request=1, oneapi="oneapi-1", **kwargs):
    return {**base(request, **kwargs), "event": "response_headers", "http_status": 200,
            "oneapi_request_id": identifier(oneapi), "upstream_request_id": identifier("upstream")}


def usage(request=1, input_tokens=100, cached=0, output=5, reported=True, **kwargs):
    return {**base(request, **kwargs), "event": "usage", "usage_reported": reported,
            "input_tokens": input_tokens, "cached_input_tokens": cached, "output_tokens": output}


def encoded(records):
    names = {value: key for key, value in analysis.TAGS.items()}
    return b"".join(names[record["event"]] + json.dumps(record).encode() + b"\n" for record in records)


def run(records, external=None):
    return analysis.analyze(io.BytesIO(encoded(records)),
                            io.BytesIO(json.dumps(external).encode()) if external is not None else None)


class PrefixAnalysisTests(unittest.TestCase):
    def test_whole_item_prefix_tools_and_config_are_separate(self):
        first = final(1)
        second = final(2, messages=[("system", "stable-system", 80), ("user", "changed", 90)])
        second["tools"] = {"status": "present", **summary("changed-tool-schema")}
        second["config"] = summary("changed-effort")
        second["input"] = {"status": "present", **summary("changed-input")}
        result = run([first, second])
        [group] = result["groups"]
        [comparison] = group["comparisons"]
        self.assertEqual(comparison["common_complete_items"], 1)
        self.assertEqual(comparison["common_complete_item_bytes"], 80)
        self.assertEqual(comparison["first_changed_roles"], {"previous": "user", "current": "user"})
        self.assertIs(comparison["message_lists_equal"], False)
        self.assertIs(comparison["tools_equal"], False)
        self.assertIs(comparison["config_equal"], False)
        self.assertEqual(result["measurement"], "whole_item_serialized_bytes_not_tokens")

    def test_client_request_changes_do_not_split_cache_group_and_retry_keeps_attempt(self):
        records = [final(5, call=4), headers(5, call=4), final(7, call=4, attempt=2),
                   usage(7, call=4, attempt=2, cached=40)]
        result = run(records)
        self.assertEqual(len(result["groups"]), 1)
        self.assertEqual(result["groups"][0]["requests"], [5, 7])
        self.assertEqual([r["attempt"] for r in result["requests"]], [1, 2])
        self.assertEqual(result["groups"][0]["wire_usage"]["unknown_requests"], 1)
        self.assertEqual(result["groups"][0]["wire_usage"]["cached_fraction"], .4)

    def test_multi_process_roles_protocols_and_sessions_are_isolated(self):
        records = [final(1), final(2, role="interest"), final(3, protocol="responses"),
                   final(4, session="different"), final(1, nonce="b" * 32)]
        result = run(records)
        self.assertEqual(len(result["groups"]), 5)
        self.assertTrue(all(not g["comparisons"] for g in result["groups"]))

    def test_destination_is_required_and_distinct_endpoints_never_compare(self):
        first, second, missing = final(1), final(2), final(3)
        second["destination"] = summary("different-destination")
        del missing["destination"]
        result = run([first, second, missing])
        self.assertEqual(len(result["groups"]), 2)
        self.assertEqual(result["ungrouped_requests"], 1)
        self.assertTrue(all(not group["comparisons"] for group in result["groups"]))
        self.assertEqual(result["requests"][-1]["destination"]["status"], "unknown")

    def test_interleaved_events_are_joined_by_identity_and_sorted_by_request_counter(self):
        records = [usage(4, cached=70), headers(1), final(4), final(2, session="other"),
                   usage(1, cached=20), final(1), headers(4)]
        result = run(records)
        group = next(g for g in result["groups"] if g["requests"] == [1, 4])
        self.assertEqual(group["wire_usage"]["cached_fraction"], .45)
        self.assertEqual(group["comparisons"][0]["previous_request"], 1)
        self.assertTrue(result["requests"][0]["has_headers"])
        self.assertTrue(result["requests"][0]["has_usage"])

    def test_missing_session_thread_or_cache_never_aggregates(self):
        for field in analysis.GROUP_IDS:
            with self.subTest(field=field):
                records = [final(1), final(2)]
                for record in records:
                    record["identifiers"][field] = {"status": "missing", "bytes": 0}
                result = run(records)
                self.assertEqual(result["groups"], [])
                self.assertEqual(result["ungrouped_requests"], 2)
        empty = final()
        empty["identifiers"]["session_id"] = identifier("")
        self.assertEqual(run([empty])["ungrouped_requests"], 1)

    def test_invalid_and_unreported_identifiers_are_distinct_from_present(self):
        for status in ("multiple", "oversized", "invalid_type"):
            record = final()
            record["identifiers"]["prompt_cache_key"] = {"status": status, "bytes": 1025}
            row = run([record])["requests"][0]
            self.assertIsNone(row["group"])
            self.assertIsNone(row["identifiers"]["prompt_cache_key"]["sha256"])
        orphan = run([headers(), usage()])["requests"][0]
        self.assertFalse(orphan["has_final"])
        self.assertEqual(orphan["identifiers"]["session_id"]["status"], "unknown")

    def test_unknown_role_and_type_never_echo_or_group_dynamic_text(self):
        a, b = final(1, role=SECRET), final(2, role=SECRET)
        a["messages"][0]["role"] = SECRET
        a["messages"][0]["type"] = SECRET
        result = run([a, b])
        self.assertNotIn(SECRET, json.dumps(result))
        self.assertEqual(result["groups"], [])
        self.assertEqual(result["requests"][0]["role"], "unknown")
        a["role"] = b["role"] = "dialogue"
        b["messages"][0]["hmac_sha256"] = sha("different")
        comparison = run([a, b])["groups"][0]["comparisons"][0]
        self.assertEqual(comparison["first_changed_roles"]["previous"], "other")

    def test_missing_zero_invalid_and_zero_input_are_separate_and_weighted(self):
        records = []
        cases = [(None, None, None, False), (100, None, 2, True), (100, 0, 2, True),
                 (300, 150, 2, True), (50, 60, 2, True), (0, 0, 2, True),
                 (10, -1, 2, True), (100, True, 2, True)]
        for index, (input_tokens, cached, output, reported) in enumerate(cases, 1):
            records += [final(index), usage(index, input_tokens, cached, output, reported)]
        result = run(records)
        metric = result["groups"][0]["wire_usage"]
        self.assertEqual(metric, {"valid_requests": 2, "unknown_requests": 2, "invalid_requests": 3,
                                  "zero_input_requests": 1, "input_tokens": 400,
                                  "cached_input_tokens": 150, "cached_fraction": .375})
        self.assertIsNone(result["requests"][0]["usage"]["cached_input_tokens"])
        self.assertEqual(result["requests"][2]["usage"]["cached_input_tokens"], 0)
        self.assertIsNone(result["requests"][6]["usage"]["cached_input_tokens"])

    def test_usage_reported_false_with_numbers_is_invalid(self):
        result = run([final(), usage(reported=False)])
        self.assertEqual(result["requests"][0]["usage"]["status"], "invalid")
        self.assertIsNone(result["groups"][0]["wire_usage"]["cached_fraction"])

    def test_message_truncation_is_only_a_lower_bound_even_if_input_digest_equal(self):
        messages = [("user", str(i), 20) for i in range(513)]
        result = run([final(1, messages), final(2, messages)])
        comparison = result["groups"][0]["comparisons"][0]
        self.assertEqual(comparison["common_complete_items"], 512)
        self.assertEqual(comparison["common_complete_item_bytes"], 10240)
        self.assertEqual(comparison["prefix_certainty"], "lower_bound")
        self.assertIsNone(comparison["message_lists_equal"])
        self.assertIs(comparison["input_equal"], True)

    def test_length_difference_uses_whole_items_not_internal_message_prefix(self):
        first, second = final(1), final(2)
        second["messages"] = second["messages"][:1]
        second["message_count"] = 1
        comparison = run([first, second])["groups"][0]["comparisons"][0]
        self.assertEqual(comparison["common_complete_items"], 1)
        self.assertEqual(comparison["first_changed_roles"], {"previous": "user", "current": "absent"})
        self.assertIs(comparison["message_lists_equal"], False)

    def test_unseen_truncated_role_is_unknown_not_absent(self):
        messages = [("user", str(i), 20) for i in range(513)]
        comparison = run([final(1, messages[:512]), final(2, messages)])["groups"][0]["comparisons"][0]
        self.assertEqual(comparison["common_complete_items"], 512)
        self.assertEqual(comparison["first_changed_roles"], {"previous": "absent", "current": "unknown"})
        self.assertIsNone(comparison["message_lists_equal"])

    def test_unknown_summaries_remain_unknown_and_tools_presence_is_comparable(self):
        first, second = final(1), final(2)
        first["config"] = {"status": "serialization_failed"}
        first["tools"] = {"status": "missing", "bytes": 0}
        first["messages"][0] = {"index": 0, "role": "system", "status": "serialization_failed"}
        comparison = run([first, second])["groups"][0]["comparisons"][0]
        self.assertIsNone(comparison["config_equal"])
        self.assertIs(comparison["tools_equal"], False)
        self.assertEqual(comparison["common_complete_items"], 0)
        self.assertEqual(comparison["prefix_certainty"], "lower_bound")

    def test_identical_duplicate_records_deduplicate_all_event_kinds(self):
        records = [final(), headers(), usage()]
        result = run(records + copy.deepcopy(records))
        self.assertEqual(result["duplicate_records"], 3)
        self.assertEqual(len(result["requests"]), 1)
        self.assertEqual(result["groups"][0]["wire_usage"]["valid_requests"], 1)

    def test_conflicting_duplicate_and_cross_identity_are_data_errors(self):
        for factory, field, value in [(final, "body", summary("changed")),
                                      (headers, "http_status", 503), (usage, "cached_input_tokens", 50)]:
            with self.subTest(factory=factory):
                a, b = factory(), factory()
                b[field] = value
                with self.assertRaisesRegex(analysis.DataError, "conflicting_duplicate"):
                    run([a, b])
        for b in [headers(role="interest"), headers(call=9), headers(attempt=2),
                  headers(protocol="responses"), final(2, call=1)]:
            with self.assertRaisesRegex(analysis.DataError, "identity_conflict"):
                run([final(), b])

    def test_unknown_fields_are_ignored_but_conflicting_duplicate_is_not_silently_dropped(self):
        record = final()
        record[SECRET] = {"secret": SECRET}
        record["identifiers"]["session_id"][SECRET] = SECRET
        self.assertNotIn(SECRET, json.dumps(run([record])))
        changed = copy.deepcopy(record)
        changed[SECRET] = "different"
        with self.assertRaisesRegex(analysis.DataError, "conflicting_duplicate"):
            run([record, changed])

    def test_narrow_external_join_uses_only_oneapi_hash_and_keeps_usage_separate(self):
        records = [final(), headers(), usage(cached=None)]
        external = [{"request_id_sha256": sha("oneapi-1"), "input_tokens": 80,
                     "cached_input_tokens": 60, "output_tokens": 8, "source": SECRET},
                    {"request_id_sha256": sha("upstream"), "input_tokens": 1,
                     "cached_input_tokens": 1}]
        result = run(records, external)
        row = result["requests"][0]
        self.assertEqual(row["external_join"], "matched")
        self.assertIsNone(row["usage"]["cached_input_tokens"])
        self.assertEqual(row["external_usage"]["cached_input_tokens"], 60)
        self.assertEqual(result["external"]["matched_rows"], 1)
        self.assertEqual(result["external"]["unmatched_rows"], 1)
        self.assertEqual(result["groups"][0]["external_usage"]["cached_fraction"], .75)
        self.assertNotIn(SECRET, json.dumps(result))

    def test_current_cache_actual_groups_rows_shape_and_cache_present(self):
        rows = [{"request_id_present": True, "request_id_sha256": sha("oneapi-1"),
                 "input": 120, "output": 4, "cache_tokens": 0, "cache_present": True,
                 "model": SECRET, "request_path": SECRET},
                {"request_id_present": True, "request_id_sha256": sha("oneapi-2"),
                 "input": 120, "output": 4, "cache_tokens": 99, "cache_present": False}]
        result = run([final(1), headers(1), final(2), headers(2, oneapi="oneapi-2")],
                     {"groups": [{"role_credential": SECRET, "rows": rows}], "credential_link": SECRET})
        self.assertEqual(result["requests"][0]["external_usage"]["cached_input_tokens"], 0)
        self.assertIsNone(result["requests"][1]["external_usage"]["cached_input_tokens"])
        self.assertEqual(result["external"]["format"], "current_cache")
        self.assertNotIn(SECRET, json.dumps(result))

    def test_external_duplicate_conflict_and_ambiguous_join_are_errors(self):
        row = {"request_id_sha256": sha("oneapi-1"), "input_tokens": 10, "cached_input_tokens": 2}
        result = run([final(), headers()], [row, row])
        self.assertEqual(result["external"]["duplicate_rows"], 1)
        with self.assertRaisesRegex(analysis.DataError, "external_conflict"):
            run([], [row, {**row, "cached_input_tokens": 4}])
        with self.assertRaisesRegex(analysis.DataError, "external_join_ambiguous"):
            run([final(1), headers(1), final(2), headers(2)], [row])

    def test_external_unmatched_missing_and_invalid_usage_stay_explicit(self):
        rows = [{"request_id_sha256": sha("oneapi-1"), "input_tokens": 10, "cached_input_tokens": 11},
                {"request_id_sha256": None}]
        result = run([final(1), headers(1), final(2), headers(2, oneapi="other"), final(3)], rows)
        self.assertEqual([r["external_join"] for r in result["requests"]],
                         ["matched", "unmatched", "missing_header"])
        self.assertEqual(result["groups"][0]["external_usage"]["invalid_requests"], 1)
        self.assertEqual(result["external"]["skipped_missing_id"], 1)

    def test_bounds_apply_to_lines_records_total_bytes_and_external(self):
        with self.assertRaisesRegex(analysis.DataError, "line_limit"):
            analysis.analyze(io.BytesIO(b"x" * (analysis.MAX_LINE + 1)))
        with patch.object(analysis, "MAX_RECORDS", 2):
            with self.assertRaisesRegex(analysis.DataError, "record_limit"):
                run([final(), headers(), usage()])
            with self.assertRaisesRegex(analysis.DataError, "record_limit"):
                run([final()], [{"request_id_sha256": None}] * 2)
        with patch.object(analysis, "MAX_INPUT_BYTES", 10):
            with self.assertRaisesRegex(analysis.DataError, "input_limit"):
                analysis.analyze(io.BytesIO(b"ignored\nignored\n"))
        with patch.object(analysis, "MAX_LINES", 2):
            with self.assertRaisesRegex(analysis.DataError, "line_count_limit"):
                analysis.analyze(io.BytesIO(b"x\nx\nx\n"))

    def test_schema_rejects_corrupt_counts_duplicate_keys_and_nonfinite_json(self):
        for count, truncated in [(3, False), (513, False), (2, True)]:
            record = final()
            record["message_count"], record["messages_truncated"] = count, truncated
            with self.assertRaises(analysis.DataError):
                run([record])
        for payload in [b'{"key":1,"key":2}', b'{"key":NaN}']:
            with self.assertRaises(analysis.DataError):
                analysis.analyze(io.BytesIO(b"EVE_MODEL_WIRE_FINAL " + payload))

    def cli(self, payload=b"", args=()):
        return subprocess.run([sys.executable, "-B", str(SCRIPT), *args], input=payload,
                              capture_output=True, timeout=10)

    def test_cli_ignores_nondiagnostic_secrets_and_unknown_fields(self):
        record = final()
        record["body"][SECRET] = SECRET
        result = self.cli((SECRET + "\nBearer " + SECRET + "\n").encode() + encoded([record]))
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, b"")
        self.assertNotIn(SECRET.encode(), result.stdout)
        self.assertEqual(json.loads(result.stdout)["ignored_lines"], 2)

    def test_cli_errors_are_fixed_and_do_not_echo_raw_json_paths_or_arguments(self):
        cases = [(b"EVE_MODEL_WIRE_FINAL " + SECRET.encode(), ()),
                 (b"", ("--log", "/missing/" + SECRET)),
                 (b"", ("--" + SECRET,))]
        for payload, args in cases:
            result = self.cli(payload, args)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stderr, b"")
            self.assertNotIn(SECRET.encode(), result.stdout)
            self.assertEqual(json.loads(result.stdout)["status"], "data_error")

    def test_cli_log_file_and_external_file_are_read_only(self):
        with tempfile.TemporaryDirectory(prefix="eve-prefix-analysis-") as directory:
            log, external = Path(directory) / "wire.log", Path(directory) / "safe.json"
            log.write_bytes(encoded([final(), headers(), usage()]))
            external.write_text(json.dumps([{"request_id_sha256": sha("oneapi-1"),
                                              "input_tokens": 100, "cached_input_tokens": 25}]))
            before = log.read_bytes(), external.read_bytes()
            result = self.cli(args=("--log", str(log), "--newapi-safe", str(external)))
            self.assertEqual(result.returncode, 0)
            self.assertEqual(json.loads(result.stdout)["external"]["matched_rows"], 1)
            self.assertEqual((log.read_bytes(), external.read_bytes()), before)


if __name__ == "__main__":
    unittest.main()
