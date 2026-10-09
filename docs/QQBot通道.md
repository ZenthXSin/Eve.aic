# QQBot 通道插件

新增[内置表达与主动提问训练](./主动提问训练.md)：从用户原文保存表达统计，`/train start`、`/train stop` 按可信会话启停，`/train stats` 查看、`/train reset` 重置；六小时上限测试结束后保存计数报告和加密交流证据。群聊处理官方 @ 事件或携带已确认自身提及的群事件，平台授权及沙箱群范围仍须在 QQ 开放平台控制台配置。

首版接入官方 QQ 开放平台的 C2C 私聊与群 @ 文本。复用腾讯 `@tencent-connect/qqbot-nodejs` 1.0.4，使用 Node 22 桥接 WebSocket 与被动文本回复；Rust `eve-qqbot-plugin` 只依赖公开 Control/Message/Session/Plugin 契约，宿主 `eve-qqbot` 与终端共用 AGENT、配置、主模型、会话及 echo 装配。Kernel 不包含 QQ 业务。来源、版本与 MIT 许可见 [THIRD_PARTY](../connectors/qqbot/THIRD_PARTY.md)。

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

本地持久部署可追加 `--database-config /仓库外/连接.json`，把会话、回执、训练开关与表达统计写入同一个本地 PostgreSQL；默认仍使用文件后端。新目录必须没有 `state.json`，此参数不迁移现有 QQ 历史。目录首次绑定 SQL 后，漏参数或更换目标会拒绝启动；完成历史恢复、旧消息不重放及投递不确定时不补发的边界不变。普通配置仍在 `--state-dir/configuration`。配置、单宿主排他和独立测试库验收见[PostgreSQL 状态](./PostgreSQL状态.md)。

