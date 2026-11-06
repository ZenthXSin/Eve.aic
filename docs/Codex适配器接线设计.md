# Codex 适配器接线设计

本文记录原 Codex 适配器、主/辅模型共同构造、识图入口和显式切换工具的实现。远端真实 Rust 1.95 已完成 metadata、离线锁核对与原文件校验，准确锁已原样取回，见[锁解析记录](Codex依赖锁解析记录.md)。提交 `d2aa3f9a` 对应的合并检验已通过真实 Rust 1.95 全工作区 all-targets 构建检查、274 项定向测试和七个受影响包的严格 Clippy，QQ/Web 四个适配器进程用例通过；Stable Linux 全工作区测试 2491 项成功、0 失败，压缩流六项通过，整个 Linux 作业成功。缩小调试产物后，QQ 完整候选仍在就绪阶段超时；QQ 长回归另在延后回复的插入消息场景失败。本次显式隔离热更新夹具的自动能力探测，并让延后回复夹具在退出前等待通道结束命令，保留记忆、开发工具和全部交接断言；远端新头及最终默认完整组合仍待验收。当前未切换生产或向真实 Codex 端点请求。基础会话 API 来自 `a2eb9e52c135ae03e36233541242f6fe287a8a91`，其最终验收提交 `c0ede7efed0a3b2ee2676b37c578fc8eb6c6da85` 已正式合并。Rust 定向通过不代表宿主完整运行验收完成。

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

