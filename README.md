# Eve.aic

Rust 插件运行时底座。一切皆插件，分离定义层、实现层和组合层。

## 已实现功能

- **插件与依赖**：唯一 ID 注册，检查缺失与循环依赖，自动启动依赖。
- **异步生命周期**：Tokio 启动与逆序清理；失败回滚新增依赖并保留清理错误。
- **Context 边界**：统一能力入口，停止后的旧 Context 失效。
- **事件总线**：可替换 `EventBus`，同步按序广播，错误隔离，自动注销。
- **共享服务**：可替换 `ServiceRegistry`，字符串 ID 与 Rust 类型双重约束。
- **状态存储**：可替换 `StateStore`，内存键值按插件隔离，明确返回读写错误。
- **权限声明检查**：可替换 `PermissionChecker`，精确匹配声明；属于可信插件约定，不是沙箱。
- **协作验收**：提供者发布事件，消费者调用服务、写状态，验证停止清理与失败回滚。

## 运行与测试

需要 Rust stable：

```bash
cargo run -p eve-runtime
cargo test --workspace
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

成功运行会输出“插件协作验收通过：已接收：你好，Eve.aic”，并确认资源清理和预期失败回滚。

## 项目结构与边界

`plugin-api` 定义契约；`kernel` 提供可替换的默认实现；`example-plugins` 只依赖契约；`runtime` 负责装配与验收。详见[插件开发](./docs/插件开发.md)和[文档中心](./docs/README.md)。

当前支持单进程、可信、静态编译插件。Task、Logger、持久化与重启恢复、异步事件、WASM/进程隔离、CLI 和认知插件留待后续阶段。已取得的服务引用不会被强制撤销；宿主应显式停止 Runtime。

所有更新通过中文 Pull Request 提交，不直接修改 main。

MIT
