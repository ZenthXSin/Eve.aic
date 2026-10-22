#!/usr/bin/env python3
"""从 eve-portable 的固定版本导入 Mindustry 资料，生成 vendor/eve-portable-knowledge。

只读给定检出，不执行其中任何文件；选取规则写在本脚本里，重复运行得到相同结果。
命中敏感模式（本机路径、IP、长数字、令牌字样）的文件整篇排除；旧长期记忆只保留
技术小节，并逐行去掉含路径、主机或凭据模板的行。
用法：python3 scripts/import_portable_knowledge.py <eve-portable 检出目录>
"""
import hashlib
import json
import pathlib
import re
import shutil
import subprocess
import sys

COMMIT = "e7e43b4db4de38775a8d28f59dd4ec69e5e1686e"
UPSTREAM = "https://gitee.com/zenthxsin/eve-portable"
CORPUS = "eve-portable"
DESTINATION = pathlib.Path(__file__).resolve().parent.parent / "vendor" / "eve-portable-knowledge"
SENSITIVE = re.compile(r"/home/|/root/|[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|sk-[A-Za-z0-9]{16,}"
                       r"|token|password|密码|QQ号|[1-9][0-9]{7,10}|sftp|sshpass|<SECRET>|<SERVER_IP>", re.I)
SAFE_PATH = re.compile(r"[a-z0-9][a-z0-9._-]*(/[a-z0-9][a-z0-9._-]*)*\.md")
# 旧长期记忆中可作为技术资料的小节；身份、关系与环境小节不导入。
MEMORY_SECTIONS = ["Mindustry 关键机制", "MultiCrafter 项目", "RingWorld 修复（NewHorizon）",
                   "MWU (Modular-Weapon-Unit)", "IE UI DSL", "Helium (he) Mod — v159.7", "Teleport-Unlock",
                   "通用技术笔记", "FluidShader 测试模组 (2026-08-14)"]


README = """# 继承自 eve-portable 的 Mindustry 资料

本目录是一份外部资料的固定副本，**不是本仓库 MIT 许可的一部分**。

- 来源：https://gitee.com/zenthxsin/eve-portable
- 版本：`e7e43b4db4de38775a8d28f59dd4ec69e5e1686e`
- 许可证：AGPL-3.0（见同目录 `LICENSE`，随副本一起提供）

内容由 `scripts/import_portable_knowledge.py` 从固定版本生成，重复运行结果相同；不要手工修改本目录。按固定版本 Mindustry 源码更正过的文件列在脚本的 `CORRECTED` 中，重新生成时保留现有内容。

| 目录 | 来源 | 说明 |
| --- | --- | --- |
| `mindustry/` | `knowledge/mindustry/` | 游戏各子系统的源码梳理 |
| `mod-making-guide/` | `knowledge/mod-making-guide/` | 模组制作指南与知名模组分析 |
| `concepts/` | `knowledge/concepts/` 中以 `mindustry-`、`arc-`、`json-`、`mdt-` 开头的文件 | 单个机制或问题的整理 |
| `mod-making/` | `knowledge/mod-making/` | 模组制作中的经验与反模式 |
| `analysis/` | `knowledge/analysis/` 中 Mindustry 与具体模组的技术分析（不含海报、旧助手框架与部署记录） | 崩溃诊断、迁移、校验与平衡分析 |
| `entities/` | `knowledge/entities/` 中的模组与工具条目（不含旧表情索引与 SCP 设定） | 知名模组与工具介绍 |
| `memory/technical-notes.md` | `memory/MEMORY.md` 的技术小节 | 节选，去掉了含本机路径、主机、凭据模板的行 |

只写着旧机器本地路径的引用说明行（`> ` 开头）会被去掉；其余命中敏感模式（本机路径、IP、长数字、令牌字样）的文件整篇排除，导入脚本会列出它们。`MANIFEST.json` 记录每个文件的 SHA-256；Eve 打开资料源时逐个核对，不符就拒绝启用。

Eve 通过受控研究读取这些文件，来源地址形如 `eve-corpus://eve-portable/mindustry/power-system.md`。这些资料是旧 Eve 整理的二手材料，结论对应的游戏版本以原文为准；研究摘录逐字引用原文，不把它们当作已经核实的源码证据。
"""