`d2aa3f9a` 的 [MSRV 作业](https://github.com/ZenthXSin/Eve.aic/actions/runs/42178999666/job/127763978007) 与 [QQ/Web 作业](https://github.com/ZenthXSin/Eve.aic/actions/runs/42178999730/job/127763977889) 原日志均确认实际检出 `46224a79bcab87ffdbe73eec9df068889bf95b5c`，由基线 `8b6fd3d5955d44b0f015dd33f1f06d8366b12002` 与本 PR 头组成。真实 rustc/cargo 1.95.0 的定向测试再次 274 成功、0 失败、2 忽略；全目标检查与七包 Clippy 成功。Web 两项（34.671 秒）、QQ 两项（1.577 秒）通过，小日志 Artifact `13484532589` 的官方压缩摘要与内部日志摘要已核对。

同轮 [Stable Linux 作业](https://github.com/ZenthXSin/Eve.aic/actions/runs/42178999666/job/127763977849) 原日志确认相同合并检验提交，实际 Stable rustc 为 1.99.1。全工作区测试含 412 条结果行，合计 2491 成功、0 失败、24 忽略；反向特性树包含 `arbitrary_precision/default/raw_value/std`，fmt、全目标检查、工作区 Clippy 与整个 Linux 作业均成功。后续压缩流六项在 20.838 秒内全部通过，原 context_compaction 三项在 7.586 秒内通过。四条成功分支的真实估算为 524040 → 89057；六条均保留 35 条工具回执，摘要只在第三批之后触发，两个负向场景也保留全部回执。QQ 长回归没有到达这些用例，不能据 Linux 独立步骤的通过写成 QQ 全量通过。该 Rust run 的总体失败来自旧 Windows 作业；按用户要求，其共享矩阵由 PR #482 移除。

调试信息关闭后，热更新程序由 922,293,416 缩小至 173,807,768 字节，初始宿主 7.576 秒运行、9.090 秒交接。候选监督器于 11.109 秒发布 starting，20.333 秒仍未就绪，外层进入 `host_supervisor_not_ready` 回退，28.331 秒完成保留状态的回退；原 35 秒内没有提交候选。同版本重启通过。减小调试产物确实缩短了前段，但不能据此称候选启动问题已经解决。

两条手写热更新命令遗漏 `--minimal-profile`，默认组合会在发出 READY 之前同步探测最多三个浏览器，每个上限 5 秒。受控 PATH 只放三个慢合成浏览器，用固定 `52c2e0f3` / Rust 1.89 release 程序和实际测试桥验证：默认组合 11.036 秒仍未就绪、已依次探测三个浏览器；显式最小组合 0.160 秒就绪且没有浏览器探测。这证明夹具的自动能力发现足以与 10 秒就绪期限冲突，但未证明它是该次 CI 失败的唯一原因。

现为同版本重启和候选升级显式传入 `--minimal-profile`，保留 `--memory`、已有开发权限配置、真实宿主与候选包，以及 35/10 秒期限、平台 Sent、全部工具回执、第二进程零重放和状态恢复断言。相同旧 release 程序的两项完整夹具验收通过，用例共 9.153 秒，候选 5.150 秒提交、同版本 2.605 秒重启。旧产物 SHA-256 为 `55dc6a5dde12469a4d5ec3d5cd008d61d10810c4f6925e1a619df274ea375b38`；这只确认隔离夹具兼容性，不计作当前 PR / Rust 1.95 / Codex 实测，也不代表默认完整能力组合的启动慢已修复。最终 Debian 12、Rust 1.95 release 必须使用默认完整组合进行 QQ/Web 与升级预演，不能借用最小组合的通过结论。

该轮 QQ 长回归于 2026-11-06 07:33:15 UTC 自然结束，在 `deferred_reply_test.py::test_new_input_during_backoff_is_applied_before_the_next_request` 报 `QQBot 命令写入失败`，同组其余五项通过。实际检出版本的源码确认通道先保存 Handled，再发送 finish，而该手写场景只等 Handled 就让桥接进程退出。实际 Node 测试桥的受控时序复现：先写 Handled、延后 finish 时，旧脚本退出导致 BrokenPipe；补等 finish 后保持管道直到收尾。修复仅让该场景像公共 send 辅助方法一样等待结束命令，并新增恰好三个 Chat 请求和两个 finish 各一次的断言；保留 503、500 毫秒退避、Merged、补充原话、单条回复及同轮历史断言。相同旧 `52c2e0f3` / Rust 1.89 release 程序的目标用例在 1.191 秒通过；这仍不替代新头 CI。QQ 后续压缩流、浏览器领域实践和离线 Cargo 完整恢复尚未执行，新头须完整复验；旧作业未取消。

上一头 `c31213a5` 对应 [MSRV 作业](https://github.com/ZenthXSin/Eve.aic/actions/runs/42173404493/job/127746231737) 实际检出 `f680458e4c34772afd555dc1af6b89d8063a0302`，基线为 `f3809617852513bceea0a4b58712562d3aad91fe`。真实 rustc/cargo 1.95.0、全目标检查和七包 Clippy 通过，274 项成功、2 个忽略入口；其中 Codex/OpenAI 本轮新增 12 项解析回归均通过。OpenAI 单包反向特性树明确为 `default/raw_value/std`。同头 [Stable Linux 作业](https://github.com/ZenthXSin/Eve.aic/actions/runs/42173404493/job/127746232058) 的全工作区测试原日志含 412 条结果行，合计 2491 成功、0 失败、24 忽略；fmt、全目标检查和工作区 Clippy 成功，反向特性树明确包含 `arbitrary_precision/default/raw_value/std`，原九项数字解析失败已经消除。

Stable Linux 作业最终仍失败：后续 `context_compaction_test.py` 三项成功，`compaction_stream_test.py` 六项中两个负向场景失败。原日志显示已发出失败回复，报告 15 次工具调用和 15 条回执，夹具却等待 35 次；桥接器报 `missing_reply_fragment`，最终呈现为 30 秒等待超时。大工具目录叠加重复大文件结果，导致第二批之后已达 150k。修正仅调整这组六用例的负载：前 1+14 次读取短文件，最后 20 次读取大文件，摘要请求必须在第三批之后；35 条完整且唯一的工具回执、主对话 3/4 次、一次摘要纠错、平台 Sent、生产 150k/100k 与原超时全部保留，并记录真实压缩前后估算。

该夹具已用既有 `52c2e0f35911ea90193d9f9763ccaeb3611fb0cf`、Rust 1.89 release 产物完成 6/6 隔离进程验收（二进制 SHA-256 `55dc6a5dde12469a4d5ec3d5cd008d61d10810c4f6925e1a619df274ea375b38`）；前 15 条完整回执之后未发生摘要，四个成功场景的真实估算均由 522441 缩小至 87457，两个失败场景均保留全部 35 条回执。这个结果只证明夹具兼容性，不代替本 PR 新提交、Rust 1.95、Codex 或 `arbitrary_precision` 组合的远端复验。

同头 [QQ/Web 作业](https://github.com/ZenthXSin/Eve.aic/actions/runs/42173404447/job/127746231320) 的 Web 两项（26.822 秒）、QQ 两项（1.875 秒）均实际通过；官方小日志 Artifact `13483415244` 的压缩摘要和三个日志摘要已核验。热更新两项中同版本重启通过，完整候选切换仍在 35 秒内未提交。程序实测 922,293,416 字节；初始宿主桥 4.336 秒进入 starting，内层监督器 7.161 秒进入 starting，14.382 秒出现 `candidate_not_ready` 并被外层恢复。第二次 24.971 秒运行、30.763 秒交出旧监督器，候选尚未完成就超时。

原 QQ 长回归已于 2026-11-06 07:00:08 UTC 自然结束，完整日志再次复现同两个压缩流负向场景：期望 35 次、实际 15 次工具，之前各脚本均通过；后续浏览器领域实践和离线 Cargo 完整恢复步骤因此未执行。原作业没有被取消。这两个失败由上述统一负载修正覆盖，后续全部检查仍须按新提交重新验收。

该旧轮的期限分析：宿主桥放行 bootstrap 后启动外层 10 秒计时，bootstrap 完整校验与监督器初始化发生在其内；内层监督器在创建 Eve 子进程后才开始自己的 10 秒等待。外层停止可使内层记录 `candidate_not_ready`，因此这条记录不证明 Eve 自身用满 10 秒。就绪确认还需整文件摘要与稳定窗口后的完整校验。阶段证据显示总窗口被校验/启动和一次恢复消耗，尚无分项 CPU/I/O 计时，不能只凭文件大小确定每一秒的原因。QQ 工作流随后与 Stable/MSRV 统一 `CARGO_PROFILE_DEV_DEBUG=0`、`CARGO_PROFILE_TEST_DEBUG=0`、`CARGO_INCREMENTAL=0`；保留全部校验、35/10 秒期限和回退/零重放断言。上述 `d2aa3f9a` 的实际结果表明减小产物还不足以完成候选交接，需继续验证显式隔离组合。

以下 `d2baee7b` 指 PR 分支头；三份作业原日志一致记录实际构建的是 GitHub 合并检验提交 `0608ecfa687a5a46fec73de90ce5e7b7b2e38cb3`，由该头与当时基线 `805f063e3ee175e00b5a7a07cc19a73e8ebc6761` 组成。结果归属于这个准确组合，不外推到后来推进的基线或最终生产组合。

提交 `d2baee7b` 的 [Rust 1.95 原作业](https://github.com/ZenthXSin/Eve.aic/actions/runs/42165999523/job/127722662155) 已实际通过，共 262 项成功、2 个忽略入口：公共 API 27 个，独立适配器 23 个（另有 1 个忽略的恢复子进程入口），原 OpenAI 各目标合计 95 个（另有 1 个忽略入口），宿主模型 21 个（包含新增 8 个 Codex 组合），模型命令 6 个（包含新增 4 个适配器入口），Runtime 七目标合计 90 个。指纹用例捕获实际请求核对摘要，并验证请求序号、服务端 token 和凭据不改变 body 指纹。组合用例含真实配置插件、普通会话工具往返、主辅各三种实现/协议的九种有效组合、独立主/辅服务、识图与目录实际入口；九组合中主、辅请求均直接断言 high 和无 temperature。切换入口用例含权限、持久化重开和实际 CAS 冲突。

另外新增 4 个真实宿主进程用例：QQ 指令切 Codex 后由普通对话实际发送 Responses SSE，模型调用切换工具后当前轮仍走 Codex、下一轮走 Chat，重启后重复入站不重放配置写入；独立 eve-web 经 `/api/chat/send` 在两个方向调用真实工具切换，验证当前轮、下一轮、两个独立 Web 会话和两次重启。两种入口各自检查可信身份与伪造参数拒绝，逐条捕获 HTTP 请求核对工具定义、high、无 temperature、模型名、输出上限、稳定会话/缓存键和独立请求 ID。请求总数必须恰好等于正常模型/工具往返，不容许额外目录探测、模型审查或失败回退。这些夹具使用本地合成模型响应，不代表外部供应商兼容性或真实模型自主能力已验收。

`16d3200d` 的 [QQ/Web 原作业](https://github.com/ZenthXSin/Eve.aic/actions/runs/42163238228/job/127713612941) 已完成三个真实宿主二进制编译，原 WebChat 10 项完成（1 项浏览器用例跳过）。新 Web 身份用例通过；双向切换用例最后捕获到第九条无工具 Chat 请求，严格断言失败，后续 QQ 新用例因此未执行。该版日志未保留系统提示，不能事后把其具体来源确定为历史摘要。

`d2baee7b` 的 [QQ/Web 复验](https://github.com/ZenthXSin/Eve.aic/actions/runs/42165999519/job/127722507954) 已关闭夹具内默认四轮触发的后台历史摘要，并将两个适配器脚本拆成独立步骤。QQ 两项实际通过；Web 身份用例通过，双向用例重启后的第八条请求超出当时应有的七条。原日志中的系统提示准确对应 `training-plugin` 的后台交流学习器。现只为两个合成 Web 会话写入训练插件已有的持久暂停模式，并在退出时核验模式保持暂停；历史摘要与交流学习的生产默认和专门回归不变。逐条请求、工具、身份和参数断言保留，不跳过无工具请求。该隔离修正已由上述 `c31213a5` 的两项真实 Web 进程验收确认。

同一 QQ 作业的原热更新用例在 35 秒内未观察到完整交接提交：候选监督器已发布新候选和源码修订，状态仍为 `starting`，第二个 Eve 尚未发布就绪标记。已有证据不足以区分完整校验耗时和子进程启动阻塞。本次把该脚本移到独立步骤并保存小日志，补充宿主、监督器、交接阶段和阶段变化耗时；只记录选定状态字段与二进制大小/摘要，不输出命令、凭据或会话正文。候选夹具改为流式哈希和文件复制，避免整个调试二进制长期保留在测试进程内存。原 35 秒总期限、10 秒就绪期限、平台投递和新进程去重断言全部保留，尚不把夹具内存优化当作失败根因已修复。

`d2baee7b` 的 [Stable Linux 全工作区作业](https://github.com/ZenthXSin/Eve.aic/actions/runs/42165999523/job/127722661944) 通过格式、all-targets 构建检查及完整 Clippy，但全工作区测试有三个目标失败：OpenAI 会话 embedding 返回 `InvalidOutput`，语义存储单元与分段归档返回 `CorruptState`，合计 9 项失败。新增依赖使全工作区的 `serde_json/arbitrary_precision` 启用，原严格 JSON visitor 将内部数字表示当成普通对象；单包定向通过不能代表这一特性组合。26 处严格 JSON 入口现改用公开 `RawValue` 借用原文，按真实容器/标量 token 解码，共享原有 127 层容器深度预算。工作区显式开启 `raw_value`，保留 `arbitrary_precision`、准确锁与 vendor；对象逐层拒绝重复字段（含转义键），真实私有同名字段在严格 JSON 入口返回的 `Value` 中保持对象，数字按当前 `serde_json` 特性保留精度。持久格式不变，原有合法分段直接恢复，不转换或清空损坏状态；新增回归覆盖数字类型/精度、同名对象、重复键、尾随输入和深度，并验证脚本冷回执重启和任务策略目录中的数字。上述同名字段保证仅覆盖严格入口；下游再次用官方 `serde_json::from_value` 解码含 `Value` 的结构时，仍受上游保留字段语义影响，本轮没有扩大为通用类型往返改造。单包及全工作区动态结果仍待精确提交的 CI。

CI 已按实际 1.95 版本拆分：最低版本任务运行定向 Rust 回归与受影响包的严格 Clippy，QQBot 通道任务编译宿主后分别执行 Web、QQ 新增适配器与热更新用例；其中一组失败仍会执行其他组并保存失败结果。三份小日志在原长回归开始前上传，原 QQ 回归命令全部保留，Stable 的 Linux 任务继续完整回归。新增适配器进程验收仅在 Linux 执行；Windows 支持已按用户要求停止，原仓库共享矩阵的移除另行处理。最低版本任务在依赖构建前校验原文件，关闭调试信息和增量编译，限时 45 分钟。

最小动态验收命令（先完成候选锁文件解析，再由真实 Rust 1.95 验证）：

```bash
python3 scripts/verify-codex-upstream.py
cargo +1.95.0 check --workspace --all-targets --locked
cargo +1.95.0 fmt -- --check
cargo +1.95.0 test -p eve-llm-api --lib --locked
cargo +1.95.0 test -p eve-llm-codex --lib --test responses --locked
cargo +1.95.0 test -p eve-app --lib models::tests --locked
cargo +1.95.0 test -p eve-app --lib qq_model --locked
cargo +1.95.0 test -p eve-llm-openai --locked
cargo +1.95.0 test -p eve-runtime --test llm_host --test session_host --test session_http --test model_resolver --test context_compaction --test openai_provider --test streaming --locked
cargo +1.95.0 clippy -p eve-llm-api -p eve-llm-openai -p eve-llm-codex -p eve-runtime -p eve-app -p eve-developer-tools -p eve-web-panel-api --all-targets --locked -- -D warnings
cargo +1.95.0 build -p eve-app --bin eve-qqbot --bin eve-web --locked
EVE_QQBOT_BINARY=target/debug/eve-qqbot python3 connectors/qqbot/test/model_adapter_test.py
EVE_WEB_BINARY=target/debug/eve-web python3 scripts/test-web-model-adapter.py
```

随后运行现有工作区测试、示例、QQ 模型切换/模型健康/工具目录/识图实际进程及 Web 宿主回归。已通过项仅按上文的准确提交和作业记录归属，不能把 Rust 定向结果替代后续进程和完整回归结果。

## 工具链与原文件保留

原 Codex workspace 声明 Rust 1.95。Eve 顶层及适配器统一最低版本为 1.95，CI 的 MSRV、各实际进程工作流、发行构建和便携包元数据同步；不修改 vendor manifest 降版本，不使用可选 feature 回避全工作区编译。过去的 1.89 成功记录保持历史版本，不冒充 1.95 结果。

格式检查在 Eve workspace 根目录执行 `cargo fmt -- --check`。`--all` 会递归 path dependencies，进入应保持原文的 Codex vendor；根目录默认模式依据 `metadata --no-deps` 检查 Eve 的全部 workspace targets。真实 1.95 metadata 已确认 Eve 的 106 个成员、330 个目标，vendor 不在 workspace_members 内；`16d3200d` 的 QQ 作业已实际通过 `cargo +1.95.0 fmt -- --check`。

原包来源检查覆盖 577 文件、6171502 字节、24 个本地 package，Apache-2.0 与 NOTICE 保留。上游原 prompt 的一处行尾空白也忠实保留。这里使用的是 Codex 原 Responses 请求、HTTP、重试和 SSE 代码，不等于整个 Codex core：OAuth 刷新、WebSocket continuation、加密 reasoning 的持久历史、原生非函数工具和全部多模态输出不在当前 Eve 契约范围内。
