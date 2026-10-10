"""以当前平台原生构建产物生成明确文件清单的便携包，不读取宿主密钥或用户状态。"""
import argparse
import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
NODE_VERSION = "22.22.0"
TARGETS = {
    "x86_64-pc-windows-msvc": ("windows-x64", "win-x64", "zip", "c97fa376d2becdc8863fcd3ca2dd9a83a9f3468ee7ccf7a6d076ec66a645c77a"),
    "aarch64-pc-windows-msvc": ("windows-arm64", "win-arm64", "zip", "5b44fd410df7b4cd0a1891a05a7b606f8fb7d8786a94997b996a372e82478d7a"),
    "x86_64-unknown-linux-gnu": ("linux-x64", "linux-x64", "tar.xz", "9aa8e9d2298ab68c600bd6fb86a6c13bce11a4eca1ba9b39d79fa021755d7c37"),
    "aarch64-unknown-linux-gnu": ("linux-arm64", "linux-arm64", "tar.xz", "1bf1eb9ee63ffc4e5d324c0b9b62cf4a289f44332dfef9607cea1a0d9596ba6f"),
    "x86_64-apple-darwin": ("macos-x64", "darwin-x64", "tar.gz", "5ea50c9d6dea3dfa3abb66b2656f7a4e1c8cef23432b558d45fb538c7b5dedce"),
    "aarch64-apple-darwin": ("macos-arm64", "darwin-arm64", "tar.gz", "5ed4db0fcf1eaf84d91ad12462631d73bf4576c1377e192d222e48026a902640"),
}
BINARIES = ("eve", "eve-qqbot", "eve-cognition", "eve-memory", "eve-message-evaluate")


def digest(file):
    with file.open("rb") as reader:
        return hashlib.file_digest(reader, "sha256").hexdigest()


def checked(command, **kwargs):
    return subprocess.run(command, check=True, **kwargs)


