"""真实原生便携包的离线更新验收；验证下载中断、切换、回退与用户文件保留。"""
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile
import threading
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def exercise(root, work, original_manifest, restart):
    copy = work / "更新运行器 中文 空格"
    shutil.copytree(root, copy)
    manifest = dict(original_manifest, version="v9.0.0")
    (copy / "build-info.json").write_text(json.dumps(manifest), encoding="utf8")
    (copy / "AGENT.md").write_text("# 合成自定义身份\n请保持这个文件原样。\n", encoding="utf8")
    (copy / "data/保留训练样本.txt").write_text("synthetic training fixture", encoding="utf8")
    config_before = (copy / "config.json").read_bytes()
    identity_before = (copy / "AGENT.md").read_bytes()
    state_before = (copy / "data/qq/state.json").read_bytes()
    label = manifest["platform_label"]
    extension = ".zip" if label.startswith("windows-") else ".tar.gz"
    folder = "Eve-v9.0.1-" + label
    archive = work / (folder + extension)
    launch = work / "candidate-Launch.mjs"
    launch.write_bytes(b"console.log('EVE_UPDATE_NATIVE_CANDIDATE');\n" + (root / "Launch.mjs").read_bytes())
    # 使用发行包中实际编译的程序与 Node，只有启动器标记和合成版本发生变化。
    candidate = dict(manifest, version="v9.0.1", files_sha256=dict(manifest["files_sha256"]))
    candidate["files_sha256"]["Launch.mjs"] = hashlib.sha256(launch.read_bytes()).hexdigest()
    metadata = work / "candidate-build-info.json"
    metadata.write_text(json.dumps(candidate), encoding="utf8")
    files = [(name, launch if name == "Launch.mjs" else root / name) for name in manifest["files_sha256"]]
    files.append(("build-info.json", metadata))
    if extension == ".zip":
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as zipped:
            for name, source in files:
                zipped.write(source, folder + "/" + name)
    else:
        with tarfile.open(archive, "w:gz") as tar:
            for name, source in files:
                tar.add(source, arcname=folder + "/" + name, recursive=False)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    release = {"tag_name": "v9.0.1", "draft": False, "prerelease": False, "assets": [{
        "name": archive.name, "size": archive.stat().st_size, "digest": "sha256:" + digest,
        "browser_download_url": "https://github.com/ZenthXSin/Eve.aic/releases/download/v9.0.1/" + archive.name}]}
    broken = True

    class Assets(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            if self.path == "/latest":
                body = json.dumps(release).encode()
                self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers()
                self.wfile.write(body)
            elif self.path == "/archive":
                self.send_response(200); self.send_header("Content-Length", str(archive.stat().st_size)); self.end_headers()
                with archive.open("rb") as reader:
                    if broken:
                        self.wfile.write(reader.read(1000)); self.wfile.flush(); self.close_connection = True
                    else:
                        shutil.copyfileobj(reader, self.wfile)
            else:
                self.send_error(404)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Assets)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
    node = copy / ("runtime/node.exe" if os.name == "nt" else "runtime/bin/node")
    helper = pathlib.Path(__file__).with_name("test-client-update-native.mjs")
    url = f"http://127.0.0.1:{server.server_port}"
    try:
        subprocess.run([str(node), str(helper), str(copy), url, "failure"], check=True, timeout=90)
        assert (copy / "data/qq/state.json").read_bytes() == state_before
        broken = False
        subprocess.run([str(node), str(helper), str(copy), url, "success"], check=True, timeout=120)
        assert (copy / "data/qq/state.json").read_bytes() == state_before
        restart(copy, True, "updated")
        subprocess.run([sys.executable, str(pathlib.Path(__file__).with_name("test-launcher-signal.py")), str(copy)],
                       check=True, timeout=90)
        subprocess.run([str(node), str(copy / "Launch.mjs"), "update", "--rollback"], check=True, timeout=45)
        restart(copy, False, "rolled-back")
        assert (copy / "config.json").read_bytes() == config_before
        assert (copy / "AGENT.md").read_bytes() == identity_before
        assert (copy / "data/保留训练样本.txt").read_text(encoding="utf8") == "synthetic training fixture"
    finally:
        server.shutdown(); server.server_close(); thread.join(timeout=5)
