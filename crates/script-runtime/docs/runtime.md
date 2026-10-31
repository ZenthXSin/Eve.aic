# 脚本执行与恢复契约

## 定义、实现与装配

公开定义在 `model.rs`：`ScriptSpec`、内容寻址的 `VersionRef`、状态 schema、迁移、证据和回执。`ScriptStore` 是文件账本实现，`ScriptRuntime` 执行解释器进程，`AuthorizedToolDispatch` 是可替换宿主调用边界。Eve 的 `qq_scripts::ScriptTools` 只做 Tool 装配，不给脚本注入 QQ 凭据、不加载脚本进入 Rust 地址空间。

使用顺序是 `publish` 保存版本，`activate` 初始化版本状态，`bind` 取得不可变句柄，`preflight` 只读预检，再 `run` 实际调用。依赖也需先发布、初始化。句柄保留完整依赖图；所有依赖都是 `{script,digest}`，调用时不查询最新版本。每次调用重新启动语言进程；热更新只影响后续新绑定的版本，已绑定句柄继续调用原版本。当前只支持独立进程插件，不支持不受信 native 动态库。

## 权限和隔离

`ScriptSpec.tools` 只是候选清单。宿主显式注入 `ToolAuthorizer`；`CheckedTools` 在完整声明图预检时检查工具可用性，在每次实际调用前校验参数及实时权限，在 dispatch 时再次检查。传递的是宿主原 `ToolExecutionContext`，脚本无法指定 user/session、内核权限、delegated 标记或预算。`None` 参数的预检只能检查工具级权限；生成的动态参数会在实际调用前按 `Some(arguments)` 单独检查，预检不承诺任意未来参数获准。

跨脚本宿主操作都走同一个 dispatch，包含文件操作、命令审核与 QQ 交付。未声明工具、未装配工具、不可见依赖、未知授权策略一律拒绝。实际 QQ 装配必须使用与直接工具相同的路径、成员、命令和管理员边界。生成脚本不能通过注册新工具绕过这些检查。

QQ 组合层用 `qq_scripts::is_synchronous_leaf` 显式选择已审核同步叶子，`bind_tools` 再次拒绝不在表中的工具；不把工作流、任务派发、子代理或脚本管理入口装配给脚本。脚本依赖经本执行器的 `script` 协议调用，不借工作流形成旁路递归。新增插件叶子须在可信组合层审核其上下文传递和取消语义后加入，不能由生成脚本自行拓展。

Linux 后端强制创建 user/pid/network/mount 等 namespace、清空环境、丢弃 capabilities、禁止再次创建 user namespace，只读挂载系统库、明确语言解释器、当前源文件及调用链目录。宿主 `/workspace`、`/root`、`/home`、`/etc` 和环境凭据不挂载；网络不共享。运行时二进制与系统库目录必须由可信装配管理，不应在这些运行时目录存放业务凭据。`/usr/bin` 不挂载。Python 使用 isolated 模式；Node 禁止 native addons，使用 jitless 与有界 V8 堆。

seccomp 拒绝 fork/vfork、非线程 clone、clone3、额外 namespace 和部分跨进程/内核访问。Node 的线程通过 flags 可检查的 clone 创建。此规则是 namespace 之上的附加限制，不是完整 syscall allowlist。资源限额为每进程 1GiB 地址空间、3秒 CPU、64个文件描述符、1MiB单文件和禁止 core dump；调用链默认30秒墙钟上限，可被宿主更短期限收紧。`/tmp` 为每进程16MiB tmpfs。取消或期限使整个调用链停止，PID namespace 随进程销毁，后代不会留在宿主运行。宿主授权工具自身的资源策略仍由原工具负责。

预检不启动隔离器，因此只确认平台、架构、可执行文件配置、声明图、身份和权限；内核是否允许 namespace/seccomp 在实际启动时确认。缺少 bubblewrap、内核拒绝隔离、未知架构、进程启动失败都终止执行，绝不回退普通 exec。原生 exec、Python subprocess 或 Node child_process 不作为宿主沙箱；本后端还直接禁止创建非线程子进程。

可信宿主还可选择 `ContainerSandbox`，用于Windows上的Linux容器daemon，或Linux本地容器。镜像必须为预装的完整sha256 ID、CLI为可信绝对路径，插件不能控制容器参数。其隔离资源与取消差异、安装依赖和真实验收入口见 [容器后端](../container/README.md)。普通Windows契约测试通过不等于容器执行已验证。

## 调用链和 JSON 行协议

宿主首先向 stdin 写一行 JSON，包含 `protocol:1`、call/root ID、精确版本、arguments、`state:{revision,schema,value}` 和 `workspace:"/work"`。脚本 stdout 每行是一个请求；除 return 外，宿主返回 `{ok:true,result:...}`。失败停止进程和后续调用，不让脚本重试未知操作。stderr 不回传账本，防止复制凭据或无界输出。

