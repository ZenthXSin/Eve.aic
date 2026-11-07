# 入站前恢复检查点

显式发布启动时，同一个 Eve 进程先完成持久恢复，再暂停于 QQ 桥创建、Web 监听和后台激活之前。发布端在这个边界读取实际冷、热状态，确认原状态保留后让该进程继续。普通启动不设置请求环境变量，行为不增加等待，也不要求用户逐步审批。

这个检查点只证明恢复边界与进程身份，不是业务快照、业务事务日志或自主探索的审查。执行审查仍只核对可信用户身份与直接系统危害。不能用检查点摘要代替完整冷库记录，不能把就绪后的正常学习变化判成恢复丢失。

## 公开组合契约

`eve_qqbot_plugin::QqBeforeIngress` 在回执、冷回执绑定、审核原文绑定和旧回执本地观察恢复完成后调用；成功才注册通道状态并创建桥接任务。替换宿主负责把其它入站与后台置于同一边界之后。

`eve_app::startup_checkpoint::StartupCheckpointSink` 接收 `StartupState`。`QqBotOptions.startup_checkpoint` 可注入可信宿主实现；文件实现仅在 `EVE_STARTUP_CHECKPOINT_REQUEST` 显式指向请求文件时装配，同时配置注入实现和文件请求会失败。文件实现要求 Linux 文件状态后端；PostgreSQL 宿主应替换 sink，不能套用文件恢复证明。

QQ 组合层在回调前完成插件恢复、QQ 归档绑定及旧回执本地恢复、Web 记忆恢复和通知账本同步收集。后台任务可以预先建立休眠句柄，但此时没有模型请求与业务写入；认知循环本身在回调后才启动。回调成功后才启动 Web 通知后台、监听 HTTP 并激活学习、自主探索和其它后台。

## 一次性文件协议 v1

请求、检查点、确认放在同一个预先创建的私有目录中。请求与已有路径须为规范绝对路径；Unix 私有目录不得向组或其它用户开放权限。普通文件读取拒绝符号链接、非常规文件和读取期间的替换。请求与确认拒绝未知、重复字段，输出通过同目录临时文件、同步与不覆盖提交保存。

请求字段全部必填：

| 字段 | 含义 |
| --- | --- |
| `format_version` | 整数 `1` |
| `nonce` | 此次启动独占的 64 位小写十六进制值 |
| `source_revision` | 发布端已绑定的 40 位源码提交 |
| `binary_sha256` | 此次实际执行的二进制原字节 SHA-256 |
| `runtime_path`、`runtime_sha256` | 最终候选 runtime 的规范绝对路径与原字节 SHA-256 |
| `state_directory` | 同进程实际文件状态目录 |
| `checkpoint_path` | 尚不存在的检查点输出路径 |
| `acknowledgement_path` | 尚不存在的确认路径，不能与检查点或占用文件重合 |
| `timeout_ms` | 检查点发布后等待确认的期限，`1000` 至 `60000` 毫秒 |

打开请求即原子创建 `<nonce>.claimed`，其中保存请求原字节摘要。崩溃或失败也保留占用文件；同一路径、nonce、旧确认不能作为下次恢复请求重复使用。每次显式发布或恢复必须创建新目录和新 nonce。

检查点包含请求中的身份和路径字段（不重复 `timeout_ms`），以及：

| 字段 | 含义 |
| --- | --- |
| `request_sha256` | 请求文件原字节 SHA-256 |
| `pid`、`process_start_ticks` | 当前 Linux 进程号和 `/proc/self/stat` 的启动 tick |
| `exe_path` | 当前实际执行文件的规范路径 |
| `automatic_capabilities` | 完整自动组合为 `true`，最小组合为 `false` |
| `ledgers` | `tasks`、`phrases`、`receipts` 的实际持久字节摘要 |

每个账本包含 `present`、`version`、`bytes`、`sha256`。缺失精确表示为 `{"present":false,"version":null,"bytes":0,"sha256":null}`，不创建空账本来替代缺失。存在时版本取实际磁盘 JSON，摘要取当前 `StateStore` 返回的原字节，不先规范化 JSON。任务版本字段为 `format_version`，说法和回执为 `version`。

