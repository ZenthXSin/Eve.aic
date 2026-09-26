# Eve.aic

Eve.aic 是使用 Rust 构建的插件优先运行时底座，目标是为可演化认知系统提供可组合的基础设施。

## 核心功能

- **Plugin API 与边界**：提供 Plugin、Manifest、Dependency、PluginContext、Event、Service、State、Cleanup；插件只能通过 Context 访问 Runtime。
- **注册与依赖**：按唯一 ID 注册静态插件，检查缺失依赖和循环依赖，按依赖顺序启动。
- **异步生命周期**：Registered → WaitingDependencies → Starting → Active → Stopping → Stopped；启动失败进入 Failed。
- **回滚与清理**：失败时清理当前插件并逆序停止新启动的依赖；Cleanup 支持异步操作，逐个执行并汇总错误。
- **Event 与 Service**：按 EventId 管理监听器和事件投递；按 ServiceId 提供、获取和随插件释放类型安全的共享服务。
- **State**：提供插件键值状态读写。

## 运行

需要 Rust stable：

~~~bash
cargo run -p eve-runtime
cargo test --workspace
~~~

检查：

~~~bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
~~~

项目是 Cargo Workspace：plugin-api 定义契约，kernel 实现内核，runtime 提供宿主示例。详见 [docs](./docs/README.md)。

## 第一版边界

当前只支持单进程、可信、静态编译插件，不加载任意 Rust 动态库。Task、持久化 State、权限执行、异步事件、WASM/独立进程插件、CLI 及 LLM、Memory、Learning 属于后续阶段。

所有更新通过 Pull Request 完成：请从功能分支提交实现、测试和说明，不要直接修改 main。

MIT
