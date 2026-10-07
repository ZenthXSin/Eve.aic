"""兼容 Windows 验收入口；共用实际解压和原生进程验收。"""
import os
import pathlib
import runpy

if __name__ == "__main__":
    if os.name != "nt":
        raise SystemExit("该入口仅用于 Windows；其他平台使用 test-portable-package.py")
    runpy.run_path(str(pathlib.Path(__file__).with_name("test-portable-package.py")), run_name="__main__")
