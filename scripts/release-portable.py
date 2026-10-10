"""为主线分配发行版本，验证六平台产物，并在完整上传后公开 Release。"""
import argparse
import hashlib
import json
import os
import pathlib
import re
import subprocess
import time
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
TAG = re.compile(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[A-Za-z0-9][A-Za-z0-9.-]*)?")
LABELS = {"windows-x64", "windows-arm64", "linux-x64", "linux-arm64", "macos-x64", "macos-arm64"}
CI_NAMES = {"Rust 持续集成", "QQBot 通道离线验收", "PostgreSQL 状态与宿主验收", "自动性能测试"}


def validate_tag(tag):
    if not TAG.fullmatch(tag):
        raise ValueError("发行版本必须为 v主版本.次版本.补丁版本，可带预发布后缀")
    return tag


class GitHub:
    def __init__(self, repository):
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
            raise ValueError("仓库名称无效")
        self.repository = repository

    def api(self, route, *, method="GET", data=None, optional=False, pages=False):
        args = ["gh", "api", f"repos/{self.repository}/{route}", "--method", method]
        if pages:
            args.extend(["--paginate", "--slurp"])
        if data is not None:
            args.extend(["--input", "-"])
        result = subprocess.run(args, input=json.dumps(data) if data is not None else None,
                                capture_output=True, text=True, encoding="utf-8")
        if result.returncode:
            if optional and "HTTP 404" in result.stderr:
                return None
            raise RuntimeError(f"GitHub {method} {route} 失败：{result.stderr.strip()}")
        return json.loads(result.stdout) if result.stdout.strip() else None

    def upload(self, tag, files):
        subprocess.run(["gh", "release", "upload", tag, *(str(f) for f in files),
                        "--repo", self.repository, "--clobber"], check=True)


def commit_of(client, obj):
    for _ in range(8):
        if obj["type"] == "commit":
            return obj["sha"]
        if obj["type"] != "tag":
            break
        obj = client.api(f"git/tags/{obj['sha']}")["object"]
    raise ValueError("发行标签未指向可解析的提交")


def select_version(client, source, initial):
    stable = []
    for page in client.api("git/matching-refs/tags/v?per_page=100", pages=True):
        for ref in page:
            tag = ref["ref"].removeprefix("refs/tags/")
            match = TAG.fullmatch(tag)
            if match and "-" not in tag:
                stable.append((tuple(map(int, match.groups())), tag, ref["object"]))
    # 同一提交的重跑复用原版本，包括已存在的草稿或公开版本。
    for _, tag, obj in sorted(stable, reverse=True):
        if commit_of(client, obj) == source:
            return tag
    if not stable:
        return validate_tag("v" + initial)
    major, minor, patch = max(item[0] for item in stable)
    return f"v{major}.{minor}.{patch + 1}"


def prepare(client, event, ref, source, initial):
    automatic = ref == "refs/heads/main" and event in {"push", "workflow_dispatch"}
    tagged = ref.startswith("refs/tags/v") and event in {"push", "workflow_dispatch"}
    version = select_version(client, source, initial) if automatic else validate_tag(ref[10:]) if tagged else ""
    return {"release_tag": version, "publish": str(automatic or tagged).lower(),
            "automatic": str(automatic).lower()}


def digest(file):
    with file.open("rb") as reader:
        return hashlib.file_digest(reader, "sha256").hexdigest()


def verify(folder, tag, source):
    validate_tag(tag)
    reports = {}
    archives = []
    for file in sorted(folder.glob("acceptance-*.json")):
        report = json.loads(file.read_text(encoding="utf-8"))
        label = report["platform_label"]
        if label not in LABELS or label in reports or report["source_commit"] != source or report.get("version") != tag:
            raise ValueError("平台重复、缺失或源码/发行版本不一致")
        expected = f"Eve-{tag}-{label}." + ("zip" if label.startswith("windows-") else "tar.gz")
        if report["archive"] != expected:
            raise ValueError("发行包文件名与平台/版本不一致")
        archive = folder / expected
        if digest(archive) != report["archive_sha256"] or any(report.get(key) is not True for key in
                ("unicode_and_space_path", "web_http", "memory_and_learning_views", "qq_segmented_delivery", "recovery_without_replay",
                 "client_update_download_verified", "client_update_next_start", "client_update_rollback", "client_update_preserved_user_files")):
            raise ValueError("发行包散列或实际进程验收不符")
        if report.get("external_model_requests") != 0 or report.get("production_qq_connections") != 0:
            raise ValueError("发行验收不是独立替身环境")
        if report.get("node_launcher") is not True or report.get("console_ctrl_c") is not True:
            raise ValueError("Node 启动器没有通过实际 Ctrl+C 收尾验收")
        reports[label] = report
        archives.append(archive)
    if set(reports) != LABELS:
        raise ValueError("六个平台没有全部完成原生验收，不发布部分 Release")
    archives.sort()
    sums = folder / "SHA256SUMS.txt"
    sums.write_text("".join(f"{digest(file)}  {file.name}\n" for file in archives), encoding="utf-8")
    evidence = folder / "release-verification.json"
    evidence.write_text(json.dumps(reports, ensure_ascii=False, indent=2), encoding="utf-8")
    return [*archives, sums, evidence]


