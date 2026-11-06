# Codex 原源码来源

此目录复制自官方 `openai/codex` 的 `rust-v0.162.0` 标签，提交为 `c1382380de69521303b416720a52f42d51af6248`。原文件适用 [Apache-2.0](LICENSE)，原 [NOTICE](NOTICE) 保留。

`UPSTREAM_FILES.json` 记录 577 个原文件的路径、字节数、SHA-256、Git blob SHA 和固定提交下载地址。原包的 Rust 文件、Cargo.toml、内嵌资源均逐字节保留。执行 `python3 scripts/verify-codex-upstream.py` 可验证来源与本地依赖闭包。

保留 24 个本地 package，覆盖 `codex-api`、`codex-client`、`codex-protocol` 的运行时、跨平台和 manifest 测试依赖。实际请求调用原 `ResponsesClient`、`ReqwestTransport`、退避重试和 Responses SSE parser。

`codex-rs/Cargo.toml` 是 Eve 为这 24 个包生成的构建组合文件，不是原工作区根文件。上游完整工作区 Cargo.toml、Cargo.lock 与 Rust 1.95.0 工具链声明保存在 `provenance/codex-rs/`。第三方依赖由 Eve 顶层 Cargo.lock 最终锁定；原 fork 的固定 revision 由 Eve 顶层 patch 继承。构建请使用 Rust 1.95 或更新兼容版本。

Eve 类型转换、请求配置扩展与生命周期适配位于 `crates/llm-codex`，不混入此处原源码。该适配器只支持 Responses；不会把 Chat Completions 伪装成原 Codex 能力，也不会替换 Eve 的工具执行、安全规则、存储和任务调度。
