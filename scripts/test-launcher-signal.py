"""只创建自己的控制台/进程，验证 Node 启动器等待 Rust 的 Ctrl+C 收尾。"""
import ctypes
import json
import os
import pathlib
import queue
import signal
import subprocess
import sys
import tempfile
import threading
import urllib.request


def main():
    root = pathlib.Path(sys.argv[1]).resolve()
    console = None
    handler = None
    if os.name == "nt":
        # 在独立辅助进程的新控制台广播，不能向 runner 的控制台发信号。
        from ctypes import wintypes
        console = ctypes.WinDLL("kernel32", use_last_error=True)
        console.FreeConsole()
        if not console.AllocConsole():
            raise OSError(ctypes.get_last_error(), "无法分配本测试的独立控制台")
        callback = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.DWORD)
        handler = callback(lambda event: event in (0, 1))
        console.SetConsoleCtrlHandler.argtypes = [callback, wintypes.BOOL]
        if not console.SetConsoleCtrlHandler(handler, True):
            raise OSError(ctypes.get_last_error(), "无法安装辅助进程信号处理器")
    try:
        with tempfile.TemporaryDirectory(prefix="eve-owned-signal-") as temporary:
            bridge = pathlib.Path(temporary) / "bridge.mjs"
            bridge.write_text('''import readline from 'node:readline';
process.on('SIGINT', () => {});
process.stdout.write(JSON.stringify({version:1,type:'ready'})+'\\n');
for await (const line of readline.createInterface({input:process.stdin})) {
  if (JSON.parse(line).type === 'stop') break;
}
process.stdout.end(() => process.exit(0));
''', encoding="utf-8")
            node = root / ("runtime/node.exe" if os.name == "nt" else "runtime/bin/node")
            child = subprocess.Popen([str(node), str(root / "Launch.mjs"), "qq", "--no-prompt", "--bridge-script", str(bridge)],
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
            lines = []
            ready = queue.Queue()
            web = queue.Queue()

            def read(stream):
                for line in stream:
                    lines.append(line)
                    if line.startswith("EVE_QQBOT_READY"):
                        ready.put(True)
                    if line.startswith("EVE_WEB_READY http://"):
                        web.put(line.split("EVE_WEB_READY ", 1)[1].strip())

            readers = [threading.Thread(target=read, args=(stream,), daemon=True)
                       for stream in (child.stdout, child.stderr)]
            for reader in readers:
                reader.start()
            try:
                ready.get(timeout=30)
                url = web.get(timeout=5)
                token = json.loads((root / "config.json").read_text(encoding="utf-8"))["web"]["token"]
                with urllib.request.urlopen(urllib.request.Request(url + "/api/status", headers={"Authorization": "Bearer " + token}), timeout=5) as response:
                    if response.status != 200:
                        raise RuntimeError("信号发送前宿主没有进入工作状态")
                if os.name == "nt":
                    if not console.GenerateConsoleCtrlEvent(0, 0):
                        raise OSError(ctypes.get_last_error(), "无法向本测试控制台发送 Ctrl+C")
                else:
                    child.send_signal(signal.SIGINT)
                child.wait(timeout=30)
                for reader in readers:
                    reader.join(timeout=3)
                if child.returncode != 0:
                    raise RuntimeError("Node 启动器没有完成正常收尾：" + "".join(lines))
                reports = []
                for line in lines:
                    try:
                        value = json.loads(line)
                        if isinstance(value, dict) and "closed" in value:
                            reports.append(value)
                    except json.JSONDecodeError:
                        pass
                if len(reports) != 1 or not reports[0]["closed"] or reports[0]["terminal_error"]:
                    raise RuntimeError("未取得 Rust 实际关闭确认，不能把强制终止当作正常收尾：" + "".join(lines))
                print(json.dumps({"console_ctrl_c": True, "rust_closed": True, "owned_processes_only": True}))
            finally:
                if child.poll() is None:
                    if os.name == "nt":
                        subprocess.run(["taskkill", "/PID", str(child.pid), "/T", "/F"],
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
                    else:
                        child.terminate()
                        try:
                            child.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            child.kill()
                    child.wait(timeout=10)
                for stream in (child.stdout, child.stderr):
                    stream.close()
    finally:
        if console:
            console.FreeConsole()


if __name__ == "__main__":
    main()
