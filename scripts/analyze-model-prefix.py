#!/usr/bin/env python3
"""离线分析 EVE_MODEL_WIRE_*；只输出白名单元数据，不读取网络或调用模型。

默认读 stdin，也可 --log FILE。--newapi-safe FILE 接受 current-cache.json 的
groups[].rows[]，或窄 JSON 数组：request_id_sha256、input_tokens、
cached_input_tokens、output_tokens。只按 oneapi_request_id 的 SHA256 精确关联。
消息前缀的 bytes 为完整项目 JSON 字节数之和，不是 token 或 tokenizer 前缀。
"""
import argparse
import hashlib
import json
import re
import sys


MAX_LINE = 1024 * 1024
MAX_RECORDS = 20000
MAX_INPUT_BYTES = 64 * 1024 * 1024
MAX_LINES = 200000
MAX_NUMBER = (1 << 64) - 1
DOMAIN = "eve.wire_diagnostics.v1"
TAGS = {b"EVE_MODEL_WIRE_FINAL ": "request",
        b"EVE_MODEL_WIRE_HEADERS ": "response_headers",
        b"EVE_MODEL_WIRE_USAGE ": "usage"}
ROLES = frozenset("dialogue speech plan interest research practice skill forge outreach clarify "
                  "preference reflection summary task digest phrase conversation_learning impression "
                  "chime group_relation_learning group_digest sticker review progress "
                  "message_relation exploration".split())
MESSAGE_ROLES = frozenset("system developer user assistant tool function absent other".split())
ID_FIELDS = ("session_id", "thread_id", "client_request_id", "prompt_cache_key")
GROUP_IDS = ("session_id", "thread_id", "prompt_cache_key")


class DataError(Exception):
    """参数仅允许代码内固定错误类别，不携带输入值。"""


def require(condition, code="invalid_schema"):
    if not condition:
        raise DataError(code)


def number(value, positive=False):
    return type(value) is int and (1 if positive else 0) <= value <= MAX_NUMBER


def digest(value, length=64):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{%d}" % length, value) is not None


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=True).encode()).hexdigest()


def decode(payload):
    def object_pairs(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, "duplicate_json_key")
            result[key] = value
        return result

    def invalid_constant(_):
        raise DataError("invalid_json")

    try:
        return json.loads(payload, object_pairs_hook=object_pairs, parse_constant=invalid_constant)
    except (ValueError, UnicodeError, RecursionError):
        raise DataError("invalid_json") from None


class Budget:
    def __init__(self):
        self.bytes = 0
        self.lines = 0
        self.records = 0

    def count(self):
        self.records += 1
        require(self.records <= MAX_RECORDS, "record_limit")

    def lines_from(self, stream):
        while True:
            line = stream.readline(MAX_LINE + 1)
            if not line:
                return
            require(len(line) <= MAX_LINE, "line_limit")
            self.bytes += len(line)
            self.lines += 1
            require(self.bytes <= MAX_INPUT_BYTES, "input_limit")
            require(self.lines <= MAX_LINES, "line_count_limit")
            yield line


def identifier(value):
    if value is None:
        return {"status": "unknown", "sha256": None, "bytes": None}
    require(isinstance(value, dict))
    status = value.get("status")
    require(status in ("present", "missing", "oversized", "multiple", "invalid_type"))
    size = value.get("bytes")
    if status == "present":
        require(number(size) and size <= 1024 and digest(value.get("sha256")))
    elif status == "missing":
        require(size == 0 and type(size) is int)
    elif status == "oversized":
        require(number(size) and size > 1024)
    else:
        size = None
    return {"status": status, "sha256": value["sha256"] if status == "present" else None,
            "bytes": size}


def summary(value):
    if value is None:
        return {"status": "unknown", "hmac": None, "bytes": None}
    require(isinstance(value, dict))
    status = value.get("status", "present")
    require(status in ("present", "missing", "serialization_failed"))
    if status == "present":
        require(digest(value.get("hmac_sha256")) and number(value.get("bytes")))
        return {"status": status, "hmac": value["hmac_sha256"], "bytes": value["bytes"]}
    if status == "missing":
        require(value.get("bytes") == 0 and type(value.get("bytes")) is int)
    return {"status": status, "hmac": None, "bytes": 0 if status == "missing" else None}


