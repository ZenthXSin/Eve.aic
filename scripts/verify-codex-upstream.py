#!/usr/bin/env python3
"""验证 vendored Codex 原文件与锁定提交的哈希，不访问网络或运行 Cargo。"""
import hashlib
import json
from pathlib import Path
import re
import sys
import tomllib


def fail(message):
    raise ValueError(message)


def main():
    repository = Path(__file__).resolve().parents[1]
    root = repository / "vendor/codex-0.162.0"
    manifest = json.loads((root / "UPSTREAM_FILES.json").read_text())
    if manifest["commit"] != "c1382380de69521303b416720a52f42d51af6248":
        fail("Codex 锁定提交与验证器不一致")
    if manifest.get("failures"):
        fail("Codex 原文件清单仍有下载失败")
    seen = set()
    size = 0
    for entry in manifest["files"]:
        relative = Path(entry["path"])
        if relative.is_absolute() or ".." in relative.parts or relative in seen:
            fail("Codex 清单存在重复或越界路径")
        seen.add(relative)
        path = root / relative
        if path.is_symlink() or not path.is_file():
            fail(f"Codex 原文件缺失或变为符号链接: {relative}")
        data = path.read_bytes()
        size += len(data)
        if len(data) != entry["bytes"] or hashlib.sha256(data).hexdigest() != entry["sha256"]:
            fail(f"Codex 原文件内容改变: {relative}")
        blob = hashlib.sha1(f"blob {len(data)}\0".encode() + data).hexdigest()
        if blob != entry["git_blob_sha"]:
            fail(f"Codex 原文件 Git blob 不匹配: {relative}")
        upstream = entry.get("upstream_path", entry["path"])
        source = f'https://raw.githubusercontent.com/openai/codex/{manifest["commit"]}/{upstream}'
        if source != entry["source"]:
            fail(f"Codex 原文件来源不匹配: {relative}")

    workspace = root / "codex-rs"
    composition = tomllib.loads((workspace / "Cargo.toml").read_text())
    dependencies = composition["workspace"]["dependencies"]
    package_count = 0
    for member in composition["workspace"]["members"]:
        package_count += 1
        member_root = workspace / member
        package = tomllib.loads((member_root / "Cargo.toml").read_text())
        # 原 Cargo.toml 的直接、构建、测试和跨平台本地依赖必须完整。
        tables = [package]
        tables.extend(package.get("target", {}).values())
        for table in tables:
            for kind in ("dependencies", "build-dependencies", "dev-dependencies"):
                for name, options in table.get(kind, {}).items():
                    if not isinstance(options, dict):
                        continue
                    base = member_root
                    if options.get("workspace"):
                        options = dependencies[name]
                        base = workspace
                    if isinstance(options, dict) and "path" in options:
                        local = (base / options["path"]).resolve()
                        if not (local / "Cargo.toml").is_file():
                            fail(f"Codex 原 manifest 的本地依赖缺失: {member}: {name}")
        # 包目录中每个原文件都必须在来源清单里，禁止悄悄添加替代源码。
        for path in member_root.rglob("*"):
            if path.is_file() and path.relative_to(root) not in seen:
                fail(f"Codex 包内存在未列入原文件清单的文件: {path.relative_to(root)}")

    for relative in seen:
        if relative.suffix != ".rs":
            continue
        path = root / relative
        for include in re.findall(r'include_(?:str|bytes)!\s*\(\s*"([^"\n]+)"', path.read_text()):
            included = (path.parent / include).resolve()
            if not included.is_relative_to(root) or not included.is_file():
                fail(f"Codex 编译内嵌资源缺失: {relative}")

    print(json.dumps({
        "commit": manifest["commit"], "original_files": len(seen),
        "original_bytes": size, "local_packages": package_count,
        "sha256_and_git_blob": "verified", "local_dependencies": "present",
        "note": "只验证原文件来源与静态闭包；不代表 Cargo 构建或运行验收通过",
    }, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except (KeyError, ValueError, OSError) as error:
        print(f"Codex 原文件校验失败: {error}", file=sys.stderr)
        raise SystemExit(1)
