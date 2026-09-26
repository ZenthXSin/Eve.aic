# Eve.aic

Eve.aic 是一个使用 Rust 构建、以插件为核心的运行时底座（plugin-first runtime）。
它的长期目标是为可演化的认知系统提供稳定、可组合、可替换的基础设施。

当前项目处于第一阶段：先把单进程 Runtime、插件边界和生命周期做稳定，再逐步加入更高层的能力。

## 当前状态

第一阶段的异步插件内核已经完成：

- 使用 Cargo Workspace 管理多个 crate
- 使用 Tokio 驱动异步插件生命周期
- 通过依赖声明决定插件启动顺序
- 通过 PluginContext 访问 Runtime 能力
- 提供 Event、Service、State 和资源清理能力
- 启动失败时清理当前插件，并回滚本次请求中新启动的依赖
- 停止时先停止消费者，再按逆序等待所有异步 Cleanup
- 使用 Rust trait 接入可信、静态编译的插件

## 核心设计

Eve.aic 遵循“一切皆插件”的长期方向。Runtime 负责调度、隔离边界和资源生命周期；具体能力由插件提供。

~~~text
Plugin Registration
        |
        v
Dependency Resolution
        |
        v
Async Plugin Lifecycle
        |
        v
PluginContext
   |       |       |
 Event   Service  State
        |
        v
Reverse Async Cleanup
~~~

单个插件的生命周期如下：

~~~text
Registered
    -> WaitingDependencies
    -> Starting
    -> Active
    -> Stopping
    -> Stopped

启动 Future 失败 -> Failed
~~~

插件只能通过 PluginContext 与 Runtime 通信，不能直接依赖 Kernel 的内部结构。这样可以让 API 契约保持稳定，也为未来的 WASM 或独立进程插件协议留下边界。

## Workspace 结构

~~~text
crates/
├── plugin-api/   插件可见的稳定 API 和数据类型
├── kernel/       插件注册、依赖、生命周期、Event、Service、State 和清理
└── runtime/      可运行的 Runtime 示例宿主
docs/             企划、架构、决策、计划和执行文档
~~~

### plugin-api

插件 API 定义包括：

- Plugin 和 PluginManifest
- PluginContext
- PluginDependency
- Event、EventId 和 EventHandler
- ServiceId
- 插件状态读写
- 异步 PluginFuture 和 Cleanup
- 跨边界的 PluginError

### kernel

Kernel 是可嵌入的 Tokio Runtime 内核，负责：

- 注册和查找插件
- 依赖检查、启动顺序和循环检测
- 异步启动、停止和失败回滚
- Event Bus
- Service Registry
- 内存 State Store
- 插件资源作用域和逆序清理

### runtime

Runtime crate 提供最小可运行示例，用于验证宿主能够注册、启动和停止插件。

## 快速开始

需要 Rust stable 工具链。

运行示例 Runtime：

~~~bash
cargo run -p eve-runtime
~~~

运行完整测试：

~~~bash
cargo test --workspace
~~~

执行项目检查：

~~~bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
~~~

## 编写一个插件

插件通过异步的 Plugin::start 返回启动 Future。插件创建的服务、事件监听器和其他资源都应通过 PluginContext 注册，这些资源会在停止时自动清理。

~~~rust
use eve_kernel::Kernel;
use eve_plugin_api::{
    cleanup, Cleanup, Plugin, PluginContext, PluginFuture, PluginId, PluginManifest, PluginResult,
};

struct ExamplePlugin {
    manifest: PluginManifest,
}

impl Plugin for ExamplePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            ctx.state_set("started", b"yes".to_vec())?;

            Ok(Some(cleanup(|| async { Ok(()) })))
        })
    }
}

#[tokio::main]
async fn main() -> PluginResult<()> {
    let kernel = Kernel::new();
    let manifest = PluginManifest::new("example", "0.1.0")?;
    let id = PluginId::new(manifest.id.as_str())?;

    kernel.register(Box::new(ExamplePlugin { manifest }))?;
    kernel.start(&id).await?;
    kernel.stop(&id).await?;
    Ok(())
}
~~~

Cleanup 会按注册顺序的逆序执行。即使某个 Cleanup 返回错误，内核仍会继续执行其余 Cleanup，并在最终结果中报告错误。

## 第一版边界

当前版本只支持单进程、静态编译、可信插件，不加载任意 Rust 动态库，也不承诺 Rust ABI 的动态兼容性。

以下能力属于后续阶段：

- 更完整的事件投递和异步 Handler 模型
- Task Manager、任务取消和后台任务监管
- 持久化 State Store
- 权限声明与实际资源访问检查
- WASM 或独立进程插件
- CLI、插件发现和版本化加载协议
- LLM、Memory、Personality、Learning 等高层能力

## 文档

- [文档中心](./docs/README.md)
- [架构设计](./docs/架构设计.md)
- [决策记录](./docs/决策记录.md)
- [开发计划](./docs/开发计划.md)
- [执行书](./docs/执行书.md)
- [待确认问题](./docs/待确认问题.md)
- [企划案](./docs/企划案.md)

## 贡献方式

仓库的更新统一通过 Pull Request 进行：

1. 从最新的 main 创建功能分支。
2. 在功能分支上完成实现和测试。
3. 提交 Pull Request，并附上变更说明和验证结果。
4. 通过审查后再合并到 main。

请不要直接推送或直接修改 main。

## License

MIT
