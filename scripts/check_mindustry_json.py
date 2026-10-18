#!/usr/bin/env python3
"""实际执行 `eve_modcheck.py`，确认它接受环境里真实的 hjson 内容文件。

规则来自 eve-portable（AGPL-3.0），本脚本只用于验收：把给定目录下的
mod.hjson 与 content/**/*.hjson|json 送进去，按 JSON 结果判断有没有 ERROR。
用法：python3 scripts/check_mindustry_json.py <模组目录>
"""
import json
import os
import pathlib
import subprocess
import sys

DRIVER = (
    pathlib.Path(__file__).resolve().parent.parent
    / "vendor"
    / "eve-portable-mindustry-json-validator"
    / "eve_modcheck.py"
)


def blocks(root: pathlib.Path):
    files = [root / "mod.hjson"] if (root / "mod.hjson").exists() else []
    files += sorted((root / "content").rglob("*.hjson"))
    files += sorted((root / "content").rglob("*.json"))
    for path in files:
        relative = path.relative_to(root).as_posix()
        yield relative, path.read_text(encoding="utf8")


def main(argv):
    # 输出被管道接收时（如 CI）Windows 默认用本地代码页，中文结果无法编码。
    sys.stdout.reconfigure(encoding="utf-8")
    if len(argv) != 2:
        print(__doc__)
        return 2
    root = pathlib.Path(argv[1]).resolve()
    if not root.is_dir():
        print("不是目录：%s" % root)
        return 2
    body = []
    for relative, content in blocks(root):
        body.append("==== %s ====" % relative)
        body.append(content.rstrip("\n"))
        body.append("==== end ====")
    if not body:
        print("目录里没有 mod.hjson 或 content 文件")
        return 2
    completed = subprocess.run(
        [sys.executable, str(DRIVER), "--mod-name", root.name],
        input="\n".join(body) + "\n",
        capture_output=True,
        text=True,
        # 两端都按 UTF-8；Windows 默认代码页无法编码中文结果。
        encoding="utf-8",
        env={**os.environ, "PYTHONIOENCODING": "utf-8"},
        check=False,
    )
    if completed.returncode != 0:
        print("调用层失败：%s" % completed.stderr.strip())
        return 1
    report = json.loads(completed.stdout)
    if report["error"]:
        print("预检没有完成：%s" % report["error"])
        return 1
    errors = 0
    for file in report["files"]:
        for message in file["messages"]:
            print("%s %s: %s" % (message["level"], file["path"], message["message"]))
            errors += message["level"] == "ERROR"
    print("共 %d 个文件，%d 条错误" % (len(report["files"]), errors))
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
