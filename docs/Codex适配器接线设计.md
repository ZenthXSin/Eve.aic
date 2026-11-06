# Codex 适配器接线设计

本文记录原 Codex 适配器、主/辅模型共同构造、识图入口和显式切换工具的实现。尚未运行 Cargo、Rust 1.95 依赖解析与构建、新的 Rust 测试或新 QQ/Web 进程用例，也未切换生产或向真实 Codex 端点请求。基础会话 API 来自 `a2eb9e52c135ae03e36233541242f6fe287a8a91`，其最终验收提交 `c0ede7efed0a3b2ee2676b37c578fc8eb6c6da85` 已正式合并；原适配器草稿提交为 `78f2b526404a9d0dc3c939a428e51abfa642542d`。静态实现不算运行验收完成。

目标是在保留现有 Chat 默认路径的前提下，为主模型和辅助模型提供显式的原 Codex Responses 实现。模型名称、端点、协议和实现选择分开处理；失败不自动切换模型、协议、凭据或实现。

## 可选组合

已有 `runtime.models.<角色>_provider` 是实现标识，主、辅角色在这里允许 `openai` 或 `codex`，不另增与它竞争的开关。未选择角色的旧配置仍使用 `openai`。`provider.openai` 命名空间继续保存现有兼容端点、协议、reasoning 等字段，避免迁移现有配置；该名称不代表新 Codex 分支调用旧 HTTP/SSE 代码。

| 模型角色与 provider | 有效协议 | 请求实现 | 凭据边界 |
| --- | --- | --- | --- |
| 旧配置，`model_role` 为空 | `chat` / `responses` | 原 `eve-llm-openai` | 原主凭据 |
| `primary_provider=openai` | `chat` / `responses` | 原 `eve-llm-openai` | 原主凭据 |
| `primary_provider=codex` | 仅 `responses` | `eve-llm-codex` 调用原 Codex 客户端 | 同一主凭据租约，不读取新环境名 |
| `auxiliary_small_provider=openai` | `chat` / `responses` | 原 `eve-llm-openai` | 共享主凭据、显式辅助凭据或显式匿名 |
| `auxiliary_small_provider=codex` | 仅 `responses` | 原 Codex Responses 客户端 | 沿用同一辅助凭据引用检查 |
| `jev_provider=jev` | Jev 专用契约 | 原 Jev 接线 | 独立 Jev 凭据 |
| `semantic_provider=openai` | embedding 专用契约 | 原 embedding 接线 | 主或显式语义凭据 |

Codex 配合 `chat`、Chat 完整端点、缺少可信 `ModelSession` 或不支持的凭据引用时，请求前明确报错。Jev、embedding 和专门的匿名 idle 小模型不因主模型切换而改成 Codex。主、辅混用时，辅助协议和端点继续按 `auxiliary_protocol` / `auxiliary_base_url` 的现有继承规则处理。

## 配置、捕获与构造

`CoreModelResolver::capture` 在同一配置修订下捕获 provider、profile、用途、输出上限、流式与重试设置，实现选择随 `ModelProfile.provider` 进入捕获和缓存比较。旧 profile 与缺省值无需迁移，`openai + chat` 的默认值和既有请求构造保留。

组合层的 `model_provider::ProviderConfig` 承载 `OpenAiConfig` 或 `CodexConfig`，共同 `build` 入口对外只返回 `Arc<dyn LlmProvider>`。配置校验、`inspect_model_runtime`、面板和实际构造调用同一解析过程。实现由明确的 provider 标识选择，识图也使用该入口。状态读取不构造客户端、不读取凭据、不发送探测请求。