| 请求 | 内容与行为 |
| --- | --- |
| `check_tool` | `name,arguments`，只对当前版本已声明叶子进行实际参数、可信身份及权限预检；不执行叶子 |
| `tool` | `name,arguments`，通过同一授权派发器调用宿主叶子 |
| `script` | `dependency,arguments`，只调用声明的不可变子版本 |
| `state` | `value`，校验后暂存，成功终结时提交 |
| `write` | `path,content`，通过宿主写单层共享临时文件 |
| `artifact` | `path`，暂存提升请求，顶层成功后统一提升 |
| `return` | `value`，须进程正常退出，才算成功 |

`check_tool` 复用实际 `tool` 的参数解析（包括 `$workspace`）、参数校验和宿主只读授权策略，不运行叶子、不扣叶子步骤，也不增加执行证据或保留权限。响应为 `{allowed,executed:false,authorization_reserved:false,reason}`；`reason` 仅为 `preflight_passed` 或 `rejected`，不复制内部策略详情。`Rejected` 的参数/权限拒绝返回 `allowed:false`，脚本可停止或选择已授权替代方案；`Failed`、取消和到期仍终止调用链，不能当作普通拒绝继续。未声明工具、坏协议和非法临时路径也直接拒绝。预检可能检查文件存在及当前权限，但不证明外部服务可用或未来权限仍有效，实际 `tool` 必须重新检查。每次预检计入每进程 128 条 RPC 上限和原调用链期限；脚本进程本身仍按原规则扣一步，不能用预检请求重领预算或无限轮询。

不允许额外请求字段或重复 JSON 键。每行、输入、响应和状态至多1MiB，源文件至多256KiB，单插件至多16个依赖和32个工具；调用深度至多8，每进程128条RPC。每个脚本调用和宿主叶子各原子预留一步，共享宿主 `SharedToolSteps`，失败不退款。RPC的预算随实际调用扣除，不预先假定动态分支。Eve 外层工具调用若已计步，该次 Tool 包装开销与脚本进程步骤分别计费；不会因此扩大预算。

`/work` 在全链只读挂载；`write` 由宿主控制写入，不允许目录、符号链接或路径穿越，最多64个文件、总量8MiB。此方式避免让不受信进程直接写宿主磁盘。叶子参数可使用独立对象 `{"$workspace":"report.txt"}`，宿主在授权前解析为该文件的真实临时路径。脚本不可指定自己的宿主临时根。子调用共享同一目录；顶层退出即删除，宿主工具在返回前必须自行完成读取或复制，不能保存临时路径供异步延后读取。

临时路径引用不会给生产工具增加路径特权。QQ 普通成员的 `send_file` 仍只接受自己项目的文件，不接受任意宿主临时路径。`examples/plugins/eve_report.js` 展示生产工具协议：先通过已授权 `write_file` 写入自己 `exports/`，再经 `send_file` 排队，最后 `query_file_delivery` 查询真实回执；Pending/Sending/Unknown 均不当作送达、不重发。本地 `report.js` 使用单独的模拟 `qq.send_file` 叶子验证临时文件读取与生命周期，不声称生产 QQ 有此路径例外。

`artifact` 不立即产生永久文件。顶层成功时，宿主把已声明文件复制到产物目录的暂存子目录、fsync，然后重命名为 `<root_call_id>/...`。不覆盖已有产物。产物提升与最终账本提交跨文件系统对象，可能在崩溃时留下有文件而回执 unknown 的情况；只能核对现存文件和证据，不能自动重跑生成或交付。中间子脚本成功时自己的状态和回执已持久化，顶层失败不会回滚已知子调用或外部效果。

预检不会调用危险动作的审核模型，也不产生后续审核放行证据；即便 `allowed:true`，真实调用仍可能因人工/模型审核、权限撤回、文件变化或服务失败而拒绝。不得用预检绕过正常执行守卫。

## Rust独立编译与热重载

`language:"rust"` 使用同一版本化JSON行RPC和独立插件进程，不加载Rust动态库，不依赖不稳定Rust ABI，不重启Eve或重新编译整个workspace。可信宿主配置 `EVE_SCRIPT_RUSTC` 为实际toolchain `bin/rustc` 的绝对路径（不是rustup代理）；也可使用 `Bubblewrap::with_rustc`。当前Rust编译仅在Linux bubblewrap实现，容器/Windows Rust明确失败关闭。

