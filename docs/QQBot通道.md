# QQBot 通道插件

首版接入官方 QQ 开放平台的 C2C 私聊与群 @ 文本。复用腾讯 `@tencent-connect/qqbot-nodejs` 1.0.4，使用 Node 22 桥接 WebSocket 与被动文本回复；Rust `eve-qqbot-plugin` 只依赖公开 Control/Session/Plugin 契约，宿主 `eve-qqbot` 与终端共用 AGENT、配置、主模型、会话及 echo 装配。Kernel 不包含 QQ 业务。来源、版本与 MIT 许可见 [THIRD_PARTY](../connectors/qqbot/THIRD_PARTY.md)。

## 启动与手机配置

AppID 默认固定为 `1904159860`，无需创建 AppID Secret；可选 `QQBOT_APP_ID` 环境覆盖用于更换机器人或测试。密钥只由 `QQBOT_APP_SECRET` 环境提供，模型凭据沿用 `EVE_OPENAI_API_KEY`，都不写入参数、普通配置、状态或日志。

```sh
cd connectors/qqbot
npm ci --ignore-scripts
cd ../..
cargo build -p eve-app --bin eve-qqbot --locked
# 启动前在宿主环境提供 QQBOT_APP_SECRET 和 EVE_OPENAI_API_KEY。
QQBOT_SANDBOX=true ./target/debug/eve-qqbot --state-dir .eve-qqbot
```

核心默认 deepseek-v4.1-flash / Chat / none；模型、协议和基址仍按[核心对话](./核心对话.md)配置。默认启动正式 QQ API；首次测试使用 `QQBOT_SANDBOX=true`，非法布尔值拒绝启动。QQ Bot、测试用户和测试群的权限/沙箱范围按当前开放平台控制台配置。沙箱与正式 API 使用不同固定基址，token 地址固定，不允许通过事件修改。