| 配置或能力 | Codex 对应行为 |
| --- | --- |
| `profile.model` | 原 `ResponsesApiRequest.model`，不由名字推断实现 |
| 当前有效 `base_url` | 与现有拼接规则一致，根地址补 `/v1/responses`，已有版本路径追加 `/responses`，完整 Responses 地址保留 |
| `profile.timeout_ms` | provider 单请求期限；用途 `timeout_cap` 继续缩小它 |
| 用途输出上限 | 非零用途值覆盖 profile；用途为零继承 profile；最终无值就省略 `max_output_tokens` |
| `reasoning_effort=omit` | 不发送 reasoning；其他值必须可由原 Codex `ReasoningEffort` 表示 |
| temperature | 永不发送 |
| 主/辅凭据 | 原 `CredentialBank` 按用途和引用产生租约，再交给适配器构造；不复制凭据到配置或诊断 |
| `EVE_OPENAI_TRANSIENT_RETRIES` | 宿主默认启用原 Codex 有界 HTTP 重试，设为 `false` / `0` 可关闭；直接新建 `CodexProvider` 默认不重试。整个请求共用总期限，应用完成/协议失败不自动换实现重试 |
| `EVE_OPENAI_STREAM_COMPLETIONS` | OpenAI 分支维持原行为；Codex 原客户端始终 SSE，关闭此项不会让它伪装成非流式 JSON HTTP |
| 首包/停顿/总期限 | 现有用途限制保留，转换成 Codex 同名时间策略；不重置等待或重试的总截止 |
| 文本 sink | Codex 原流完整验证后一次交最终正文；暂不提供逐字预览；工具批次不混入 commentary |
| usage/cache | 读取原 parser 的 usage；缺失保持未知，真实零仍为零，不把本地前缀相似度当供应商缓存命中 |

Codex 构造器拒绝携带凭据的非本地明文 HTTP，并拒绝 URL 用户信息、查询串和片段。该边界不需要改原源码，也不能通过创建匿名 Provider 再另附主凭据绕过。

## 期限与共享包装器

外层顺序继续为：角色准入 → 宿主总准入 → 自然对话改写 → 前缀观测 → 已捕获的 Provider。先验证候选 Provider 与凭据，再更新共享准入；无效配置不能改变在途额度。每个角色和用途继续使用原健康记录分类，不因新实现多领请求槽。

Codex 即使通过 `complete` 调用也使用原 SSE，`provider_timeout` 已按真实 transport 是否 SSE 计算，与 OpenAI SSE 同样保留一秒收尾余量。用途传入的 `timeout_cap` 继续限制提供方内部期限，speech 60 秒等组件外层硬上限仍生效。若外层硬上限先到，保留取消/失败语义，不假称已完整响应。

## 显式切换入口

QQ 命令和 QQ/Web 工具装配共用既有 `Commands` 的可信管理员身份与 `ConfigAdmin::replace`，没有模型审批步骤。一次 `ApplyMode::NewRequests` 事务保存 `runtime.models.<角色>_provider` 与有效协议；指定 `base_url` 时同时更新该角色地址。参数不能包含身份或密钥。角色须已经启用并配置模型；主角色切换同时设置 `provider.openai.model_role=primary`，辅助切换只改辅助设置，保持原用途路由。

```text
/model adapter primary codex responses
/model adapter primary openai chat
/model adapter auxiliary_small codex responses
```

工具名为 `set_model_adapter`，参数示例：

```json
{"role":"primary","adapter":"codex","protocol":"responses"}
```

`base_url` 可选，省略时保留该角色当前地址。如果原地址是完整 Chat 端点，切入 Codex 时必须在同次操作提供对应 API 根地址或完整 Responses 地址；不会擅自改写成另一地址。辅助独立地址继续要求显式辅助或匿名凭据引用。地址变更仅来自当前可信管理员操作，保存不解引用凭据也不访问目标；实际请求沿该角色的既有凭据域和传输约束。

Codex+Chat、协议与完整端点冲突、无效地址或引用在保存前拒绝。并发 CAS 冲突保留他人的最新配置，不提交半个切换；保存发生存储故障时明确报告结果无法确认，须重新读取配置。回执的 `provider_availability_verified=false` 表示未探测上游。模型名、reasoning、temperature 和输出限制不由切换工具调整。`/model 模型名` 与 `set_reasoning_effort` 仍各自只处理原来的配置。

## 会话、恢复与切回

宿主提供的 `ModelSession` 是唯一身份来源，主对话、后台任务、审查、发言和识图分别沿现有隔离键。适配器不从 prompt、模型回复或配置中的姓名推断身份，不创建全局 cache key。