`ScriptRuntime::publish_candidate`（自然工具save入口）先隔离编译候选，成功才保存不可变版本。纯存储 `ScriptStore::publish` 仍不启动编译器；直接使用此底层API的可信宿主须自行执行候选预验证。v1只支持单个std-only Rust源文件，不运行Cargo、build.rs、下载依赖或装配第三方proc-macro。编译器路径/目标/编译参数由可信宿主固定，模型只能提交源码。

编译沙箱不挂载宿主用户目录、凭据或网络；只读公开sysroot和系统linker工具目录。仅编译阶段允许可信rustc调用linker子进程，运行阶段仍使用原禁止fork的seccomp。编译使用共享预算一步、最长20秒墙钟、10秒CPU、2GiB地址空间和16MiB单文件输出上限，取消清理PID namespace。只有成功的普通二进制文件进入内存索引的私有临时缓存，缓存键包含源码、实际rustc版本/host target、固定flags与linker路径；每次复用核对产物SHA-256，校验不符拒绝执行。缓存最多32项，进程退出删除，重启按已保存源码重新隔离编译，不重放历史调用。

执行每次另扣进程步骤，未缓存时还扣编译一步。旧句柄和正在执行的进程保留旧产物；发布/启用新版不会卸载旧代码。无状态插件可在旧调用运行中启用新版并立即接受新调用；有状态插件遵循确认快照/CAS迁移约束。编译失败、超时、私有宿主文件include失败都不会改变active。Rust迁移程序同样只能读取确认状态并返回候选状态，不能调用宿主工具；编译和运行各自计步。

## 状态、迁移和回退

无状态版本（同schema且fields为空）允许旧版本仍有running调用时启用独立新版；新调用绑定新版，旧调用保留旧句柄与独立快照。unknown源仍拒绝启用。含业务状态的首次切换必须等待源运行结束后复制或显式迁移；不承诺在可变状态仍在写入时立即切换。

schema 是明确支持的有界子集：正整数 version，以及必须恰好包含的字段和 JSON 类型；不默默接受未实现 JSON Schema 关键字。状态只在成功回执同一原子提交中更新，每次提交递增 revision。同版本一次只准入一个运行，防止外部效果发生后才发现状态冲突。

每个不可变版本保留独立状态快照。新版本 schema 相同时可复制当前已确认状态，schema 变化必须显式给出 `StateMigration{from,expected_revision,value}`；校验源 revision 和新 schema，源有 running/unknown 调用时禁止复制和迁移。已有目标快照不可覆盖。回退只选择旧版本及其保留状态，不调用脚本、不重放工具、不声称撤销已发送消息。旧句柄继续更新旧版本状态，不隐式改写新版本；需要汇合状态时必须发布新版本并显式迁移。

### 隔离迁移函数

启用工具还接受与 `migration` 互斥的 `migration_script:{from,expected_revision,language,source}`。它不改变旧版 `ScriptSpec` 的内容摘要及账本格式。Python/JavaScript 迁移程序先从 stdin 读一行 JSON，包含 `mode:"migration"`、精确源/目标版本、确认的 `state:{revision,schema,value}` 和 `target_schema`。函数根据旧状态计算候选对象，stdout 必须只有一行 `{"op":"return","value":候选对象}`，随后正常退出。可以在脚本内声明并调用纯 `migrate(state)` 函数。

迁移使用原可信身份、同一共享原子预算（每次进程一步）、宿主取消与最长30秒期限，以及同一 Linux namespace/seccomp/CPU/内存上限。只读空工作目录、无网络；不装配任何宿主派发器，`tool/script/state/write/artifact` 都被拒绝。进程的临时内存与私有 tmpfs 在退出后销毁，不产生可见宿主副作用。这里的“纯”指无外部副作用，不承诺时间/随机数等解释器输入可重复。

启动前检查源版本身份、revision、running/unknown 和目标未初始化；完成时重新在同一账本事务内检查这些条件、校验新 schema，提交目标初始状态、选中版本及成功迁移回执。回执含源码/源revision请求摘要与候选状态摘要。并发源更新导致 CAS 拒绝，不覆盖新状态。预算耗尽、取消、超时、非零退出、额外输出、禁用协议、schema错误及提交前崩溃均不启用候选版本；旧代码和已确认状态继续可用。失败返回原工具审计链，成功回执持久保存；不声称已撤销外部消息或历史工具效果。未配置可信容器后端的非 Linux 执行继续失败关闭，保存和显式值迁移仍可跨平台使用。

## 账本、回执和恢复

账本包含全部不可变版本、选中版本、状态及调用记录。标准库独占OS文件锁保证不同宿主不能同时打开，Unix与Windows均支持契约存储；提交使用暂存文件、文件fsync和原子替换，Unix额外执行目录fsync，成功后才替换内存状态。Windows缺少可移植目录flush，因此不宣称与Unix相同的掉电后新目录项保证；Windows未配置可信Linux容器时明确拒绝脚本执行，不降级原生exec。持久化失败使 Store 进入不可继续状态，要求重新打开核对，不继续执行下一步。总量64MiB、256个版本；满额拒绝，不自动淘汰。文件格式、checksum、重复键、schema、引用或版本内容损坏均拒绝打开，保留原数据；已初始化账本缺失时不重新创建。

