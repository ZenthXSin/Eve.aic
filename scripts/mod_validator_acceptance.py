#!/usr/bin/env python3
"""用从源码构建的 Mindustry 模组验证器检查随项目提供的示例模组。

用法：python3 scripts/mod_validator_acceptance.py <mindustry-mod-validator-*-all.jar>

正确的示例必须通过（退出码 0、没有错误），故意写错的示例必须失败（退出码 1）且报告指出出错的内容；
其余示例只要求验证器正常给出报告（退出码 0 或 1，不能是环境初始化失败的 2）。
"""
import json
import shutil
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
EXAMPLES = ROOT / "vendor" / "mindustry-mod-validator" / "example-mods"
PASS = ["good-json-mod", "test-json-mod"]
FAIL = {"bad-json-mod": ["broken-block", "NonExistentBlockType"]}
REPORT_ONLY = ["good-json-mod-with-unit", "full-test-mod"]


def validate(jar: pathlib.Path, name: str, scratch: pathlib.Path) -> tuple[int, dict]:
    report = scratch / f"{name}.json"
    # 每个示例单独一个临时目录：验证器把 java.io.tmpdir 下的测试数据目录整体删除后重建。
    tmp = scratch / f"{name}-tmp"
    tmp.mkdir()
    # 目录形式的模组会被打包到同级 zip，复制一份再验证，不改动仓库中的示例。
    mod = scratch / name
    shutil.copytree(EXAMPLES / name, mod)
    process = subprocess.run(
        ["java", "-Xmx1g", "-Djava.awt.headless=true", f"-Djava.io.tmpdir={tmp}",
         "-jar", str(jar), str(mod), "--json", "--output", str(report)],
        capture_output=True, text=True, timeout=300,
    )
    print(f"== {name}: exit={process.returncode}")
    if process.returncode == 2 or not report.exists():
        print(process.stdout[-4000:])
        print(process.stderr[-4000:])
        raise SystemExit(f"{name}: 验证器未能给出报告")
    data = json.loads(report.read_text(encoding="utf-8"), strict=False)  # 报告不转义制表符
    for issue in data.get("issues", []):
        if issue.get("severity") in ("ERROR", "CRITICAL", "WARN"):
            print(f"  [{issue.get('severity')}] [{issue.get('category')}] {str(issue.get('message'))[:300]}")
    return process.returncode, data


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    jar = pathlib.Path(sys.argv[1]).resolve()
    failures = []
    with tempfile.TemporaryDirectory() as scratch:
        scratch = pathlib.Path(scratch)
        for name in PASS:
            code, data = validate(jar, name, scratch)
            if code != 0 or data["summary"]["errors"] != 0:
                failures.append(f"{name} 应通过验证")
        for name, needles in FAIL.items():
            code, data = validate(jar, name, scratch)
            text = json.dumps(data["issues"], ensure_ascii=False)
            if code != 1 or data["summary"]["errors"] == 0:
                failures.append(f"{name} 应验证失败")
            if not any(needle in text for needle in needles):
                failures.append(f"{name} 的报告没有指出出错的内容")
        for name in REPORT_ONLY:
            validate(jar, name, scratch)
    if failures:
        raise SystemExit("；".join(failures))
    print("验证器验收通过")


if __name__ == "__main__":
    main()