def final_record(value):
    items = value.get("messages")
    count = value.get("message_count")
    truncated = value.get("messages_truncated")
    require(isinstance(items, list) and len(items) <= 512 and number(count))
    require(type(truncated) is bool and len(items) == min(count, 512)
            and truncated == (count > 512))
    messages = []
    for index, item in enumerate(items):
        require(isinstance(item, dict) and type(item.get("index")) is int and item["index"] == index)
        role = item.get("role")
        item_summary = summary(item)
        require(item_summary["status"] in ("present", "serialization_failed"))
        messages.append({**item_summary, "role": role if isinstance(role, str) and role in MESSAGE_ROLES else "other"})
    ids = value.get("identifiers", {})
    require(isinstance(ids, dict))
    return {"body": summary(value.get("body")), "config": summary(value.get("config")),
            "destination": summary(value.get("destination")),
            "input": summary(value.get("input")), "tools": summary(value.get("tools")),
            "messages": messages, "message_count": count, "messages_truncated": truncated,
            "identifiers": {name: identifier(ids.get(name)) for name in ID_FIELDS}}


def usage_record(value, reported=None):
    fields = ("input_tokens", "cached_input_tokens", "output_tokens")
    raw = [value.get(name) for name in fields]
    invalid = any(item is not None and not number(item) for item in raw)
    safe = {name: item if number(item) else None for name, item in zip(fields, raw)}
    if reported is not None:
        require(type(reported) is bool)
        invalid |= reported != any(item is not None for item in raw)
    input_tokens, cached = safe["input_tokens"], safe["cached_input_tokens"]
    if invalid or (input_tokens is not None and cached is not None and cached > input_tokens):
        status = "invalid"
    elif input_tokens is None or cached is None:
        status = "unknown"
    elif input_tokens == 0:
        status = "zero_input"
    else:
        status = "valid"
    return {"usage_reported": reported, **safe, "status": status}


def empty_usage():
    return {"usage_reported": None, "input_tokens": None, "cached_input_tokens": None,
            "output_tokens": None, "status": "unknown"}


def read_wire(stream, budget):
    requests = {}
    seen = {}
    request_ids = {}
    call_attempts = {}
    calls = {}
    duplicates = 0
    ignored = 0
    for line in budget.lines_from(stream):
        tag = next((tag for tag in TAGS if line.startswith(tag)), None)
        if tag is None:
            ignored += 1
            continue
        budget.count()
        value = decode(line[len(tag):])
        require(isinstance(value, dict))
        require(value.get("schema_version") == 1 and type(value.get("schema_version")) is int
                and value.get("domain") == DOMAIN and value.get("event") == TAGS[tag])
        nonce, protocol, role = value.get("process_nonce"), value.get("protocol"), value.get("role")
        require(digest(nonce, 32) and protocol in ("chat", "responses"))
        require(role is None or isinstance(role, str))
        role = role if role in ROLES else "unknown"
        call, request, attempt = (value.get(name) for name in ("call", "request", "attempt"))
        require(all(number(n, positive=True) for n in (call, request, attempt)))
        key = nonce, call, request, attempt
        identity = key + (role, protocol)
        for index, map_key in ((request_ids, (nonce, request)), (call_attempts, (nonce, call, attempt))):
            require(map_key not in index or index[map_key] == identity, "identity_conflict")
            index[map_key] = identity
        call_key = nonce, call
        require(call_key not in calls or calls[call_key] == (role, protocol), "identity_conflict")
        calls[call_key] = role, protocol
        event_key = key + (TAGS[tag],)
        content_sha = fingerprint(value)
        if event_key in seen:
            require(seen[event_key] == content_sha, "conflicting_duplicate")
            duplicates += 1
            continue
        seen[event_key] = content_sha
        if key not in requests:
            requests[key] = {"process_nonce": nonce, "call": call, "request": request,
                             "attempt": attempt, "role": role, "protocol": protocol}
        if TAGS[tag] == "request":
            requests[key]["final"] = final_record(value)
        elif TAGS[tag] == "response_headers":
            status = value.get("http_status")
            require(type(status) is int and 100 <= status <= 599)
            requests[key]["headers"] = {"http_status": status,
                "oneapi_request_id": identifier(value.get("oneapi_request_id")),
                "upstream_request_id": identifier(value.get("upstream_request_id"))}
        else:
            require("usage_reported" in value and type(value["usage_reported"]) is bool)
            requests[key]["usage"] = usage_record(value, value["usage_reported"])
    return sorted(requests.values(), key=lambda r: (r["process_nonce"], r["request"])), duplicates, ignored


