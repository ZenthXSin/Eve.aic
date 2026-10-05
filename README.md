# Eve.aic

Rust 认知系统底座：稳定插件 Runtime 与主模型闭环，持续推进记忆、驱动和认知能力。

## 当前能力

- 插件注册/卸载、SemVer 依赖、异步生命周期、失败回滚及逆序清理。
- 事件、服务、状态、任务、日志与权限公开契约；后端可替换。
- 文件/本地 PostgreSQL 状态、目录独占、严格恢复、诊断与停止错误汇总。
- 普通配置的来源优先级、请求快照、版本、备份与回滚。
- Mock/Responses/Chat 主模型、插件上下文、工具批次执行和结果回传。
- AGENT.md 显式来源、内容版本和固定身份快照。
- 多轮会话、完整工具历史、重启恢复；失败输入不进入完成历史，旧工具不重放。
- 独立流式输出、任务取消、代际过滤；QQ 支持明确补充、纠正和取消。
- 有界内生反思、待办与草稿查询；每个目标修订至多一次反思，零工具。
- QQ 有来源交互记忆、明确偏好查看/修正/撤销及按可信范围装配 Context。
- 低频偏好候选提炼、证据与有效期、用户确认及重启不重复请求。
- `--segmented` 按自然段分条投递同一轮回复：逐段回执，段间取消，重启不补发。

## 运行

Rust 1.89+。在仓库根目录配置主模型及宿主凭据：

```powershell
$env:EVE_OPENAI_MODEL = "deepseek-v4.1-flash"
$env:EVE_OPENAI_PROTOCOL = "chat"
$env:EVE_OPENAI_API_KEY = "<宿主凭据>"
cargo run -p eve-app --locked -- --state-dir ./.eve --agent ./AGENT.md
```

每行一轮，/cancel 取消，/quit 或 Ctrl+C 退出，EOF 处理完队列。同目录、用户和会话恢复历史；当前串行、非流式，内置 echo 工具。详见[核心对话](./docs/核心对话.md)。

QQ 用 `eve-qqbot --memory --cognition` 显式开启记忆和反思，默认关闭；`--database-config` 可选择本地 SQL，不迁移旧文件状态。配置与本地测试见[QQBot 通道](./docs/QQBot通道.md)。

```bash
cargo test --workspace --locked
cargo run -p eve-runtime --locked
```

测试使用本地 HTTP；最后一条运行插件协作验收。

## 计划与边界

核心、真实主模型及 QQ 收发有实机证据；记忆、认知、SQL 与分段已做本地进程验收，QQ 新命令与分段仍待实机验证。成功交互不自动变成偏好，反思草稿不代表目标完成，候选须用户确认。下一步推进分段偏好学习、语义检索、受控目标执行和 AGI 质量评估；Jev 与 Web 后置，见[开发计划](./docs/开发计划.md)。

单进程可信插件，权限检查不是沙箱；定义、实现、组合分层。密钥由宿主提供；退出显式收尾。

[文档中心](./docs/README.md) · [企划案](./docs/企划案.md) · 中文 PR 更新 · MIT
