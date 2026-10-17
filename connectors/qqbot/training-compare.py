"""同一历史输入的新旧提示词对照；不连接 QQ、不调用工具，正文仅加密保存。"""
from collections import Counter
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import tempfile
import time

spec = importlib.util.spec_from_file_location("review", Path(__file__).with_name("training-review.py"))
review = importlib.util.module_from_spec(spec)
spec.loader.exec_module(review)
ROOT = Path(__file__).resolve().parents[2]
MAX_SAMPLES = 12


def samples(evidence, limit=MAX_SAMPLES):
    if type(limit) is not int or not 1 <= limit <= MAX_SAMPLES:
        raise ValueError("sample_limit")
    marked = [i for i, e in enumerate(evidence) if len(re.findall(r"[?？]+", e["reply"])) > 1]
    coverage = [int(k * (len(evidence) - 1) / max(1, limit - 1)) for k in range(limit)] if evidence else []
    chosen = list(dict.fromkeys(marked + coverage + list(range(len(evidence)))))[:limit]
    histories, selected = {}, []
    for index, item in enumerate(evidence):
        history = histories.setdefault(item["session"], [])
        if index in chosen:
            selected.append({"history": history[-2:], "input": item["input"]})
        history.append({"input": item["input"], "reply": item["reply"]})
        del history[:-2]
    return selected


def generate(prompt, identity, record, timeout, response_policy=None):
    messages = [{"role": "system", "content": identity}, {"role": "system", "content": prompt}]
    for turn in record["history"]:
        messages.extend([{"role": "user", "content": turn["input"]}, {"role": "assistant", "content": turn["reply"]}])
    if response_policy:
        messages.append({"role": "system", "content": response_policy})
    messages.append({"role": "user", "content": record["input"]})
    return review.chat(messages, 4096, timeout)


def compare(evidence, baseline, candidate, identity, generator=generate, evaluator=review.judge, clock=time.monotonic,
            candidate_identity=None, candidate_response_policy=None):
    selected = samples(evidence)
    deadline = clock() + 600
    rows, requests, failures = [], 0, Counter()
    for index, record in enumerate(selected):
        pair = []
        for variant, prompt in (("baseline", baseline), ("candidate", candidate)):
            if clock() >= deadline:
                break
            requests += 1
            try:
                selected_identity = identity if variant == "baseline" or candidate_identity is None else candidate_identity
                extra = {"response_policy": candidate_response_policy} if variant == "candidate" and candidate_response_policy else {}
                text = generator(prompt, selected_identity, record, timeout=min(30, max(0.1, deadline - clock())), **extra)
                if type(text) is not str or not text.strip() or len(text.encode()) > 32768:
                    raise ValueError("model_protocol")
            except (ValueError, TypeError, KeyError, IndexError, AttributeError, OSError) as error:
                failures[review.error_code(error)] += 1
                break
            pair.append({"index": index * 2 + len(pair), "history": record["history"],
                         "input": record["input"], "reply": text, "variant": variant})
        if len(pair) == 2:
            rows.extend(pair)
    counts = {v: {"question_acts": Counter(), "issue_flags": Counter(), "paragraphs": 0, "characters": 0} for v in ("baseline", "candidate")}
    assessed = 0
    for start in range(0, len(rows), review.MAX_BATCH):
        if clock() >= deadline:
            break
        batch = rows[start:start + review.MAX_BATCH]
        # 评审不知道哪个提示词生成了回复，只接收相同上下文及有界正文。
        records = [{k: v for k, v in row.items() if k != "variant"} for row in batch]
        requests += 1
        try:
            if evaluator is review.judge:
                value = review.judge(records, timeout=min(45, max(0.1, deadline - clock())))
            else:
                value = evaluator(records)
            results = review.validate_reviews(value, {r["index"] for r in records})
        except (ValueError, TypeError, KeyError, IndexError, AttributeError, OSError) as error:
            failures[review.error_code(error)] += 1
            continue
        for row in batch:
            item = results[row["index"]]
            count = counts[row["variant"]]
            count["question_acts"][item["question_act"]] += 1
            count["issue_flags"].update(item["flags"])
            count["paragraphs"] += len([p for p in re.split(r"\n\s*\n", row["reply"]) if p.strip()])
            count["characters"] += len(row["reply"])
            assessed += 1
    for count in counts.values():
        for key in ("question_acts", "issue_flags"):
            count[key] = dict(count[key])
    complete = len(rows) == assessed == len(selected) * 2 and len(selected) > 0
    metrics = [(counts[v]["question_acts"].get("multiple", 0),
                *(counts[v]["issue_flags"].get(flag, 0) for flag in sorted(review.FLAGS))) for v in ("baseline", "candidate")]
    resolved = all(counts[v]["question_acts"].get("uncertain", 0) == 0 for v in counts)
    non_regression = complete and resolved and all(new <= old for old, new in zip(*metrics))
    summary = {"selected_pairs": len(selected), "generated_pairs": len(rows) // 2,
               "reviewed_pairs": assessed // 2, "model_requests": requests, "failure_codes": dict(failures),
               "counts": counts, "complete": complete, "advisory_non_regression": non_regression,
               "advisory_improved": non_regression and any(new < old for old, new in zip(*metrics)),
               "question_assessment_resolved": resolved,
               "assessment": "same_inputs_two_recent_turns_model_advisory",
               "same_identity": candidate_identity is None or candidate_identity == identity,
               "candidate_response_policy": bool(candidate_response_policy),
               "limitations": ["优先覆盖旧多问号候选及分布样本，最多12对；不代表全量用户质量。",
                               "两种策略使用同一输入和最近两轮历史；身份是否一致由 same_identity 标明，不含工具或QQ发送。",
                               "分类只为主模型建议，长期偏好和真人满意度仍需另外验收。"]}
    return summary, rows


