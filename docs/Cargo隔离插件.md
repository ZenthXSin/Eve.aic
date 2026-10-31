# 离线 Cargo 插件

Rust 标准库插件继续使用 `language="rust"`。需要依赖时使用 `language={"rust_cargo":{"dependencies":{"serde_json":"1.0.151"},"bundle_sha256":"冻结资源摘要"}}`，`source` 仍是 Rust 源码。可选 `build_source` 在编译容器内作为 build.rs 执行；不能指定宿主命令、路径、Git 地址、版本范围或容器参数。原脚本依赖字段仍指其他已发布脚本版本，与 crate 依赖分开。

宿主同时设置 `EVE_SCRIPT_CARGO_CLI`、`EVE_SCRIPT_CARGO_IMAGE`、`EVE_SCRIPT_CARGO_TOOLCHAIN`、`EVE_SCRIPT_CARGO_VENDOR`、`EVE_SCRIPT_CARGO_BUNDLE_SHA256`。缺项拒绝启动，全部缺省则不开启 Cargo。CLI 必须为绝对文件路径，只使用本地 `/var/run/docker.sock`；镜像必须为已有的 `sha256:` ID，根层最多 64 MiB，禁止自动拉取。toolchain 与 vendor 是管理员预先准备的公开只读资源，不能含私有代码或凭据。QQ 无参数 `list_script_plugins` 返回配置摘要，不暴露宿主路径，不访问模型或 Docker；configured 不表示实际健康或已授权。

本后端仅支持 Linux x86_64、cgroup v2 和本地 Docker；编译产物通过现有 bubblewrap 沙箱执行，不能将缺失隔离降级成宿主 exec。现有容器解释器后端没有原生 Rust 执行能力，不能用它代替 bubblewrap 运行 Cargo 产物。管理员还须提供本机 `/usr/bin/python3`、x86_64 GNU 链接工具及对应公共系统库；镜像内编译工具使用固定只读挂载。编译器身份使用 toolchain 非 root 属主 UID；root 所有的公共工具链使用 65534，不放宽私有宿主目录权限。

## 配方、产物和恢复

保存候选先完整复制并核对 vendor 冻结摘要，再离线生成 lockfile 和 locked 构建。单文件最多 8 MiB，总资源最多 64 MiB、4096 文件、8192 目录成员、16 层目录；不接受链接。暂存副本避免校验后原资源变动影响本次编译。根项目最多 32 个明确版本依赖；锁定同一公共 vendor，不能访问网络或宿主 Cargo 缓存。

编译失败返回原有结构化 compilation 回执，诊断最多 8192 UTF-8 字节，含阶段和错误类别；没有可激活的新版本。成功只证明编译完成，不能据此声称业务执行成功。

保存格式保持现有 `language="rust"`，源码以 `// eve-cargo-v1` 单行 JSON 配方和 `compile_error!` 保护头开头。配方绑定资源摘要、构建脚本、精确依赖和实际 ELF 摘要；输入伪造的产物摘要在候选准备时被实际结果替换。版本摘要覆盖完整保存内容。旧版支持当前冷账本的运行器可读取此 Rust 标签；其标准 rustc 会拒绝保护头，避免误跑不同构建流程。更早、不支持格式 2 冷账本的旧版仍须完整快照回退，见《脚本冷账本》。不要移除保护头。

每次运行重新离线构建并比较发布时 ELF 摘要，只有完全一致才交给执行沙箱。没有新增 Cargo 缓存；重启后也不能默默改为最新编译结果。工具链或 build.rs 的不确定输出导致不一致时保留版本、拒绝运行，修复后重新准备候选、验证、显式启用。源码、状态、活动版本、未知回执及旧句柄规则继续沿用脚本运行器。Cargo 状态迁移暂不支持，使用 Python、JavaScript 或标准库 Rust 的现有纯迁移入口。

## 调用链与资源

原始可信身份、取消、期限和原子预算保持贯穿调用链。每次实际编译扣一次原预算，脚本帧/叶子/子脚本照原规则扣除；不另领额度。标准 rustc 与 Cargo 共用全局一个编译槽。排队和 Docker 等待服从原取消/期限，最长 30 秒；不提高业务预算。

容器网络关闭、根只读、无新增权限，内存 2 GiB、禁用 swap、CPU 一个、最多 64 进程；项目、目标、Cargo home、日志、交接目录各有 tmpfs 上限。控制器实际核对 cgroup 和只读/权限状态，宿主可用磁盘低于 512 MiB 则拒绝。容器内不继承宿主凭据环境；编译用户不能写 root 控制器和交接文件。冻结 ELF 前杀净该用户所有后代，再读取固定普通文件；超过 16 MiB、链接、SHA 不符或退出未知均不交付。宿主取消/drop 发起有界容器清理，控制器同时有硬期限；不能把清理请求发出当作业务成功。

## 复现验收

先构建 QQ 宿主并提供真实 Docker/bubblewrap，然后运行 `python3 scripts/test-cargo-isolation.py`。脚本从本仓库 Cargo.lock 和本地已下载的 crate 归档制作测试 vendor，先校验归档 SHA，再有界解包；不联网、不运行依赖构建脚本。测试镜像由本次空 tar 导入，最终只删除本次镜像标签。所有依赖脚本只在生产隔离控制器中编译执行。

覆盖真实 serde_json 编译、build.rs 无法读取宿主私有文件/凭据或连接网络、资源限制变弱时拒绝、产物篡改拒绝；原生版本保存/热切换/旧句柄/失败保留/重开恢复/摘要不符，以及 QQ 工具保存、激活、运行和另一宿主进程恢复。模型与 QQ 桥接为本地替身，不宣称真实提供商或正式 QQ 投递质量。GitHub QQ CI 执行同一入口；无显式资源的普通 Rust 测试将两项真实 Docker 场景标为 ignored，而非伪造通过。