def package(target, binary_directory, output_directory):
    label, node_platform, node_extension, pinned_digest = TARGETS[target]
    windows = label.startswith("windows-")
    if windows != (os.name == "nt") or label.startswith("macos-") != (os.uname().sysname == "Darwin" if os.name != "nt" else False):
        raise RuntimeError("打包必须使用相符平台的原生运行环境。")
    source = checked(["git", "-C", str(ROOT), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    tree = checked(["git", "-C", str(ROOT), "rev-parse", "HEAD^{tree}"], capture_output=True, text=True).stdout.strip()
    version = os.environ.get("EVE_RELEASE_VERSION", "") or (os.environ.get("GITHUB_REF_NAME", "") if os.environ.get("GITHUB_REF_TYPE") == "tag" else "")
    if version and not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[A-Za-z0-9][A-Za-z0-9.-]*)?", version):
        raise RuntimeError("发布标签必须为 v主版本.次版本.补丁版本，可带预发布后缀。")
    name = f"Eve-{version or source[:7]}-{label}"
    output_directory.mkdir(parents=True, exist_ok=True)
    bundle = output_directory / name
    bundle.mkdir()  # 不覆盖旧发行包。
    extension = ".exe" if windows else ""
    for binary in BINARIES:
        shutil.copy2(binary_directory / (binary + extension), bundle)
    shutil.copy2(ROOT / "AGENT.md", bundle)
    shutil.copy2(ROOT / "packaging/windows/config.example.json", bundle)
    for file in ("Launch.mjs", "Updater.mjs", "update-contract.mjs", "update-archive.mjs"):
        shutil.copy2(ROOT / "packaging/common" / file, bundle)
    if windows:
        for file in ("Launch.ps1", "Open-Panel.ps1", "Start-Eve.cmd", "Start-Console.cmd", "Open-Panel.cmd", "Update-Eve.cmd", "使用说明.md"):
            source_file = ROOT / "packaging/windows" / file
            if source_file.suffix == ".ps1":
                (bundle / source_file.name).write_text(source_file.read_text(encoding="utf-8"), encoding="utf-8-sig")
            elif source_file.suffix == ".cmd":
                (bundle / source_file.name).write_bytes(source_file.read_text().replace("\n", "\r\n").encode("ascii"))
            else:
                shutil.copy2(source_file, bundle)
        if label == "windows-arm64":
            for script, mode in (("Start-Eve", "qq"), ("Start-Console", "console"), ("Open-Panel", "panel"), ("Update-Eve", "update")):
                (bundle / f"{script}.cmd").write_bytes((
                    '@echo off\r\nchcp 65001 >nul\r\n'
                    '"%~dp0runtime\\node.exe" "%~dp0Launch.mjs" ' + mode + ' %*\r\npause\r\n').encode("ascii"))
    else:
        for file in ("使用说明.md",):
            shutil.copy2(ROOT / "packaging/posix" / file, bundle)
        script_extension = "command" if label.startswith("macos-") else "sh"
        for script, mode in (("Start-Eve", "qq"), ("Start-Console", "console"), ("Open-Panel", "panel"), ("Update-Eve", "update")):
            shell = bundle / f"{script}.{script_extension}"
            shell.write_text('#!/bin/sh\nEVE_PACKAGE_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1\nexec "$EVE_PACKAGE_DIR/runtime/bin/node" "$EVE_PACKAGE_DIR/Launch.mjs" ' + mode + ' "$@"\n', encoding="utf-8")
            shell.chmod(0o755)
    bridge = bundle / "connectors/qqbot"
    bridge.mkdir(parents=True)
    for file in ("bridge.mjs", "bridge-core.mjs", "package.json", "package-lock.json", "THIRD_PARTY.md"):
        shutil.copy2(ROOT / "connectors/qqbot" / file, bridge)
    runtime = bundle / "runtime"
    runtime.mkdir()
    with tempfile.TemporaryDirectory(prefix="eve-node-package-") as temporary:
        temp = pathlib.Path(temporary)
        archive_name = f"node-v{NODE_VERSION}-{node_platform}.{node_extension}"
        node_archive = temp / archive_name
        with urllib.request.urlopen(f"https://nodejs.org/dist/v{NODE_VERSION}/{archive_name}", timeout=60) as response, node_archive.open("wb") as writer:
            shutil.copyfileobj(response, writer)
        if digest(node_archive) != pinned_digest:
            raise RuntimeError("Node 下载 SHA-256 不符。")
        if windows:
            with zipfile.ZipFile(node_archive) as zipped:
                zipped.extractall(temp)
        else:
            with tarfile.open(node_archive) as tar:
                tar.extractall(temp, filter="data")
        node_root = temp / f"node-v{NODE_VERSION}-{node_platform}"
        if windows:
            node = runtime / "node.exe"
            shutil.copy2(node_root / "node.exe", node)
            npm = node_root / "node_modules/npm/bin/npm-cli.js"
        else:
            (runtime / "bin").mkdir()
            node = runtime / "bin/node"
            shutil.copy2(node_root / "bin/node", node)
            node.chmod(0o755)
            npm = node_root / "lib/node_modules/npm/bin/npm-cli.js"
        shutil.copy2(node_root / "LICENSE", runtime / "NODE-LICENSE.txt")
        reported = checked([str(node), "--version"], capture_output=True, text=True).stdout.strip()
        if reported != f"v{NODE_VERSION}":
            raise RuntimeError("发行包 Node 版本错误。")
        checked([str(node), str(npm), "ci", "--omit=dev", "--ignore-scripts", "--no-audit", "--no-fund"], cwd=bridge)
    checked([str(node), "--input-type=module", "-e", 'await import("@tencent-connect/qqbot-nodejs");'], cwd=bridge)
    for binary in BINARIES:
        checked([str(bundle / (binary + extension)), "--help"], stdout=subprocess.DEVNULL)
    files = {file.relative_to(bundle).as_posix(): digest(file) for file in sorted(bundle.rglob("*")) if file.is_file()}
    manifest = {"format_version": 1, "source_commit": source, "source_tree": tree, "version": version,
                "target": target, "platform_label": label, "rust_version": "1.89.0", "static_crt": windows,
                "node_version": NODE_VERSION, "node_archive_sha256": pinned_digest,
                "credentials_included": False, "user_data_included": False, "files_sha256": files,
                "updater_protocol": 1, "state_schema_generation": 1}
    (bundle / "build-info.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8")
    if windows:
        archive = pathlib.Path(shutil.make_archive(str(output_directory / name), "zip", root_dir=output_directory, base_dir=name))
    else:
        archive = output_directory / (name + ".tar.gz")
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(bundle, arcname=name)
    checksum = digest(archive)
    (output_directory / f"SHA256SUMS-{label}.txt").write_text(f"{checksum}  {archive.name}\n", encoding="utf-8")
    print(json.dumps({"archive": str(archive), "sha256": checksum, "source_commit": source, "target": target}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--binary-directory", type=pathlib.Path)
    parser.add_argument("--output-directory", type=pathlib.Path, default=ROOT / "dist")
    arguments = parser.parse_args()
    binaries = arguments.binary_directory or ROOT / "target" / arguments.target / "release"
    package(arguments.target, binaries.resolve(), arguments.output_directory.resolve())