# 按固定版本源码更正过的文件：保留副本中的现有内容，不被上游原文覆盖（更正记录见提交 ccafd7e）。
CORRECTED = ["concepts/mindustry-version-numbering.md", "mindustry/java-mod-structure.md"]
# 问题分析只取 Mindustry 与具体模组的技术记录；旧助手框架、旧环境部署、海报与用户画像不导入。
ANALYSIS = re.compile(r"(mindustry|mindustryide|crystal-works|deepspace|voidshield|world-processor|newhorizon"
                      r"|tianlu|bundle|android|lmm)-")
# 模组条目中与 Mindustry 无关的：旧表情索引、SCP 设定。
ENTITY_EXCLUDED = {"emotes.md", "foundation-republic-foundation.md"}


def selected(source):
    knowledge = source / "knowledge"
    for directory in ("mindustry", "mod-making-guide", "mod-making"):
        yield from sorted((knowledge / directory).glob("*.md"))
    yield from sorted(path for path in (knowledge / "concepts").glob("*.md")
                      if re.match(r"(mindustry|arc|json|mdt)-", path.name))
    yield from sorted(path for path in (knowledge / "analysis").glob("*.md")
                      if ANALYSIS.match(path.name) and "poster" not in path.name)
    yield from sorted(path for path in (knowledge / "entities").glob("*.md") if path.name not in ENTITY_EXCLUDED)


def without_local_provenance(text):
    """去掉写着旧机器本地路径的引用说明行（以 `> ` 开头）；正文仍命中敏感模式的整篇排除。"""
    return "".join(line for line in text.splitlines(keepends=True)
                   if not (line.startswith("> ") and SENSITIVE.search(line)))


def memory_notes(source):
    text = (source / "memory" / "MEMORY.md").read_text(encoding="utf8")
    sections, current = {}, None
    for line in text.splitlines():
        if line.startswith("## "):
            current = line[3:].strip()
            sections[current] = []
        elif current is not None:
            sections[current].append(line)
    body = ["# 旧 Eve 长期记忆：技术节选", "",
            "> 节选自 eve-portable `memory/MEMORY.md` 的技术小节；去掉了含本机路径、主机、凭据模板的行。",
            "> 这些是旧 Eve 在实际项目中记下的经验，个别结论对应的游戏版本以原文为准。", ""]
    for name in MEMORY_SECTIONS:
        lines = [line for line in sections[name] if not SENSITIVE.search(line)]
        body += ["## " + name, *lines, ""]
    return "\n".join(body).rstrip() + "\n"


def main(argv):
    if len(argv) != 2:
        print(__doc__)
        return 2
    source = pathlib.Path(argv[1]).resolve()
    head = subprocess.run(["git", "-C", str(source), "rev-parse", "HEAD"], capture_output=True, text=True)
    if head.returncode != 0 or head.stdout.strip() != COMMIT:
        print("检出不是固定版本 %s" % COMMIT)
        return 1
    files, skipped = {}, []
    for path in selected(source):
        relative = path.relative_to(source / "knowledge").as_posix()
        text = without_local_provenance(path.read_text(encoding="utf8"))
        if not SAFE_PATH.fullmatch(relative) or SENSITIVE.search(text):
            skipped.append(relative)
            continue
        files[relative] = text
    files["memory/technical-notes.md"] = memory_notes(source)
    for relative in CORRECTED:
        corrected = DESTINATION / relative
        if not corrected.is_file():
            print("缺少更正过的文件 %s，拒绝生成以免丢失更正" % relative)
            return 1
        files[relative] = corrected.read_text(encoding="utf8")
    if DESTINATION.exists():
        shutil.rmtree(DESTINATION)
    DESTINATION.mkdir(parents=True)
    entries = []
    for relative, text in sorted(files.items()):
        target = DESTINATION / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        data = text.encode("utf8")
        target.write_bytes(data)
        entries.append({"path": relative, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)})
    shutil.copyfile(source / "LICENSE", DESTINATION / "LICENSE")
    (DESTINATION / "README.md").write_text(README, encoding="utf8")
    manifest = {"format_version": 1, "corpus": CORPUS, "upstream": UPSTREAM, "commit": COMMIT,
                "license": "AGPL-3.0", "files": entries}
    (DESTINATION / "MANIFEST.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=1) + "\n",
                                               encoding="utf8")
    print("导入 %d 篇，排除 %d 篇：%s" % (len(entries), len(skipped), "、".join(skipped)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
