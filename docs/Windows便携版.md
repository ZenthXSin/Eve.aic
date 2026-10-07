# Windows exe 与便携发行包

面向 Windows 10/11 x64 的发行物为 `Eve-windows-x64-<提交>.zip`，包含原生 MSVC exe、内嵌 Web 页面、固定 Node 22.22.0 与锁定 QQ SDK、启动脚本和无密钥配置示例。使用方法见[随包说明](../packaging/windows/使用说明.md)。不包含用户会话、训练样本或真实密钥。

`Windows 便携版打包` 工作流在 Windows runner 上用 Rust 1.89、`x86_64-pc-windows-msvc` 和静态 CRT 构建五个入口，直接运行帮助；下载官方 Node ZIP，复核固定 SHA-256 和版本，再从锁文件安装纯 JS 桥接依赖。`scripts/package-windows.ps1` 只复制明确列出的发行文件，不递归打包仓库或工作目录。输出源码提交、树、逐文件 SHA-256 与发行 ZIP 校验值。

`scripts/test-windows-package.py` 解压实际 ZIP 到含中文、空格的临时目录，检查文件散列与空密钥模板，然后使用内置 exe/Node 和 Windows PowerShell 5.1 启动。仅连接回环模型与 QQ 替身，验证面板鉴权、认知与记忆/学习读取、两段投递、关闭和第二次启动零重放；它不连接正式 QQ 或外部模型。通过后工作流上传 ZIP、校验清单与验收 JSON，保存 30 天。

源码构建可在有 Rust 1.89/MSVC、Node 和 PowerShell 7 的 Windows 开发机器执行：

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo +1.89.0 build -p eve-app --bins --release --locked --target x86_64-pc-windows-msvc
./scripts/package-windows.ps1
python scripts/test-windows-package.py dist
```

用户运行发行包只需系统自带 Windows PowerShell 5.1，不需要上述构建工具。`config.json` 与 `data` 留在本机；读取无效配置和损坏状态会失败，不自动重置。迁移与升级先退出并备份原文件，再复制到新版目录；不同宿主模式使用独立状态目录。启动脚本只给当前子进程提供凭据和功能参数，沿用 Rust 宿主对 Node 凭据隔离、回执与恢复的原有契约。