默认文件实现核验当前执行文件和 runtime 摘要。源码提交由发布端用冻结产物清单证明，不能仅凭请求字段自行宣称对应源码。当前实现要求直接执行业务 ELF；用私有动态加载器执行时，实际执行文件会是加载器，摘要不符即拒绝，不能从 `argv` 猜成已绑定的候选。

确认字段全部必填：

| 字段 | 含义 |
| --- | --- |
| `format_version` | 整数 `1` |
| `nonce`、`pid`、`process_start_ticks` | 与本次检查点精确相同 |
| `checkpoint_sha256` | 检查点文件原字节 SHA-256 |
| `preservation_sha256` | 已落盘的完整恢复证明原字节 SHA-256 |
| `accepted` | 布尔值 `true` |

文件 sink 只校验恢复证明摘要的格式；发布端负责真实生成完整证明、验证摘要与捕获结果并绑定该次检查点，不能填一个形似摘要的值代替验证。sink 收到确认后重新核验请求、检查点、二进制、runtime 字节；组合层再读取同一 `StateStore` 的热账本，变化则失败。错误、旧确认和超时都不开放入站，不报告 ready，保留已恢复状态与失败证据。

## 发布端顺序

1. 停止已有业务进程，在其真实结束后捕获完整 before 状态；保留状态、冷库和未知执行/投递结果。
2. 用已冻结的二进制、runtime 与状态路径创建新请求，通过启动器交给本次首次 Eve 进程。
3. 检查点出现后核验 owned 父子链、实际进程启动身份、执行文件、请求和产物摘要。有 PID namespace 时按 owned 链与 `NSpid` 映射，确认仍使用检查点内 PID。
4. 在屏障内用最终 candidate runtime 捕获完整 startup 状态，包括任务冷文件、回执数据库和说法批次，严格比较 before → startup，只接受已定义的恢复语义。证明必须落盘并绑定检查点原字节后，才写确认。
5. 等同一进程 QQ/Web 就绪和健康窗口完成。后续业务阶段的学习、对话和自主探索单独观察；不能再整体严格比较说法账本并拒绝合法业务推进。

旧二进制没有此 hook，不能假装给出了入站前检查点。旧版恢复验收仍须在进程停止后严格比较。检查点不会自动回写旧状态或补造成功，不修改持久业务 schema。

## 监督器一次性移交

`host_runtime_bridge.py` 与 `self_upgrade_runtime.py` 声明 `STARTUP_CHECKPOINT_PROTOCOL = 1`。固定 host 只在第一次成功 `Popen` 后从自身环境移除 `EVE_STARTUP_CHECKPOINT_REQUEST`，首次 supervisor 因此继承请求；supervisor 同样在成功创建首次业务进程后移除环境变量。创建失败不会先移除请求。

两层分别保留“首启尚未就绪”的内存状态，只有它们各自观察到此次 owned 进程真实 ready/running 并通过健康窗口后才解除。此前的首启失败、超时或拒绝不能自动重启、回退成无检查点的业务进程。固定 host 返回阻断状态，supervisor 返回 `RECOVERY_STOP`；发布端保留失败现场并以新票据执行显式恢复。已经就绪后的普通自动重启不再继承旧请求，也不会重用旧确认。

生产固定路径 `/opt/eve/runtime-host-v1/host_runtime_bridge.py` 不随候选源码自动更新。发布前必须核对并安装该固定 host 的独立兼容补丁；只更新 candidate runtime 或只看到协议常量，都不能证明固定 host 已具备一次性移交和失败阻断能力。

## 验收

Rust 单元覆盖原字节摘要、缺失与空账本区分、进程/文件绑定、确认拒绝、超时与替换 sink。QQ 插件用真实 Kernel 和本地 Node marker 证明归档先绑定，确认前没有桥进程/状态服务，拒绝后不 ready。

`connectors/qqbot/test/startup_checkpoint_test.py` 用实际二进制和本地模型、QQ 替身验证完整 → 最小 → 完整组合。待学说法在屏障前不执行，确认后才完成学习；版本和已学结果跨组合保留，坏回执、旧确认、复用请求和超时不能开放通道。此用例不替代发布端对含真实冷记录完整状态的恢复证明。

Python 监督器与固定 host 用真实隔离子进程验证环境仅移交一次、创建失败保留请求、首启失败不自动绕过、成功就绪后正常重启。既有无请求的启动、升级和恢复用例继续运行。
