# Eve.aic

Rust 插件运行时：一切皆插件，分离定义、实现与组合层。

## 已实现功能

- **插件与依赖**：唯一 ID 注册、依赖校验与自动启动。
- **异步生命周期**：Tokio 启动、逆序清理、失败回滚，隔离启动与清理 panic，汇总停止错误。
- **Context 边界**：统一能力入口，停止后失效。
- **事件总线**：可替换 `EventBus`，同步按序广播，错误隔离，自动注销。
- **共享服务**：可替换 `ServiceRegistry`，字符串 ID 与 Rust 类型双重约束。
- **状态存储**：可替换 `StateStore`，支持内存与 JSON 文件、插件隔离、独占锁和进程重启恢复。
- **任务管理**：可替换 `TaskManager`，支持前后台、立即/延迟/定时/重复、自定义类型与调度、取消和清理。
- **结构化日志**：可替换 `Logger`，附加来源与时间，支持过滤、标准错误输出和限量内存记录。
- **权限检查**：`PermissionChecker` 精确匹配声明，用于可信插件，不是沙箱。
- **运行时诊断**：`RuntimeInspector` 查询插件和任务状态，示例验证协作、清理与回滚。

## 运行与测试

需要 Rust 1.89+：

```bash
cargo run -p eve-runtime
cargo test --workspace
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

## 项目结构与边界

`plugin-api` 定义契约；`kernel` 提供可替换的默认实现；`example-plugins` 只依赖契约；`runtime` 负责装配与验收。详见[插件开发](./docs/插件开发.md)和[文档中心](./docs/README.md)。

支持单进程可信静态插件。恢复字节状态，任务与服务由插件重建。异步事件、WASM/进程隔离、CLI 和认知插件留待后续。宿主须显式停止 Runtime 并刷新日志。

通过中文 PR 更新。

MIT