def main():
    parser = argparse.ArgumentParser()
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--source", type=Path)
    source.add_argument("--synthetic", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline-prompt", type=Path, default=ROOT / "crates/training-plugin/src/prompt-v4.txt")
    parser.add_argument("--candidate-prompt", type=Path, default=ROOT / "crates/training-plugin/src/prompt-v5.txt")
    parser.add_argument("--baseline-agent", type=Path)
    parser.add_argument("--candidate-policy", type=Path, default=ROOT / "crates/eve-app/src/dialogue-policy.txt")
    args = parser.parse_args()
    try:
        baseline = args.baseline_prompt.read_text().strip()
        candidate = args.candidate_prompt.read_text().strip()
        identity = (ROOT / "AGENT.md").read_text()
        baseline_identity = args.baseline_agent.read_text() if args.baseline_agent else identity
        response_policy = args.candidate_policy.read_text().strip()
        if args.synthetic:
            fixture = ROOT / "connectors/qqbot/test/fixtures/training-public-cases.json"
            summary, _ = compare(json.loads(fixture.read_bytes()), baseline, candidate, baseline_identity,
                                 candidate_identity=identity, candidate_response_policy=response_policy)
            summary.update(version=1, evidence_kind="public_synthetic",
                           fixture_sha256=hashlib.sha256(fixture.read_bytes()).hexdigest())
            summary["assessment"] = "public_synthetic_same_inputs_model_advisory"
            summary["limitations"] = ["12 个独立新写的公开场景，不读取或解密真实交流。",
                                      "覆盖短答、口语、任务、拒绝提问与自然段；不代表真实用户满意度。",
                                      "不验证完整宿主工具、记忆装配、正式QQ或真人满意度。"]
        else:
            with tempfile.TemporaryDirectory(prefix="eve-training-compare-") as directory:
                root = Path(directory)
                saved = review.verify_source(args.source, root / "source")
                summary, rows = compare(saved["evidence"], baseline, candidate, baseline_identity,
                                        candidate_identity=identity, candidate_response_policy=response_policy)
                summary.update(version=1, source_sha=saved["window"]["source_sha"])
                private = root / "comparison"
                private.mkdir()
                review.window.write_json(private / "training-report.json", {"summary": summary, "outputs": rows})
                review.window.seal(private, args.output.with_suffix(".enc"))
        summary["candidate_prompt_sha256"] = hashlib.sha256(candidate.encode()).hexdigest()
        summary["baseline_prompt_sha256"] = hashlib.sha256(baseline.encode()).hexdigest()
        summary["baseline_agent_sha256"] = hashlib.sha256(baseline_identity.encode()).hexdigest()
        summary["candidate_agent_sha256"] = hashlib.sha256(identity.encode()).hexdigest()
        summary["candidate_policy_sha256"] = hashlib.sha256(response_policy.encode()).hexdigest()
        review.window.write_json(args.output, summary)
        print(json.dumps(summary, ensure_ascii=False))
        if not summary["complete"] or not summary["advisory_non_regression"]:
            raise ValueError("comparison_incomplete_or_regressed")
    except (ValueError, TypeError, KeyError, AttributeError, OSError, review.tarfile.TarError, review.subprocess.SubprocessError):
        raise SystemExit("策略对照未通过；公开仅保留无正文计数，不更新用户偏好或发送QQ消息。") from None


if __name__ == "__main__":
    main()