主模型角色与终端共用同一装配：EVE_OPENAI_MODEL_ROLE=primary 显式使用 runtime.models.primary，默认仍按原 provider.openai 启动。启动时预检角色，随后在每轮开始时捕获模型、期限和输出限制；配置服务更新影响新轮，同一会话继续保留已完成历史。字段、凭据引用拒绝与失败保护见[核心对话](./核心对话.md#显式选择主模型角色)。这只接入主模型，其他角色仍是独立配置定义。

手机可在 [GitHub Actions Secrets](https://github.com/ZenthXSin/Eve.aic/settings/secrets/actions) 添加 `QQBOT_APP_SECRET`；已有模型 Secret 继续使用。代码合入 main 后，在 [QQBot 首次真实交互](https://github.com/ZenthXSin/Eve.aic/actions/workflows/qqbot-interaction.yml) 点击 Run workflow，默认沙箱与 300 秒窗口；日志出现窗口启动后，向测试机器人私聊或在测试群 @ 发送文字。至少一次接收、模型完成和 QQ 成功发送且没有失败，报告才判定通过。超时退出不等于成功交互。

这只是工作流测试，没有部署到测试服务器。托管 Runner 的临时状态在任务结束后清理，不是持续运行的机器人宿主；长期运行应使用持久目录与进程管理。首次真实交互工作流只上传无正文计数；专用训练工作流另以加密包保存交流证据和状态，见[主动提问训练](./主动提问训练.md)。

当前用户验收直接在本地运行，不为实机交互调度工作流。沿用已有 `--state-dir` 保留会话、训练统计与回执；宿主输出 `EVE_QQBOT_READY` 后再通知用户去 QQ 测试。该标志只说明通道已就绪，实际收发仍需单独确认。

## 本地交互记忆

`--memory` 显式开启[交互记忆与明确偏好](./交互记忆.md)，默认关闭，可与训练和内生反思独立组合。`/remember 偏好内容` 保存明确偏好，`/memories [页码]` 分页查看，`/correct-memory 偏好ID 新内容` 修正，`/forget 偏好ID` 撤销；证据和旧版本保留。命令不调用模型，关闭时返回未启用，不转成普通聊天。

读取范围绑定当前可信 QQ 完整会话及用户，不跨 AppID、私聊/群、目标或发送者。已确认偏好由 `MemoryContext` 包装既有 `TrainingContext`，最多添加 8 条、8192 字节，当前请求优先；内部反思继续使用独立空 Context。`/train stop`、`/train reset` 不影响明确偏好。

普通消息只有在实际 Session 已 `Completed` 且 QQ `Sent` 已保存后才导入用户原文和最终回复；命令、控制替代轮、取消和失败轮不冒充成功交互。只观察本次新成功消息，不从旧回执补采；单独开启 `--memory` 不调用模型提炼。命令来源和偏好同次提交，但记忆与 QQ 回执没有跨服务事务：确认回复缺失时偏好可能已保存，已送达交互也可能因崩溃尚未导入。使用新 `/memories` 核对，恢复不自动重放。记忆与 QQ 宿主共用所选的文件或 PostgreSQL 后端，各插件仍独立提交；容量和失败恢复边界见[交互记忆](./交互记忆.md#持久化与失败恢复)。

## 本地偏好候选提炼

`--memory-learning` 在 `--memory` 基础上开启[低频偏好提炼](./偏好提炼.md)，默认关闭，缺少 `--memory` 时拒绝启动：

```sh
./target/debug/eve-qqbot --state-dir .eve-qqbot --memory --memory-learning
```

同一可信会话至少积累 3 条尚未消费的完成交互才可开始，每批最多 8 条、完整批次 JSON 最多 32768 字节，同范围从上次批次开始起冷却 5 分钟。首次开启可处理 Memory 中此前已经保存的证据，不补采 QQ 旧回执。每次进程启动所有范围合计最多 4 批，每批最多一次 30 秒、零工具的主模型请求；不读取其他范围、训练统计、明确命令证据或反思 Context。无新证据时不调用模型，失败和空结果也不重试原批。

`/memory-candidates [页码]` 每页查看最多 5 条候选，包含完整正文、来源证据 ID、自评和期限；模型自评不是事实概率。`/accept-memory 候选ID` 明确确认后，才成为下一轮 Context 可以使用的偏好。首次确认期限为创建后 7 天；已确认偏好仍由 `/correct-memory`、`/forget` 控制，重复接受不覆盖修正或恢复撤销。候选不会主动推送，这些查询与确认命令均不调用模型；关闭时返回“偏好提炼未启用”。

Learning 批次先保存 `Running` 和输入，再发起请求；恢复将残留 `Running` 记为 `Interrupted`，不自动继续。确认只用一次 Memory CAS 保存真实命令与偏好，Learning 候选保持不变；新查询核对首版来源与当前偏好状态，避免跨插件接受状态双写。新增 `eve.learning/learning.v1`，不改变原 `memory.v1`；文件和 PostgreSQL 都按各插件独立提交，损坏或提交无法确认时停止并保留状态。全局容量、输入和输出边界及恢复说明见[偏好提炼](./偏好提炼.md)。

## 分段投递

追加 `--segmented` 后，已完成的模型回复按自然段分成至多 3 条消息，段间停顿至多 2.5 秒；命令确认整条发送。默认关闭，未开启时协议与回执格式不变。

```sh
./target/debug/eve-qqbot --state-dir .eve-qqbot --segmented
```

公开 `QqBotPlugin::with_segmenter` 可以设置更小的单段字节预算；规划失败也不能绕过预算整条发送。无法合法降级时保留已完成 Session 和完整失败回执、不导入交互记忆，当前桥接条目结束后继续处理新消息。

每段写出前重新核对当前代，`/cancel`、`/add`、`/correct` 或训练开关会关闭剩余片段；已发片段无法撤回。片段失败不重试，全部片段送达才记为 `Sent` 并导入交互记忆。回执以字节范围记录每段状态，重启不补发也不重发。规划规则、回执格式 2 与恢复语义见[分段投递](./表达偏好与分段输出.md#首版分段投递已实现)。

开启分段后，每个会话可以用 `/segment` 查看，用 `/segment on|off|reset`、`/segment parts 2至5`、`/segment pace 0至200` 修改，从下一条开始投递的回复生效；未开启 `--segmented` 时回复“未开启”。设置独立保存在 `eve.segment.preferences`，损坏时拒绝启动且不清空，见[会话分段设置](./表达偏好与分段输出.md#会话分段设置已实现)。

同时开启 `--memory --segmented` 后，用 `/segment suggestions [页码]` 从本会话已确认偏好查看节奏建议，`/segment adopt 偏好ID 版本` 明确采用。`--memory-learning` 的聊天候选需先 `/accept-memory` 确认，也可直接 `/remember 回复最多分成两段，段间不要停顿。`。查看只读、采用一次应用涉及字段；纠正来源后旧版本被拒绝，撤销来源后不能再次采用。已应用的设置可用 `/segment reset` 恢复默认；命令与质量边界见[节奏建议](./表达偏好与分段输出.md#qq-已确认偏好的节奏建议首版已实现)。

## 本地内生反思

`--cognition` 显式开启后台反思，默认关闭；普通聊天和训练开关不会自动开启它。每次启动最多执行 32 项，可通过 `--cognition-max-executions 1` 等值限制为 1–32 项。启动示例：

```sh
# 凭据与普通 QQ 启动相同；继续使用原状态目录。
./target/debug/eve-qqbot --state-dir .eve-qqbot --cognition --cognition-max-executions 1
```

| 命令 | 行为 |
| --- | --- |
| `/goal 待办内容` | 保存明确提出的待办；正文最多 8192 UTF-8 字节，保存后由后台检查是否需要反思 |
| `/goals` | 列出当前可信会话最多 10 项待办的 ID、状态和版本 |
| `/goal-feedback 目标ID 版本 反馈内容` | 对该版本补充用户事实或纠正；正文最多 4096 UTF-8 字节，保存后后台按新版本重新反思 |
| `/mind` | 查看当前会话最近保存待办的反思状态或草稿 |
| `/mind 目标ID` | 查看当前会话指定待办当前版本的反思状态或草稿 |

命令不会等待模型返回；查询只读取已保存状态。关闭认知时，这些命令回复“内生反思未启用”；参数不合法时返回用法，不转为普通模型请求。普通聊天不被自动提取为待办，`/goal` 也不授权工具操作。内部完成不会主动发 QQ 消息，用户用 `/mind` 发起查询后才原路回复。

反馈命令使用 `/goals` 或 `/mind` 返回的目标版本，例如 `/goal-feedback 目标ID 1 实际只能用一页，请保留预算约束`。保存成功会返回新版本；旧版本被拒绝而不覆盖他人或并发更新。来源、原文与目标版本在同一次认知提交中保存，原目标和预算不变；用户提供的事实尚未经独立验证。新版本没有已完成草稿时，`/mind` 显示等待或执行状态，不再把旧完成草稿当作当前建议。已在执行的旧草稿可正常收尾并保留为历史；重规划仍受本次启动上限约束。完整契约见[反馈驱动重规划](./反馈驱动重规划.md)。

父目标保存为 `Waiting`、用户来源 `qq.goal`。内置规划器对同一父目标同一修订最多派生一次反思；每个子目标至多一次模型请求、零工具调用，最长 30 秒。草稿必须是已提交 Session 中通过结构验证的 `summary`、`next_step`、`needs_user_input`。子目标 `Completed` 只表示草稿已保存，父目标仍为 `Waiting`，建议和现实结果尚待验证。达到启动上限后，未开始的待办保留；新的启动可以继续评估，已尝试的同修订目标不会因此重试。

反思使用独立 Context，以及通过独立插件 ID 和服务 ID 注册的 Control：只接收 AGENT 身份、目标输入及受限输出格式，不读取聊天训练、表达统计、偏好或聊天历史，也不公布工具。前台聊天仍使用原 TrainingContext 和 Control；`/cancel` 等前台命令不控制内部反思代际。反思只执行当前有效、仍为 `Waiting` 的 QQ 用户父目标的子目标。命令中的目标身份与可见范围由通道绑定，按 AppID、C2C/群、目标和发送者共同隔离；正文里的用户或目标 ID 不能扩大读取权限。

QQ 插件只公开同步 `QqCommandHandler` 契约，不依赖认知实现。通道先持久化 `Processing`，再调用一次宿主命令处理器；处理器可返回独立回复，或在没有业务副作用时继续既有控制路径。认知保存与 QQ 回执没有跨插件事务：例如待办已经保存但回复尚未保存时崩溃，用户可能没有收到确认，重启也不会重放旧命令或补发回复。可用新消息 `/goals`、`/mind` 核对已保存记录。处理器异常不回退重跑，状态错误停止通道并保留现场。

默认 `run_qqbot` 使用 `ReflectionPlannerFactory`；受信 Rust 宿主可通过 `run_qqbot_with_planner_factory(options, factory)` 注入公开 `EndogenousPlannerFactory`。只有显式开启认知才创建规划器；替换实现须遵守来源、权限、预算、修订去重与持久化契约。创建或规划失败不回退为默认实现；修订冲突在后续 tick 重新读取，其余异常进入统一收尾。启动阶段先装配认知与独立执行服务，成功启动 QQ 插件并取得状态句柄后才开放后台规划和执行；启动失败不趁收尾发起反思。

此入口默认使用 `FileStateStore`；追加 `--database-config` 时，认知、Session、训练及通道回执共同使用同一个 PostgreSQL 后端，目录继续保留锁、数据库绑定与普通配置。旧目录不自动迁移，数据库提交结果不确定时不重放，见[PostgreSQL 状态](./PostgreSQL状态.md)。内生驱动的范围、恢复与后续能力见[认知循环](./认知循环.md#qq-本地组合入口)；本切片尚不具备完整 AGI、目标执行或现实结果验证能力。

## 桥接契约与兼容

Node → Rust JSONL：`ready`、`message {id,scope,target_id,user_id,text}`、`delivery {id,index?,push?,ok,message_id?}`、`warning`、`fatal`；Rust → Node：`reply {id,text}`、`segment {id,index,count,text}`、`finish {id}`、`push {id,target_id,text}`、`stop`。`push` 只用于宿主开启的主动私聊邀请：不带消息 ID，目标只取自 Rust 已确认的私聊回执，回执以 `push:true` 区分，失败不重试。`segment` 必须从 0 起按序、一段完成后才发下一段，`count` 为 2–5 且同一消息不变，不能与 `reply` 混用；末段送达或任一段失败后释放该消息，段间 `finish` 关闭剩余片段。双方发布 `version:1`。缺失/不同版本记录 warning 后按已知字段处理；额外消息字段记录 warning 后忽略。非法路由、空文本、未知或超长帧跳过，密钥缺失、损坏持久化状态和不能确认的运行失败明确报告。

stdout 专用于 JSONL，SDK 日志后端为空，异常正文/stack/token 不转发。桥接子进程清空宿主环境，只保留基本执行环境与 QQ 凭据，不接收模型 key；凭据不放 argv。单帧 64 KiB、输入/回复 32 KiB，Node pending 最多 128、Rust 待运行最多 16。容量满时 warning 并拒绝新增，保留已有消息和状态。

平台消息 ID 是不透明字符串，原始标点和内容必须保留。Node 入站/投递与 Rust 入站/恢复统一按 UTF-8 字节限制为 253 字节；这能接纳本次正式群实际出现的 137 字节 ID，并使 `qq:` 前缀后的 Control 任务 ID 保持在公开契约的 256 字节内。空值、首尾空白、控制字符与超长 ID 仍拒绝；AppID、用户和目标 openid 继续沿用独立的 128 字节字母数字/下划线/连字符限制，不随消息 ID 放宽。该修复未改变回执格式，原有回执原样恢复，新长 ID 保存后也能去重与恢复历史；不截断或改写 QQ 被动回复的 `msg_id`。

只订阅 group/C2C intent `1 << 25`，跳过机器人消息、非文本和不支持的范围。群 @ 接纳 `GROUP_AT_MESSAGE_CREATE`；`GROUP_MESSAGE_CREATE` 必须携带服务端 `mentions[].is_you === true`，或正文中的当前 AppID 标记 `<@AppID>` / `<@!AppID>`，才进入处理。这与锁定 SDK 的提及识别方式一致；普通群消息、仅提及其他账号、未知事件和不完整标记仍跳过，不把收到群消息等同于收到自身 @。Node 对正文去除首尾空白，群消息另移除开头连续的已确认自身提及，使 `@机器人 /goal 内容` 等命令可识别；自身身份来自 AppID 或 `is_you` 元数据中的有效 ID，正文中其他位置的提及及其他账号提及保持原值。Node 保存原事件的 ReplyTarget；Rust 只能凭原消息 id 回复，不能在命令中指定任意目标。被动回复始终关联原 scope、target 和 msg_id，不自动改为主动推送。QQ群原事件来源、SDK 可选元数据与平台错误码不影响合法文本处理。

## 去重、提交与恢复

插件在 Context 的 `receipts.v1` 保存带 AppID、原路由、输入、阶段与回复的有界回执文档：最多 4096 项、1 MiB；新任务预留最大回复序列化空间。状态严格校验，坏版本/坏字段/重复 ID/损坏字节拒绝启动，不清空、不覆盖、不淘汰旧回执。没有分段记录时文档为格式 1；出现分段片段后写为格式 2，旧版本程序拒绝读取。

收到新消息先持久化 Processing，再提交 Control 或调用已识别命令的宿主处理器。普通模型回复仅在模型成功且 Session 已完成提交后保存为 ReplyPending；同步命令回复在处理器返回后保存为 ReplyPending。未开启分段时随后只调用一次 QQ sendText；delivery 成功改为 Sent，发送失败改为 Failed 并保留已提交历史与回复。开启 `--segmented` 时每个片段调用一次 sendText：中间片段成功后整条仍为 ReplyPending，末段成功才改为 Sent；任一片段失败则整条为 Failed，已发片段保持 Sent、其余为 Skipped，见[持久化与恢复](./表达偏好与分段输出.md#持久化与恢复)。桥接收到 QQ 无法承载的正文（例如只含 U+FEFF）时直接回失败回执，Rust 记为 Failed，不再等待超时。网络、回执或停止使结果不确定时保留阶段，不自动重新生成、重新执行工具或重发。相同 AppID/message id 重复、改正文或改路由均 warning 后忽略，不覆盖旧回执。去重不宣称在跨服务崩溃时能实现事务性的 exactly-once。

重启时 Processing 与 ReplyPending 仍是待人工诊断的记录，不自动继续；Session 沿用 Pending → Interrupted 与只回放 Completed 历史的语义。新消息可以恢复已有完成历史，旧工具仅作为历史传给模型。取消不会撤销已经完成的外部副作用。

首版映射集中在 `Message::session_key`：AppID、scope、target 和发送者共同确定会话。这是首次测试、路由和恢复的临时边界。后续 AGI/内生驱动阶段统一认知主体、记忆、目标和跨通道经验，保留来源与权限，迁移旧历史；通道不是独立人格。详见[企划案](./企划案.md#通道隔离与统一认知的阶段关系)。

QQ 与终端共用每轮模型解析：在新代执行前捕获当前配置，工具往返保持原选择；宿主通过 ConfigAdmin 更新后，新轮使用新模型。无效新选择不创建 Session Pending、不回退缓存，QQ 回执仍按已有状态机保留失败记录，不自动重试。没有新增 QQ 配置编辑命令，环境或密钥变更仍需重启。可通过 `--message-judge primary|jev` 显式开启在途自然消息判断，独立配置与边界见[Jev 判断适配](./Jev判断适配.md)。

## 明确消息控制

普通文字保持串行排队。任一行去除前导空白后以 `/` 开头时，交给既有规则判断和 MessageService；不调用辅助模型。命令只绑定收到消息时同 AppID、scope、目标和发送者的完整当前代，不能控制另一用户或群的任务。

| 命令 | 行为 |
| --- | --- |
| `/add 内容` | 补充当前请求；先取消并等待，再以原文修订开启下一代 |
| `/correct 内容` | 纠正当前请求，可与 `/add` 多行组合 |
| `/cancel` | 取消当前任务并返回确认，已完成工具操作不会撤销 |
| `/new 内容` | 按明确新任务路径执行，不把它当成原任务的安全重试 |
| `/pause`、`/resume`、`/answer` | 暂停/恢复尚未支持；桥接未提供可信 question 引用，答复请求需澄清 |

没有当前代时回复提示，不自动把控制命令变成普通任务。已有工具执行或副作用未知时，补充/纠正只澄清，不重跑工具；Pending/Unknown 阻塞替代。修订输出关联控制消息的原始 QQ 目标，旧消息结束投递；已完成历史保留。

通道按可信会话处理路由动作与结果接纳，外发仍逐条确认，写出前复查完整代际。开启自然判断后，不同会话可独立推进；同会话的回复与新任务等待自己的路由收尾，完整有效的任务修改命令撤销该会话尚未完成的自然判断，只读或无效命令不撤销。排队旧结果失效后不外发；已经送交 QQ 的消息无法撤回。普通消息和控制消息共用最多 16 项待处理容量，控制消息同样先保存 Processing 并去重。重启不会重放控制命令或恢复旧代句柄；Processing/ReplyPending 保留原有人工诊断边界。

## 停止与验收

宿主收到 Ctrl+C / Unix SIGTERM、桥接 EOF、启动失败、认知或偏好提炼后台异常时，统一收尾：先通过 QQ 状态服务 request_stop 关闭通道准入，同时停止认知规划和提炼，并取消、等待内部执行；随后等待前台 Control 保存、桥接 stop 与子进程退出，最后停止 Kernel 插件并刷新日志。这样避免 Kernel 的生命周期准入等待与在途模型形成等待冲突。即使后台规划任务异常，仍先等待认知循环实际结束；在途提炼正常取消后保存失败结局，突发退出则在重启时标记 Interrupted。桥接子进程不配合时有界终止并回收，不以发出信号代替确认退出。非关键消息/投递故障使用 warning 并继续；状态损坏和保存失败保留数据并终止。

认知恢复将旧 `Executing` 标为 `Blocked/Interrupted`，保留执行与 Session 关联；Session 的旧 Pending 仍转为 Interrupted。已完成草稿可由新查询读取，失败、取消或不确定的同修订反思不自动重试；QQ 的旧 Processing/ReplyPending 也不自动发送。认知、Session 和 QQ 回执各自保存，不能从任一记录缺失推断另一项业务从未发生。

```sh
npm --prefix connectors/qqbot test
cargo build -p eve-app --bin eve-qqbot --locked
python3 connectors/qqbot/test/eve_e2e.py
python3 connectors/qqbot/test/cognition_test.py
python3 connectors/qqbot/test/memory_test.py
python3 connectors/qqbot/test/learning_test.py
python3 connectors/qqbot/test/segment_test.py
```

离线验收实际运行 Eve、生产 Rust 插件、测试 Node 子进程与 loopback Chat HTTP，覆盖工具/回复、两进程回忆与去重、群发送者/AppID 路由、发送失败保留提交、Processing 不重放、损坏状态保留、SIGTERM 取消与 Node 回收。Node 测试另覆盖 C2C/群原目标与 msg_id、pending 上限、finish、无重试发送失败、坏/超长 JSONL 与 EOF。它们与真实 QQ 交互分别记录，离线通过不代表 QQ 权限、认证或消息发送已经通过。

新增认知进程验收覆盖默认关闭与无效命令零请求、后台反思不受训练上下文影响、前台取消与后台执行独立、群/用户/AppID 隔离、已保存草稿跨进程查询与去重、非法草稿阻塞、SIGTERM 取消收尾和再次启动零重放。这些使用本地 HTTP 替身，不代表新增 QQ 命令已完成实机验收。

偏好提炼进程验收另覆盖三条完成交互门槛、Running 先于 HTTP 请求保存、候选不自动注入 Context、确认后修正撤销及重复接受、可信范围隔离、持久冷却和每次启动四批上限，以及错误、取消、突发退出后的零重试。实际模型候选质量与 QQ 新命令仍须本地实机验证，不能以替身返回的合格 JSON 代替用户认可。

显式 primary 接线新增两个实际 Eve 用例：角色模型/输出上限控制与重启恢复、角色关闭后零请求且原状态不变。[PR #70 离线验收](https://github.com/ZenthXSin/Eve.aic/actions/runs/36958593968) 已通过七项 Node 与八项 Eve 进程用例，完整 CI 和性能证据见[模型配置验收](./模型配置.md#核心主模型接线验收)。

本轮明确控制的离线验收扩展为 18 项实际 Eve 进程用例及 7 项 Node 测试，覆盖取消、原文修订、先保存后替代、工具副作用澄清、跨群/用户隔离、重复消息、替代轮停止恢复，以及完成旧回复排队时的代际过滤。以下历史真实 QQ 记录不覆盖新增控制命令；未知工具副作用与慢工具析构窗口依赖独立 Rust 契约验证。

消息 ID 修复后的代码 `c548ea9b27ed62c3022d80ae8638eabf1c65d8ae` 已通过七项 Node 测试和六项 Eve 进程验收：[通道离线运行](https://github.com/ZenthXSin/Eve.aic/actions/runs/36905216689)，含官方带标点 msg_id 的原路回复、工具闭环与跨进程恢复。同一代码的[完整 CI](https://github.com/ZenthXSin/Eve.aic/actions/runs/36905216703) 三组全部成功：fmt、严格 Clippy、所有目标、工作区测试与文档测试、全部既有示例和 Rust 1.89 检查。真实 DeepSeek 三轮及恢复已通过，见[主模型验收](./主模型验收.md)。

首次[QQ 沙箱运行](https://github.com/ZenthXSin/Eve.aic/actions/runs/36903380200) 于 2026-10-01 17:59–18:02 UTC 检出消息 ID 修复前的 `090d94662797492cda8dd29acc7f925ec39c4c43`：认证与网关 ready 成功，180 秒后干净停止，但有效消息计数为零，未通过真实收发。未记录原始事件，不能据此反推用户未发消息。官方示例的 msg_id 包含 `.` 与 `!`，首版限制会误过滤；现已在 Node/Rust 分开验证 openid 和不透明消息 ID，保留原始标点及正文提及，并通过官方样例的路由、工具与恢复测试。来源为 [QQ 官方消息事件](https://github.com/tencent-connect/bot-docs/blob/645787a45937e5d9c4f0f61afefdffde0f38696e/docs/develop/api-v2/server-inter/message/send-receive/event.md)。

2026-10-02 的[真实 QQ 沙箱复验](https://github.com/ZenthXSin/Eve.aic/actions/runs/36949675220) 已通过。工作流固定检出 `1ab038ea2d155aef0309fd8a68c2de872f2e71e1`，该版本的[完整 CI](https://github.com/ZenthXSin/Eve.aic/actions/runs/36906219142) 与[通道离线验收](https://github.com/ZenthXSin/Eve.aic/actions/runs/36906219129) 均成功。实际窗口为 UTC 01:11:36–01:21:37，持续 600 秒；采用沙箱、内置 AppID 和 deepseek-v4.1-flash / Chat / none，结束时 SIGINT 收尾，进程退出码 0。安全报告：

```json
{"process_ok":true,"interaction_ok":true,"ready":true,"closed":true,"terminal_error":false,"received":10,"completed":10,"sent":10,"failed":0}
```

10 条有效消息均完成模型处理并由 QQ SDK 确认发送成功，失败计数为 0；用户在窗口内也确认交互正常。此结果证明本次 QQ 文本接收、模型完成、原路回复与干净停止；安全计数不区分 C2C/群，也不记录正文，不能推断每种通道均已实测，或本次执行过多少工具调用。工具、路由、去重与恢复证据仍见独立离线及主模型验收。

本次只使用 GitHub 托管 Runner，没有部署测试服务器。仅上传无正文计数报告，原始诊断、会话和回执未上传。一次性 PR 触发已移除，工作流恢复为仅 main 的手动入口；需要长期在线或保留本次 Runner 状态，应另行使用持久宿主。

媒体、QQ 频道、webhook 和凭据库接线后置；主动消息只用于[主动交流与邀请](./主动交流与邀请.md)的私聊邀请，默认关闭。明确修订已接线，自然消息判断现有默认关闭的 primary/Jev 实验入口；真实质量评估与暂停检查点仍待后续交付。

## 自主偏好学习

`--interest-learning` 同时开启记忆和认知，从普通聊天观察用户明确表达的兴趣、经验与困难，只保存能在原话中逐字核对的陈述，并派生低优先级后台学习目标；`/interests` 查看，`/forget-interest 兴趣ID` 撤回并取消目标。默认观察间隔五分钟，`--interest-cooldown-ms` 可调整，见[兴趣观察](./兴趣观察.md)。

`--research-source URL`（可重复，需同时 `--interest-learning`）为等待中的学习目标在入口页面同源目录内受控研究：只读 GET、每个目标修订至多一次、累计至多三次，失败不重试、中断不重放。`/knowledge 兴趣ID` 查看来源原文（网址、抓取时间、版本、逐字引用）与未验证推测，见[受控研究与领域知识](./受控研究与领域知识.md)。

`--practice-mindustry-server jar`（需同时 `--interest-learning`）为等待中的学习目标制作只含数据文件的最小 Mindustry 模组，用操作者提供的无头服务端在全新目录中实际加载并探测内容属性，至多三次尝试、依据运行证据修正；只有实际加载且全部探测通过才记为已验证。`/practice 兴趣ID` 查看产物、运行版本、警告与探测期望/实际值，见[实践验证](./实践验证.md)。

`--skill-learning`（需同时 `--practice-mindustry-server`）把实际验证通过的实践提炼为参数化技能：宿主核对模板能逐字还原原产物，再用自己选取的不同参数在同一运行环境中实际运行通过才自动启用；同一用户的后续任务第一次尝试可选用技能。`/skills` 列出技能，`/skill 技能ID` 查看版本、验证证据、启用记录与调用，`/skill disable|rollback 技能ID`、`/skill enable 技能ID 版本` 停用、回退或启用，见[技能固化与复用](./技能固化与复用.md)。

`--outreach`（需同时 `--practice-mindustry-server`）在学习目标有了已验证的进展后撰写一条邀请：用户下次私聊找 Eve 时先判断时机，合适才作为最后一段随被动回复附带，以平台回执为准；`--outreach-proactive-after-ms` 开启等待后的主动私聊，`--outreach-cooldown-ms` 调整同一用户的送达间隔；`/outreach` 查看状态与回执，`/outreach off|on` 关闭或恢复，见[主动交流与邀请](./主动交流与邀请.md)。

`--self-learning` 同时开启记忆、持续提炼和分段，证据充分的候选自动确认，明确节奏自动参与后续发送。`/self-learning status` 查看模式及关联情况；手动分段选择优先，`/segment reset` 清除手动选择并恢复跟随学习。默认提炼间隔五分钟，`--learning-cooldown-ms` 可调整。行为、恢复和容量见[自主偏好学习](./自主偏好学习.md)。

## 本机控制面板

`--web-listen 127.0.0.1:8765` 显式启用本机面板，需要独立 `EVE_WEB_TOKEN`。浏览器查看本实例状态、会话/任务分页和精确代际取消；不提供新任务或停服操作。令牌、环回访问、取消准入与停止/恢复边界见[Web 控制面板](./Web控制面板.md)。
