# Codex 依赖锁解析记录

本次只证明依赖解析、锁一致性和原文件来源；构建、Clippy、测试、QQ/Web 实际进程及外部中转兼容性仍待验证。

## 来源与结果

| 项目 | 精确记录 |
| --- | --- |
| Actions | [运行 42162196097，attempt 1](https://github.com/ZenthXSin/Eve.aic/actions/runs/42162196097) |
| 实现源码 | `34a79cb6bac054b24a14dd880864283f12d2e7f3` |
| 工作流提交 | `f53a0624f1099cef8e1fbd78c9fdaca25bd79939`，比实现源码只多锁解析工作流 |
| 实际工具链 | rustc `1.95.0 (59807616e 2026-04-14)`，cargo `1.95.0 (f2d3ce0bd 2026-03-21)` |
| Artifact | `13476828373`，711,669 字节，22 个证据文件解压共 8,330,978 字节 |
| Artifact SHA-256 | `19d9c092d89be7ee9df4d83e8662cef2e2eb985dc0d1bb902719ced1415a8fd2` |
| 原锁 SHA-256 | `8bab6d7fd5c0fcd0d8266de8bdcea30db93eaa317e936e6ac2529b25318299e9` |
| 新锁 SHA-256 | `6ce14d97597b52d7fc0e0d3843036361e45408885776627cfbfce58bca63c685` |

工作流固定 checkout 实现提交，使用独立 Cargo 目录、只读仓库权限，不引用生产 secrets、不缓存 token、不提交仓库。沿用旧锁执行真实 `cargo +1.95.0 metadata --format-version 1`，显式设置 `resolver.incompatible-rust-versions=fallback`；随后执行完整 `--locked --offline` metadata 和 `--no-deps --locked --offline` 工作区目标读取。锁前后一致，仓库最终仅 Cargo.lock 有变化，原 577 文件的摘要和 Git blob 在前后均通过。下载后再核对 GitHub 提供的压缩包摘要及内部 21 个文件摘要，Cargo.lock 按原字节提交，没有手工生成或改写包条目。

## 闭包与最低版本

完整 metadata 包含 726 个 package、726 个 resolve node；Eve 工作区为 106 个成员和 330 个目标（106 lib、186 test、30 example、8 bin），vendor 不在 workspace_members 中。保存的 24 个原本地 package 中，本次真实闭包解析到 17 个；其余仍按原 manifest 依赖保留，不能将原文件保留范围误称为本次实际编译范围。

两个实际 Git 依赖均与原固定来源一致：`tokio-tungstenite 0.28.0` 对应 `openai-oss-forks/tokio-tungstenite@0e5b2d73aa18dd9f0a50ee9ff199d5aef7594186`，`tungstenite 0.27.0` 对应 `openai-oss-forks/tungstenite-rs@4fffad30fe373adbdcffab9545e9e9bf4f2fc19f`。`runfiles` 没有进入本次闭包。没有发现声明 rust-version 高于 1.95 的包；193 个包未声明最低版本，这些仍必须用真实 1.95 编译验证，fallback 和 metadata 成功均不提供编译保证。

旧锁有 309 个包，新锁保留 302 个原条目并保持校验和，新增 424 个条目、替换 7 个旧版本。新增 Codex 的 ICU decimal/locale/plural 依赖选择 2.4 后，要求 locale/provider 2.4、tinystr 0.8.5、yoke 0.8.4、zerovec 0.11.9 及对应 derive，因此原 7 个相关版本随约束升级。其他保留包的依赖边变化来自新增特性和多版本名称消歧；没有替换其原校验和。

两份完整 metadata 的包、版本、目标、特性和依赖集合相同。首次在线解析的 tungstenite 节点有 10 处重复且内容相同的 `dep_kinds` 条目，离线锁读取时去重；两份原始 JSON 均保留，不能宣称原始文件逐字节相等。

## 容量与后续验收

远端磁盘可用空间由 94,133,428,224 B 变为 92,802,969,600 B，差值 1,330,458,624 B 包含工具链安装、Git/crate 下载和证据，不是单独 crate 容量或峰值。先前本机 128 MiB 配额的唯一 metadata 尝试在首个 Git 连接 45 秒超时，锁未变；该错误不能证明固定 revision 不存在。本次只向本机取回小型证据，没有安装工具链、下载依赖源码或编译。

后续对提交准确锁的最终 SHA 运行 Linux Rust 1.95 全目标 check、公共会话/两适配器/主辅组合/切换/Runtime 定向测试、严格 Clippy，以及 QQ/Web 真实进程和 Linux Stable 完整回归。用户已要求停止 Windows 支持，本功能不再以 Windows 验收作为完成条件，原上游文件仍逐字节保留。工作流配置、锁解析、编译和实际运行必须分别记录。临时锁解析工作流只在专用分支修改自身文件时运行；普通 Cargo.lock 推送只触发常规验收。
