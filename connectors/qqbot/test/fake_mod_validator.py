"""继承的 Mindustry 模组验证器的确定性替身：按真实验证器的命令行与 JSON 报告格式应答。

只用于离线验收调用协议与证据处理，不代表真实游戏行为；真实验证器由 CI 从源码构建后验证。
用法与真实验证器相同：python3 fake_mod_validator.py <JVM 参数...> -jar <jar> <模组目录> --json --output <报告>
与真实游戏一致：未知类型只是警告；PowerTurret 缺少 shootType 时报告内容加载错误；
内容文件里出现 `crashOnUpdate` 时报告方块更新崩溃；
模组目录里有 `FATAL` 文件时按环境初始化失败退出。
"""
import json
import pathlib
import re
import sys

BLOCK_TYPES = {"Wall", "Conveyor", "Drill", "Router", "Battery", "Door"}


def main():
    args = sys.argv[1:]
    assert "-jar" in args, "fake validator must be launched like java -jar"
    rest = args[args.index("-jar") + 2:]
    mod = pathlib.Path(rest[0])
    report = pathlib.Path(rest[rest.index("--output") + 1])
    if (mod / "FATAL").exists():
        print("致命错误: 初始化失败", file=sys.stderr)
        sys.exit(2)
    issues = [{"severity": "WARN", "category": "runtime-log", "message": "无关的警告", "detail": None}]
    for path in sorted(mod.glob("content/*/*")):
        text = path.read_text(encoding="utf8")
        declared = re.search(r"^type:\s*(\S+)", text, re.M)
        if path.parent.name == "blocks" and declared and declared.group(1) not in BLOCK_TYPES | {"PowerTurret"}:
            issues.append({"severity": "WARN", "category": "runtime-log",
                           "message": f"No type '{declared.group(1)}' found", "detail": None})
        if declared and declared.group(1) == "PowerTurret" and "shootType" not in text:
            issues.append({"severity": "ERROR", "category": "content-parse",
                           "message": f"{path.name}: 'shootType' in PowerTurret is missing!", "detail": None})
        if "crashOnUpdate" in text:
            name = path.stem
            # 真实验证器的报告不转义字符串里的制表符。
            issues.append({"severity": "ERROR", "category": "block-test",
                           "message": f"方块 '{name}' 更新时崩溃:\tNullPointerException", "detail": None})
    errors = sum(i["severity"] == "ERROR" for i in issues)
    text = json.dumps({"modName": mod.name, "status": "失败" if errors else "通过",
                       "summary": {"errors": errors, "warnings": 1, "info": 0}, "issues": issues},
                      ensure_ascii=False, indent=2).replace("\\t", "\t")
    report.write_text(text, encoding="utf8")
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
