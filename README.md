# Eve.aic

用 Rust 构建可持续运行、可扩展、可演化的认知系统底座。先做好稳定的插件 Runtime 和主模型闭环，再逐步接入记忆、人格、驱动与认知能力。

## 当前能力

- 插件注册/卸载、SemVer 依赖、异步生命周期、失败回滚及逆序清理。
- 事件、服务、状态、任务、日志与权限公开契约；后端可替换。
- 文件状态持久化、目录独占、严格恢复、诊断与停止错误汇总。
- 普通配置的来源优先级、请求快照、版本、备份与回滚。
- Mock/Responses/Chat 主模型、插件上下文、工具批次执行和结果回传。
- AGENT.md 显式来源、内容版本和固定身份快照。
- 多轮会话、完整工具历史、重启恢复；失败输入不进入完成历史，旧工具不重放。
- 独立流式输出、任务取消、代际事件过滤与消息关系基础。

## 运行

Rust 1.89+。在仓库根目录配置主模型及宿主凭据：

```powershell
$env:EVE_OPENAI_MODEL = "deepseek-v4.1-flash"
$env:EVE_OPENAI_PROTOCOL = "chat"
$env:EVE_OPENAI_API_KEY = "<宿主凭据>"
cargo run -p eve-app --locked -- --state-dir ./.eve --agent ./AGENT.md
```

每行一轮，/cancel 取消当前轮，/quit 或 Ctrl+C 取消并退出；EOF 处理完队列。相同目录、用户与会话恢复历史。当前核心入口串行、非流式，内置本地 echo 验证工具闭环。参数、Provider 兼容性与失败语义见[核心对话](./docs/核心对话.md)。

```bash
cargo test --workspace --locked
cargo test -p eve-app --test console --locked -- --nocapture
cargo run -p eve-runtime --locked
```

最后一条运行既有插件协作验收。核心进程测试使用本地 HTTP，不代表新增外部模型质量验收。

## 计划与边界

核心入口已接入对话、工具、状态、在途取消和退出收尾；当前默认 deepseek-v4.1-flash / Chat，沿用既有 API 地址。旧 Responses 实测已通过；新模型外部验收和首次通道交互继续推进。辅助小模型、Jev、语义模型、四角色路由、Web 面板、动态提示词与 AGI 驱动先保留 TODO，见[开发计划](./docs/开发计划.md)。

支持单进程可信静态插件；权限检查不是沙箱。Kernel 不承载模型/记忆业务；定义、实现和组合层分离。密钥只由宿主提供；退出显式停止并刷新日志。

[文档中心](./docs/README.md) · [企划案](./docs/企划案.md) · 中文 PR 更新 · MIT