def read_external(stream, budget):
    value = decode(b"".join(budget.lines_from(stream)))
    if isinstance(value, list):
        rows = value
        shape = "records"
    else:
        require(isinstance(value, dict) and isinstance(value.get("groups"), list), "external_schema")
        rows = []
        for group in value["groups"]:
            require(isinstance(group, dict) and isinstance(group.get("rows"), list), "external_schema")
            rows.extend(group["rows"])
            require(len(rows) <= MAX_RECORDS, "record_limit")
        shape = "current_cache"
    result, seen = {}, {}
    skipped = duplicates = 0
    for row in rows:
        budget.count()
        require(isinstance(row, dict), "external_schema")
        key = row.get("request_id_sha256")
        if key is None:
            skipped += 1
            continue
        require(digest(key), "external_schema")
        if "request_id_present" in row:
            require(row["request_id_present"] is True, "external_schema")
        content_sha = fingerprint(row)
        if key in seen:
            require(seen[key] == content_sha, "external_conflict")
            duplicates += 1
            continue
        seen[key] = content_sha
        if shape == "current_cache":
            present = row.get("cache_present")
            require(type(present) is bool, "external_schema")
            normalized = {"input_tokens": row.get("input"),
                          "cached_input_tokens": row.get("cache_tokens") if present else None,
                          "output_tokens": row.get("output")}
        else:
            normalized = {name: row.get(name) for name in
                          ("input_tokens", "cached_input_tokens", "output_tokens")}
        result[key] = usage_record(normalized)
    return result, {"format": shape, "unique_rows": len(result), "skipped_missing_id": skipped,
                    "duplicate_rows": duplicates}


def equality(left, right):
    if left["status"] == right["status"] == "missing":
        return True
    if left["status"] == right["status"] == "present":
        return (left["hmac"], left["bytes"]) == (right["hmac"], right["bytes"])
    if {left["status"], right["status"]} == {"missing", "present"}:
        return False
    return None


def compare(left, right):
    previous, current = left["final"], right["final"]
    matched = size = 0
    changed = None
    certainty = "exact"
    for before, after in zip(previous["messages"], current["messages"]):
        same = equality(before, after)
        if same is None:
            certainty = "lower_bound"
            break
        if not same:
            changed = {"previous": before["role"], "current": after["role"]}
            break
        matched += 1
        size += before["bytes"]
    else:
        visible = min(len(previous["messages"]), len(current["messages"]))
        if previous["message_count"] > visible and current["message_count"] > visible:
            certainty = "lower_bound"
        elif previous["message_count"] != current["message_count"]:
            changed = {"previous": previous["messages"][visible]["role"]
                       if visible < len(previous["messages"]) else
                       "unknown" if previous["message_count"] > visible else "absent",
                       "current": current["messages"][visible]["role"]
                       if visible < len(current["messages"]) else
                       "unknown" if current["message_count"] > visible else "absent"}
    # 整体 input HMAC 可确认完整数组；截断列表仍只给已观察到的项目下界。
    full_lists = not previous["messages_truncated"] and not current["messages_truncated"]
    if not full_lists:
        certainty = "lower_bound"
    all_equal = (matched == previous["message_count"] == current["message_count"]
                 if full_lists and certainty == "exact" else None)
    return {"previous_request": left["request"], "request": right["request"],
            "common_complete_items": matched, "common_complete_item_bytes": size,
            "prefix_certainty": certainty, "message_lists_equal": all_equal,
            "first_changed_roles": changed,
            "input_equal": equality(previous["input"], current["input"]),
            "tools_equal": equality(previous["tools"], current["tools"]),
            "config_equal": equality(previous["config"], current["config"])}


def weighted(usages):
    result = {"valid_requests": 0, "unknown_requests": 0, "invalid_requests": 0,
              "zero_input_requests": 0, "input_tokens": 0, "cached_input_tokens": 0,
              "cached_fraction": None}
    for usage in usages:
        status = usage["status"]
        result[status + "_requests"] += 1
        if status == "valid":
            result["input_tokens"] += usage["input_tokens"]
            result["cached_input_tokens"] += usage["cached_input_tokens"]
    if result["input_tokens"]:
        result["cached_fraction"] = result["cached_input_tokens"] / result["input_tokens"]
    return result