每次外部工具和子调用前先保存 running 证据。回执包含 call/parent/root ID、可信owner、不可变版本、请求摘要、status、result和操作证据。证据只复制结果的SHA-256摘要，不复制完整叶子响应或参数；摘要是规范serde JSON表示的哈希，产物摘要同样对字节数组的JSON表示计算。脚本最终返回值仍是业务输出，授权工具不应把凭据作为业务数据返回。

明确被拒绝且尚无成功步骤的调用为 rejected；已有成功步骤之后被拒绝则为 failed，保留部分效果证据。工具返回成功只证明工具的返回，不额外断言业务目标达成。工具超时、取消、失败但无法确认副作用时为 unknown，后续步骤停止。宿主直接丢弃执行future时，清理守卫同步保存unknown并销毁进程；若来不及完成或提交失败，重启把残留running转为unknown，从不重启进程或自动重放。重复 call ID 拒绝执行；unknown 版本禁止新调用和迁移，直到可信人工核对入口 `acknowledge_unknown` 附证据关闭记录。该入口不暴露给生成脚本，也不会恢复或重放历史调用。

## 本地验收

QQ 原生组合允许诊断脚本明确声明 `read_context_record` 和 `inspect_model_runtime`：前者读取当前可信会话的档案与任务记录，recent/turn 引用不返回当前 Pending 轮次；后者只读模型配置和准入观测，不发送探测或读取凭据。两者都走原发起者授权、原共享步数和期限，固定版本、状态、调用链及未知结果不重放规则保持。管理、派发、脚本管理以及权限预检工具本身不作为脚本叶子装配。

`local_plugins` 实际运行 Python父脚本、JS子插件，经宿主派发模拟 `qq.send_file`，再检查state、结构化回执、临时文件删除和产物提升。例子从不连接QQ。纯版本/身份/schema/账本/OS锁/授权/原子预算契约测试跨平台保留；真实隔离执行测试仅在Linux编译运行，Python/Node/bwrap来自可信PATH并解析真实绝对路径。非Linux另有执行和dry-run拒绝测试，检查没有改预算、账本或调用叶子。Linux集成测试实际验证版本切换/回退、权限撤回、宿主文件和网络隔离、fork拒绝、内存限制、取消/期限、坏协议和重启unknown不重放。Linux namespace或seccomp不可用时测试直接失败，不伪造通过或悄悄跳过。

## 宿主内存测量与事务临时副本

`cargo run -p eve-script-runtime --example memory_profile` 使用独立临时目录，固定生成 64 个 128 KiB 源码版本、八个身份范围，每版激活后重启账本并核对版本数；不执行脚本或外部工具。Linux 输出各阶段 `/proc/self/status` 的 VmRSS，其他平台仍输出账本大小。进程峰值应另用系统测量工具记录，不能把阶段采样误作峰值。

Linux debug、jobs=2、debug=0、incremental=0 的同样本对照中，账本始终为 8,423,468 字节；移除 Envelope 的重复全账本复制，并改用 64 KiB 缓冲流式 JSON 哈希后，进程峰值由 51,064 降到 35,316 KiB（约 30.8%），64 版本阶段由 43,132 降到 27,348 KiB，重启后由 35,664 降到 27,504 KiB。耗时为 28.76 与 28.79 秒。峰值来自独立子进程的 `getrusage(RUSAGE_CHILDREN)`，采样值可能受分配器和平台影响，不构成固定内存承诺。

本改动仅减少事务临时副本：当前 Ledger 仍全量常驻，事务仍保留独立候选供提交失败回退。JSON 校验字节、账本格式、原子提交、未知效果与隔离范围均保持；没有删除旧版本、回执或缩减工具步数。Rust 产物缓存保存的是磁盘临时目录和路径，并非 32 份常驻二进制映像，其编译子进程峰值须独立测量。

`rust_memory_profile` 是单独的可信宿主测量例子：显式设置 `EVE_PROFILE_RUSTC`，在八个范围内完成 32 次真实隔离编译、版本激活与执行。Linux 同配置样本宿主峰值 11.24 MiB，32 缓存阶段 VmRSS 8072 KiB；递归编译与链接子进程合计峰值为 142.63 MiB，耗时 42.45 秒。宿主和子进程分别报告，不能把各自峰值相加当作同一时刻总峰值。此样本在流式哈希补丁后测得，没有对应修前 Rust 样本，因此不声称 Rust 缓存内存降幅。