主模型角色与终端共用同一装配：EVE_OPENAI_MODEL_ROLE=primary 显式使用 runtime.models.primary，默认仍按原 provider.openai 启动。角色的模型、期限和输出限制在进程启动时捕获；更新后重新启动，同一会话恢复已完成历史。字段、凭据引用拒绝与失败保护见[核心对话](./核心对话.md#显式选择主模型角色)。这只接入主模型，其他角色仍是独立配置定义。

手机可在 [GitHub Actions Secrets](https://github.com/ZenthXSin/Eve.aic/settings/secrets/actions) 添加 `QQBOT_APP_SECRET`；已有模型 Secret 继续使用。代码合入 main 后，在 [QQBot 首次真实交互](https://github.com/ZenthXSin/Eve.aic/actions/workflows/qqbot-interaction.yml) 点击 Run workflow，默认沙箱与 300 秒窗口；日志出现窗口启动后，向测试机器人私聊或在测试群 @ 发送文字。至少一次接收、模型完成和 QQ 成功发送且没有失败，报告才判定通过。超时退出不等于成功交互。

这只是工作流测试，没有部署到测试服务器。托管 Runner 的临时状态在任务结束后清理，不是持续运行的机器人宿主；长期运行应使用持久目录与进程管理。真实交互只上传无正文的计数报告，不上传 Session、QQ openid、输入、回复、密钥或原始诊断。

## 桥接契约与兼容

Node → Rust JSONL：`ready`、`message {id,scope,target_id,user_id,text}`、`delivery {id,ok,message_id?}`、`warning`、`fatal`；Rust → Node：`reply {id,text}`、`finish {id}`、`stop`。双方发布 `version:1`。缺失/不同版本记录 warning 后按已知字段处理；额外消息字段记录 warning 后忽略。非法路由、空文本、未知或超长帧跳过，密钥缺失、损坏持久化状态和不能确认的运行失败明确报告。

stdout 专用于 JSONL，SDK 日志后端为空，异常正文/stack/token 不转发。桥接子进程清空宿主环境，只保留基本执行环境与 QQ 凭据，不接收模型 key；凭据不放 argv。单帧 64 KiB、输入/回复 32 KiB，Node pending 最多 128、Rust 待运行最多 16。容量满时 warning 并拒绝新增，保留已有消息和状态。

只订阅 group/C2C intent `1 << 25`，跳过机器人消息、非文本和不支持的范围。Node 保存原事件的 ReplyTarget；Rust 只能凭原消息 id 回复，不能在命令中指定任意目标。被动回复始终关联原 scope、target 和 msg_id，不自动改为主动推送。QQ群原事件来源、SDK 可选元数据与平台错误码不影响合法文本处理。

## 去重、提交与恢复

插件在 Context 的 `receipts.v1` 保存带 AppID、原路由、输入、阶段与回复的有界回执文档：最多 4096 项、1 MiB；新任务预留最大回复序列化空间。状态严格校验，坏版本/坏字段/重复 ID/损坏字节拒绝启动，不清空、不覆盖、不淘汰旧回执。

收到新消息先持久化 Processing，再提交 Control。仅模型成功且 Session 已完成提交时，将回复保存为 ReplyPending，随后只调用一次 QQ sendText；delivery 成功改为 Sent，发送失败改为 Failed 并保留已提交历史与回复。网络、回执或停止使结果不确定时保留阶段，不自动重新生成、重新执行工具或重发。相同 AppID/message id 重复、改正文或改路由均 warning 后忽略，不覆盖旧回执。去重不宣称在跨服务崩溃时能实现事务性的 exactly-once。

重启时 Processing 与 ReplyPending 仍是待人工诊断的记录，不自动继续；Session 沿用 Pending → Interrupted 与只回放 Completed 历史的语义。新消息可以恢复已有完成历史，旧工具仅作为历史传给模型。取消不会撤销已经完成的外部副作用。

首版映射集中在 `Message::session_key`：AppID、scope、target 和发送者共同确定会话。这是首次测试、路由和恢复的临时边界。后续 AGI/内生驱动阶段统一认知主体、记忆、目标和跨通道经验，保留来源与权限，迁移旧历史；通道不是独立人格。详见[企划案](./企划案.md#通道隔离与统一认知的阶段关系)。

## 停止与验收

宿主收到 Ctrl+C / Unix SIGTERM 时，先通过 QQ 状态服务 request_stop 请求停止，等待在途 Control 取消并保存、桥接 stop 与子进程退出，再停止 Kernel 插件并刷新日志。这样避免 Kernel 的生命周期准入等待与在途模型形成等待冲突。桥接子进程不配合时有界终止并回收，不以发出信号代替确认退出。非关键消息/投递故障使用 warning 并继续；状态损坏和保存失败保留数据并终止。

```sh
npm --prefix connectors/qqbot test
cargo build -p eve-app --bin eve-qqbot --locked
python3 connectors/qqbot/test/eve_e2e.py
```

离线验收实际运行 Eve、生产 Rust 插件、测试 Node 子进程与 loopback Chat HTTP，覆盖工具/回复、两进程回忆与去重、群发送者/AppID 路由、发送失败保留提交、Processing 不重放、损坏状态保留、SIGTERM 取消与 Node 回收。Node 测试另覆盖 C2C/群原目标与 msg_id、pending 上限、finish、无重试发送失败、坏/超长 JSONL 与 EOF。它们与真实 QQ 交互分别记录，离线通过不代表 QQ 权限、认证或消息发送已经通过。

显式 primary 接线新增两个实际 Eve 用例：角色模型/输出上限控制与重启恢复、角色关闭后零请求且原状态不变。[PR #70 离线验收](https://github.com/ZenthXSin/Eve.aic/actions/runs/36958593968) 已通过七项 Node 与八项 Eve 进程用例，完整 CI 和性能证据见[模型配置验收](./模型配置.md#核心主模型接线验收)。

消息 ID 修复后的代码 `c548ea9b27ed62c3022d80ae8638eabf1c65d8ae` 已通过七项 Node 测试和六项 Eve 进程验收：[通道离线运行](https://github.com/ZenthXSin/Eve.aic/actions/runs/36905216689)，含官方带标点 msg_id 的原路回复、工具闭环与跨进程恢复。同一代码的[完整 CI](https://github.com/ZenthXSin/Eve.aic/actions/runs/36905216703) 三组全部成功：fmt、严格 Clippy、所有目标、工作区测试与文档测试、全部既有示例和 Rust 1.89 检查。真实 DeepSeek 三轮及恢复已通过，见[主模型验收](./主模型验收.md)。

首次[QQ 沙箱运行](https://github.com/ZenthXSin/Eve.aic/actions/runs/36903380200) 于 2026-10-01 17:59–18:02 UTC 检出消息 ID 修复前的 `090d94662797492cda8dd29acc7f925ec39c4c43`：认证与网关 ready 成功，180 秒后干净停止，但有效消息计数为零，未通过真实收发。未记录原始事件，不能据此反推用户未发消息。官方示例的 msg_id 包含 `.` 与 `!`，首版限制会误过滤；现已在 Node/Rust 分开验证 openid 和不透明消息 ID，保留原始标点及正文提及，并通过官方样例的路由、工具与恢复测试。来源为 [QQ 官方消息事件](https://github.com/tencent-connect/bot-docs/blob/645787a45937e5d9c4f0f61afefdffde0f38696e/docs/develop/api-v2/server-inter/message/send-receive/event.md)。

2026-10-02 的[真实 QQ 沙箱复验](https://github.com/ZenthXSin/Eve.aic/actions/runs/36949675220) 已通过。工作流固定检出 `1ab038ea2d155aef0309fd8a68c2de872f2e71e1`，该版本的[完整 CI](https://github.com/ZenthXSin/Eve.aic/actions/runs/36906219142) 与[通道离线验收](https://github.com/ZenthXSin/Eve.aic/actions/runs/36906219129) 均成功。实际窗口为 UTC 01:11:36–01:21:37，持续 600 秒；采用沙箱、内置 AppID 和 deepseek-v4.1-flash / Chat / none，结束时 SIGINT 收尾，进程退出码 0。安全报告：

```json
{"process_ok":true,"interaction_ok":true,"ready":true,"closed":true,"terminal_error":false,"received":10,"completed":10,"sent":10,"failed":0}
```

10 条有效消息均完成模型处理并由 QQ SDK 确认发送成功，失败计数为 0；用户在窗口内也确认交互正常。此结果证明本次 QQ 文本接收、模型完成、原路回复与干净停止；安全计数不区分 C2C/群，也不记录正文，不能推断每种通道均已实测，或本次执行过多少工具调用。工具、路由、去重与恢复证据仍见独立离线及主模型验收。

本次只使用 GitHub 托管 Runner，没有部署测试服务器。仅上传无正文计数报告，原始诊断、会话和回执未上传。一次性 PR 触发已移除，工作流恢复为仅 main 的手动入口；需要长期在线或保留本次 Runner 状态，应另行使用持久宿主。

媒体、QQ 频道、webhook、主动消息、消息修订/并行调度和凭据库接线后置；首版只做已验证的文本闭环。
