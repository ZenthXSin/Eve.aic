# Eve.aic

Rust 认知系统底座：稳定插件 Runtime 与主模型闭环，持续推进记忆、驱动和认知能力。

## 当前能力

- 插件注册/卸载、SemVer 依赖、异步生命周期、失败回滚及逆序清理。
- 事件、服务、状态、任务、日志与权限公开契约；后端可替换。
- 文件状态持久化、目录独占、严格恢复、诊断与停止错误汇总。
- 普通配置的来源优先级、请求快照、版本、备份与回滚。
- Mock/Responses/Chat 主模型、插件上下文、工具批次执行和结果回传。
- AGENT.md 显式来源、内容版本和固定身份快照。
- 多轮会话、完整工具历史、重启恢复；失败输入不进入完成历史，旧工具不重放。
- 独立流式输出、任务取消、代际过滤；QQ 支持明确补充、纠正和取消。

## 运行

Rust 1.89+。在仓库根目录配置主模型及宿主凭据：

```powershell
$env:EVE_OPENAI_MODEL = "deepseek-v4.1-flash"
$env:EVE_OPENAI_PROTOCOL = "chat"
$env:EVE_OPENAI_API_KEY = "<宿主凭据>"
cargo run -p eve-app --locked -- --state-dir ./.eve --agent ./AGENT.md
```

每行一轮，/cancel 取消，/quit 或 Ctrl+C 退出，EOF 处理完队列。同目录、用户和会话恢复历史；当前串行、非流式，内置 echo 工具。详见[核心对话](./docs/核心对话.md)。

```bash
cargo test --workspace --locked
cargo run -p eve-runtime --locked
```

测试使用本地 HTTP；最后一条运行插件协作验收。

## 计划与边界

核心、真实主模型及 QQ 沙箱已验收；QQ 明确控制仅完成离线验收。认知循环及期限修复已合入；生产认知、辅助模型、长期记忆继续推进，Jev 与 Web 后置，见[开发计划](./docs/开发计划.md)。

单进程可信插件，权限检查不是沙箱；定义、实现、组合分层。密钥由宿主提供；退出显式收尾。

[文档中心](./docs/README.md) · [企划案](./docs/企划案.md) · 中文 PR 更新 · MIT