原 `ResponsesClient` 的两个公开入口最终都调用同一个 `stream_encoded`，再经过原 HTTP、重试和 SSE 代码。Eve 使用原 `stream(Value, headers, ...)`：先由原 `ResponsesApiRequest` 序列化，再追加原请求类型没有的 `max_output_tokens`。没有直接采用便利入口 `stream_request`，因为它还会用 `thread_id` 覆盖 `x-client-request-id`；Eve 需要同一用户会话的稳定 thread ID，同时按持久轮次和请求序号区分每次调用。session/thread 头仍调用原 `build_session_headers`。这些是桥接层的会话和参数策略，不是原源码改写，也不能描述为整个 Codex 客户端的默认头、OAuth、请求参数完全相同。

`request_fingerprint` 同步只读编码实际 Responses body 和规范端点，使用独立 domain `codex-responses-wire-v1`。它不访问网络，不取得轮次状态锁，不读/写服务端 token，也不包含凭据或 `x-client-request-id`。`complete` 和 `stream` 的实际 body 相同，故指纹相同；JSON 专用请求不属于 Runtime 的 `ResponseMode`。

HTTP 重试复用当前 request ID；进程恢复时仅相同指纹可复用检查点中的待请求序号。输入、工具、配置、端点或适配器改变，指纹随之改变，宿主分配新序号并保留已完成工具，不重跑整轮。服务端 `x-codex-turn-state` 只在同一 Provider 内按会话和轮次保存，切换或重启后重新取得，不借用别的会话。

切入 Codex 时，在一个配置事务中同时设置角色 provider 和有效 Responses 协议/地址；切回时同样一次恢复该角色的 `openai` 和原 Chat 协议/地址。正在执行的一轮沿用已捕获 Provider，新一轮按新配置解析。切换不取消旧轮、不清空会话、不要求重启。两个适配器长期 ID 来自相同宿主身份，wire fingerprint 的 domain 不同；工具去重与已发送消息的恢复都继续由 Eve 原账本负责。

## 直接构造旁路

`qq_files::DescribeImageTool` 已将下载后的图片转换成普通 `ImageInput` / `ModelRequest`，经 `ImageModel` 和共同构造入口调用所选实现。图片沿公共契约识别 PNG、JPEG、GIF、WebP 的格式签名；其他 `image/*` 不发送模型。它沿当前主角色或旧配置，保持原有识图语义，不改走 dialogue 用途的辅助路由；捕获后模型、适配器、地址、凭据租约固定。

识图会话从可信工具上下文派生独立 `vision` 身份，使用主角色与宿主共享准入；本进程学习到的成功模型按“实现+地址”区分，重启重新从主模型开始。只接受最终文本，函数调用不算识图成功。失败时通过独立只读 `ModelCatalog` 执行同服务的 GET `/models`，不构造旧推理适配器，不携带会话头，不自动切换实现或模型。目录和原识图一样保持单次尝试，原下载及响应大小边界继续生效。

`idle_small` 是已有独立匿名 Chat 线索筛选器，保留其独立设置与说明；其提示改写包装器无法同步预测额外模型输出，因此保持 fingerprint=None，让恢复使用新请求序号。Jev、语义 embedding、模型目录等也各自沿公共契约，不宣称全部已经换成 Codex。

## 接线后的验收

需要真实运行后才可标记完成：原 Chat 和原 Responses 回归；主/辅每个可用组合；Codex+Chat 配置保存/请求前拒绝；原凭据拒绝与匿名边界；分用途输出上限；旧配置加载；无探测的状态查询展示 adapter；原 HTTP/SSE 完整批次；缓存未知/零；取消和重试期限；并发准入；相同持久请求恢复、输入变化和切回后新请求序号；主工具零重放；识图旁路的实际实现一致。

独立适配器已有 23 个验收用例和 1 个忽略的恢复子进程入口；宿主新增 8 个 loopback 组合用例及 4 个切换入口用例，均尚未执行。指纹用例设计为捕获实际请求核对摘要，并验证请求序号、服务端 token 和凭据不改变 body 指纹。组合用例含真实配置插件、普通会话工具往返、主辅各三种实现/协议的九种有效组合、独立主/辅服务、识图与目录实际入口；九组合中主、辅请求均直接断言 high 和无 temperature。切换入口用例含权限、持久化重开和实际 CAS 冲突。

