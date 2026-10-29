# 脚本插件运行器

`eve-script-runtime` 为 Python/JavaScript/Rust 插件提供独立进程执行、不可变版本句柄、隔离迁移函数与显式状态迁移和结构化回执。脚本依赖绑定具体版本，更新和回退不替换正在调用的代码。

不受信进程使用 Linux bubblewrap、seccomp 和资源限额；隔离不可用即拒绝。脚本通过 JSON 行协议调用同一宿主授权派发器，声明工具不授予权限，QQ 凭据只留在宿主。调用链共享身份、原子步数、期限、取消和只读临时目录。

```bash
cargo test -p eve-script-runtime
cargo run -p eve-script-runtime --example local_plugins
```

示例实际运行 Python 父脚本、JS 子插件和本地模拟的 `qq.send_file`，不连接 QQ、不部署。需要 `/usr/bin/bwrap`、系统 Python 和 PATH 中的 Node。默认支持 Linux x86_64/aarch64，当前验收平台为 x86_64。

Eve 装配适配器为 `eve-app::qq_scripts::ScriptTools`，提供保存、查询、启用、执行及无副作用预检工具。宿主必须显式注入直接调用使用的授权策略。

持久化、恢复、RPC 格式、配额与权限语义见[运行契约](docs/runtime.md)。

Rust v1使用可信rustc独立隔离编译std-only源文件，不调用Cargo或重编Eve；通过`EVE_SCRIPT_RUSTC`配置实际编译器路径。候选失败保留旧版，产物校验后缓存。无状态插件支持旧调用在途时切换新版；有状态插件需确认快照与CAS迁移。

可选[容器后端](container/README.md)供Windows连接可信Linux容器daemon。当前仅Linux实际执行已验收，Windows专用runner工作流尚需真实环境；未配置隔离器继续拒绝执行，Rust编译当前仅支持Linux bubblewrap。