def analyze(stream, external_stream=None):
    budget = Budget()
    requests, duplicates, ignored = read_wire(stream, budget)
    external, external_info = ({}, None)
    if external_stream is not None:
        external, external_info = read_external(external_stream, budget)
    groups, output = {}, []
    joined_ids = set()
    for request in requests:
        row = {key: request[key] for key in ("process_nonce", "call", "request", "attempt", "role", "protocol")}
        final, headers = request.get("final"), request.get("headers")
        row.update({"has_final": final is not None, "has_headers": headers is not None,
                    "has_usage": "usage" in request, "http_status": headers["http_status"] if headers else None,
                    "usage": request.get("usage", empty_usage())})
        row["identifiers"] = final["identifiers"] if final else {name: identifier(None) for name in ID_FIELDS}
        row["oneapi_request_id"] = headers["oneapi_request_id"] if headers else identifier(None)
        row["upstream_request_id"] = headers["upstream_request_id"] if headers else identifier(None)
        row["message_count"] = final["message_count"] if final else None
        row["messages_truncated"] = final["messages_truncated"] if final else None
        row["body_bytes"] = final["body"]["bytes"] if final else None
        destination = final["destination"] if final else summary(None)
        row["destination"] = {"status": destination["status"], "bytes": destination["bytes"],
                              "hmac_sha256": destination["hmac"]}
        oneapi = row["oneapi_request_id"]["sha256"]
        joined = external.get(oneapi)
        row["external_join"] = ("matched" if joined is not None else "missing_header" if oneapi is None
                                else "unmatched") if external_stream is not None else "not_supplied"
        row["external_usage"] = joined
        if joined is not None:
            # 一个外部请求ID重复关联多个本地attempt无法确定归属，拒绝双重计数。
            require(oneapi not in joined_ids, "external_join_ambiguous")
            joined_ids.add(oneapi)
        ids = row["identifiers"]
        groupable = (final is not None and request["role"] != "unknown"
                     and destination["status"] == "present"
                     and all(ids[name]["status"] == "present" and ids[name]["bytes"] > 0 for name in GROUP_IDS))
        row["group"] = None
        row["group_status"] = "grouped" if groupable else "incomplete_identity"
        if groupable:
            key = (request["process_nonce"], request["role"], request["protocol"],
                   destination["hmac"], destination["bytes"],
                   *(ids[name]["sha256"] for name in GROUP_IDS))
            row["group"] = fingerprint(key)
            groups.setdefault(key, []).append((request, row))
        output.append(row)
    summaries = []
    for key, members in groups.items():
        comparisons = [compare(left[0], right[0]) for left, right in zip(members, members[1:])]
        summaries.append({"group": members[0][1]["group"], "process_nonce": key[0], "role": key[1],
                          "protocol": key[2], "destination": {"hmac_sha256": key[3], "bytes": key[4]},
                          "identifiers": dict(zip(GROUP_IDS, key[5:])),
                          "requests": [pair[0]["request"] for pair in members],
                          "comparisons": comparisons,
                          "wire_usage": weighted(pair[1]["usage"] for pair in members),
                          "external_usage": weighted(pair[1]["external_usage"] or empty_usage() for pair in members)})
    if external_info is not None:
        external_info.update({"matched_rows": len(joined_ids), "unmatched_rows": len(external) - len(joined_ids),
                              "join": "oneapi_request_id.sha256=request_id_sha256"})
    return {"schema_version": 1, "status": "ok", "measurement": "whole_item_serialized_bytes_not_tokens",
            "order": "process_nonce_then_request_counter", "requests": output, "groups": summaries,
            "ungrouped_requests": sum(row["group"] is None for row in output),
            "duplicate_records": duplicates, "ignored_lines": ignored, "external": external_info}


class SafeParser(argparse.ArgumentParser):
    def error(self, _):
        raise DataError("invalid_arguments")


def main(argv=None):
    parser = SafeParser(description=__doc__)
    parser.add_argument("--log", help="本地日志文件；省略则读stdin")
    parser.add_argument("--newapi-safe", help="已有安全NewAPI JSON；仅精确摘要join")
    try:
        args = parser.parse_args(argv)
        from contextlib import ExitStack
        with ExitStack() as stack:
            source = stack.enter_context(open(args.log, "rb")) if args.log else sys.stdin.buffer
            external = stack.enter_context(open(args.newapi_safe, "rb")) if args.newapi_safe else None
            result = analyze(source, external)
        print(json.dumps(result, ensure_ascii=True, allow_nan=False, separators=(",", ":")))
        return 0
    except DataError as error:
        print(json.dumps({"schema_version": 1, "status": "data_error", "error": error.args[0]}))
    except (OSError, ValueError, TypeError, RecursionError):
        # 文件名、OS错误、未知JSON值均不得经异常消息或traceback输出。
        print(json.dumps({"schema_version": 1, "status": "data_error", "error": "input_error"}))
    return 2


if __name__ == "__main__":
    sys.exit(main())