另外新增 4 个真实宿主进程用例，尚未执行：QQ 指令切 Codex 后由普通对话实际发送 Responses SSE，模型调用切换工具后当前轮仍走 Codex、下一轮走 Chat，重启后重复入站不重放配置写入；独立 eve-web 经 `/api/chat/send` 在两个方向调用真实工具切换，验证当前轮、下一轮、两个独立 Web 会话和两次重启。两种入口各自检查可信身份与伪造参数拒绝，逐条捕获 HTTP 请求核对工具定义、high、无 temperature、模型名、输出上限、稳定会话/缓存键和独立请求 ID。请求总数必须恰好等于正常模型/工具往返，不容许额外目录探测、模型审查或失败回退。这些夹具使用本地合成模型响应，不代表外部供应商兼容性或真实模型自主能力已验收。

CI 已按实际 1.95 版本拆分：最低版本任务运行定向 Rust 回归与受影响包的严格 Clippy，QQBot 通道任务编译两种宿主并运行上述 4 个进程用例，Stable 两个平台继续完整回归。最低版本任务在依赖构建前校验原文件，关闭调试信息和增量编译，限时 45 分钟；各任务的实际结果仍待取得。

最小动态验收命令（先完成候选锁文件解析，再由真实 Rust 1.95 验证）：

```bash
python3 scripts/verify-codex-upstream.py
cargo +1.95.0 check --workspace --all-targets --locked
cargo +1.95.0 fmt -- --check
cargo +1.95.0 test -p eve-llm-api --lib --locked
cargo +1.95.0 test -p eve-llm-codex --test responses --locked
cargo +1.95.0 test -p eve-app --lib models::tests --locked
cargo +1.95.0 test -p eve-app --lib qq_model --locked
cargo +1.95.0 test -p eve-llm-openai --locked
cargo +1.95.0 test -p eve-runtime --test llm_host --test session_host --test session_http --test model_resolver --test context_compaction --test openai_provider --test streaming --locked
cargo +1.95.0 clippy -p eve-llm-api -p eve-llm-openai -p eve-llm-codex -p eve-runtime -p eve-app -p eve-developer-tools -p eve-web-panel-api --all-targets --locked -- -D warnings
cargo +1.95.0 build -p eve-app --bin eve-qqbot --bin eve-web --locked
EVE_QQBOT_BINARY=target/debug/eve-qqbot python3 connectors/qqbot/test/model_adapter_test.py
EVE_WEB_BINARY=target/debug/eve-web python3 scripts/test-web-model-adapter.py
```

随后运行现有工作区测试、示例、QQ 模型切换/模型健康/工具目录/识图实际进程及 Web 宿主回归。当前 Cargo.lock 的原 Codex 依赖闭包尚未解析，不能把上述命令标成已通过。

## 工具链与原文件保留

原 Codex workspace 声明 Rust 1.95。Eve 顶层及适配器统一最低版本为 1.95，CI 的 MSRV、各实际进程工作流、发行构建和便携包元数据同步；不修改 vendor manifest 降版本，不使用可选 feature 回避全工作区编译。过去的 1.89 成功记录保持历史版本，不冒充 1.95 结果。

格式检查在 Eve workspace 根目录执行 `cargo fmt -- --check`。`--all` 会递归 path dependencies，进入应保持原文的 Codex vendor；根目录默认模式依据 `metadata --no-deps` 检查 Eve 的全部 workspace targets。实际 metadata 与格式 target 清单仍须在 Rust 1.95 验证窗口核对，证明没有遗漏 Eve 成员或格式化 vendor；目前只有直接 rustfmt 的自有文件静态结果。

原包来源检查覆盖 577 文件、6171502 字节、24 个本地 package，Apache-2.0 与 NOTICE 保留。上游原 prompt 的一处行尾空白也忠实保留。这里使用的是 Codex 原 Responses 请求、HTTP、重试和 SSE 代码，不等于整个 Codex core：OAuth 刷新、WebSocket continuation、加密 reasoning 的持久历史、原生非函数工具和全部多模态输出不在当前 Eve 契约范围内。