def wait_for_ci(client, source, timeout=1200):
    deadline = time.monotonic() + timeout
    while True:
        if client.api("git/ref/heads/main")["object"]["sha"] != source:
            print("主线已经更新，本次旧提交不自动发布；由新提交工作流接续。")
            return False
        runs = client.api(f"actions/runs?head_sha={source}&event=push&per_page=100")["workflow_runs"]
        latest = {}
        for run in sorted(runs, key=lambda r: r["id"], reverse=True):
            if run["name"] in CI_NAMES:
                latest.setdefault(run["name"], run)
        failed = [run for run in latest.values() if run["status"] == "completed" and run["conclusion"] != "success"]
        if failed:
            raise ValueError("精确主线 CI 未通过：" + ", ".join(run["html_url"] for run in failed))
        # Rust 每次 main push 必跑；有路径过滤的其他工作流仅在该提交触发时等待。
        if "Rust 持续集成" in latest and all(run["status"] == "completed" for run in latest.values()):
            return True
        if time.monotonic() >= deadline:
            raise TimeoutError("等待精确主线 CI 超时；没有创建标签或公开 Release")
        time.sleep(15)


def ensure_tag(client, tag, source):
    ref = client.api(f"git/ref/tags/{tag}", optional=True)
    if ref is None:
        try:
            ref = client.api("git/refs", method="POST", data={"ref": f"refs/tags/{tag}", "sha": source})
        except RuntimeError:
            # 并行手动标签可能先占用版本；只接受同一提交，不移动已有标签。
            ref = client.api(f"git/ref/tags/{tag}", optional=True)
            if ref is None:
                raise
    if commit_of(client, ref["object"]) != source:
        raise ValueError("版本标签已指向其他提交，请重跑以选择新版本；不覆盖旧标签")


def publish(client, folder, tag, source, automatic):
    files = verify(folder, tag, source)
    if automatic and not wait_for_ci(client, source):
        return None
    ensure_tag(client, tag, source)
    release = client.api(f"releases/tags/{tag}", optional=True)
    expected_names = {file.name for file in files}
    if release and not release["draft"]:
        if {asset["name"] for asset in release["assets"]} != expected_names:
            raise ValueError("版本已公开但资产不完整，不改写公开版本")
        print("同一提交的版本已公开，保留现有文件：" + release["html_url"])
        return release["html_url"]
    notes = (f"# Eve {tag}\n\n"
             "Windows、Linux、macOS，各提供 x64 与 ARM64 原生运行包。包含运行器、Web 页面、固定 Node 和 QQ 桥接，无需安装 Rust 或 Node。\n\n"
             "完整解压后使用包内 Start-Eve 启动，填写自己的主模型与 QQ 密钥；Open-Panel 打开本机面板。Windows 使用 ZIP，Linux/macOS 使用 tar.gz。\n\n"
             "六个平台均通过原生进程、中文空格路径、Web、分段投递和恢复零重放验收。Linux 需 Ubuntu 24.04 或相容 glibc，macOS 需 11+；macOS 包未签名/公证。\n\n"
             f"源码提交：`{source}`。文件散列见 SHA256SUMS.txt；六平台证据见 release-verification.json。包内不含用户凭据和训练数据。\n")
    if release is None:
        release = client.api("releases", method="POST", data={"tag_name": tag, "target_commitish": source,
                             "name": f"Eve {tag}", "body": notes, "draft": True, "prerelease": "-" in tag})
    client.upload(tag, files)
    uploaded = client.api(f"releases/{release['id']}")
    assets = {asset["name"]: asset for asset in uploaded["assets"]}
    if set(assets) != expected_names or any(assets[file.name].get("digest") != "sha256:" + digest(file)
                                          or assets[file.name]["size"] != file.stat().st_size for file in files):
        raise ValueError("远端资产不完整或 SHA-256 不符，保留草稿供重跑")
    if not uploaded["draft"]:
        raise ValueError("草稿状态已由外部修改，不覆盖公开版本")
    if automatic and client.api("git/ref/heads/main")["object"]["sha"] != source:
        print("上传期间主线更新，保留草稿，不将旧提交标为最新版本。")
        return None
    result = client.api(f"releases/{release['id']}", method="PATCH", data={"draft": False, "body": notes,
                        "prerelease": "-" in tag, "make_latest": "false" if "-" in tag else "true"})
    print(result["html_url"])
    return result["html_url"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("prepare", "publish"))
    parser.add_argument("--folder", type=pathlib.Path, default=pathlib.Path("release-assets"))
    args = parser.parse_args()
    client = GitHub(os.environ["GITHUB_REPOSITORY"])
    source = os.environ["GITHUB_SHA"]
    if args.command == "prepare":
        initial = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
        outputs = prepare(client, os.environ["GITHUB_EVENT_NAME"], os.environ["GITHUB_REF"], source, initial)
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as writer:
            for key, value in outputs.items():
                writer.write(f"{key}={value}\n")
        print(json.dumps(outputs, ensure_ascii=False))
    else:
        publish(client, args.folder, os.environ["EVE_RELEASE_TAG"], source, os.environ["EVE_RELEASE_AUTOMATIC"] == "true")


if __name__ == "__main__":
    main()
