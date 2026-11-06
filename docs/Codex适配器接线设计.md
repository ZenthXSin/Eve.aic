# Codex 适配器接线设计

本文描述待实现的宿主切换接线，当前只完成独立适配器与静态检查，未运行 Cargo、切换生产或向真实 Codex 端点请求。基础会话 API 来自 `a2eb9e52c135ae03e36233541242f6fe287a8a91`，原适配器草稿提交为 `78f2b526404a9d0dc3c939a428e51abfa642542d`。

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

`CoreModelResolver::capture` 已在同一配置修订下捕获 provider、profile、用途、输出上限、流式与重试设置。新增实现选择必须进入同一份捕获和缓存比较。现有 `ModelProfile.provider` 已在比较内；旧 profile 与缺省值无需迁移，`openai + chat` 的有效请求保持不变。

组合层增加仅内部使用的选择类型，承载 `OpenAiConfig` 或 `CodexConfig`，对外只返回 `Arc<dyn LlmProvider>`。配置校验、状态展示和实际构造必须调用同一解析过程。不能仅放宽字符串校验后让识图或其他旁路仍构造 OpenAI Provider。

| 配置或能力 | Codex 对应行为 |
| --- | --- |
| `profile.model` | 原 `ResponsesApiRequest.model`，不由名字推断实现 |
| 当前有效 `base_url` | 与现有拼接规则一致，根地址补 `/v1/responses`，已有版本路径追加 `/responses`，完整 Responses 地址保留 |
| `profile.timeout_ms` | provider 单请求期限；用途 `timeout_cap` 继续缩小它 |
| 用途输出上限 | 非零用途值覆盖 profile；用途为零继承 profile；最终无值就省略 `max_output_tokens` |
| `reasoning_effort=omit` | 不发送 reasoning；其他值必须可由原 Codex `ReasoningEffort` 表示 |
| temperature | 永不发送 |
| 主/辅凭据 | 原 `CredentialBank` 按用途和引用产生租约，再交给适配器构造；不复制凭据到配置或诊断 |
| `EVE_OPENAI_TRANSIENT_RETRIES` | 显式启用原 Codex 有界 HTTP 重试；整个请求共用总期限；应用完成/协议失败不自动换实现重试 |
| `EVE_OPENAI_STREAM_COMPLETIONS` | OpenAI 分支维持原行为；Codex 原客户端始终 SSE，关闭此项不会让它伪装成非流式 JSON HTTP |
| 首包/停顿/总期限 | 现有用途限制保留，转换成 Codex 同名时间策略；不重置等待或重试的总截止 |
| 文本 sink | Codex 原流完整验证后一次交最终正文；暂不提供逐字预览；工具批次不混入 commentary |
| usage/cache | 读取原 parser 的 usage；缺失保持未知，真实零仍为零，不把本地前缀相似度当供应商缓存命中 |

Codex 构造器拒绝携带凭据的非本地明文 HTTP，并拒绝 URL 用户信息、查询串和片段。该边界不需要改原源码，也不能通过创建匿名 Provider 再另附主凭据绕过。

## 期限与共享包装器

外层顺序继续为：角色准入 → 宿主总准入 → 自然对话改写 → 前缀观测 → 已捕获的 Provider。先验证候选 Provider 与凭据，再更新共享准入；无效配置不能改变在途额度。每个角色和用途继续使用原健康记录分类，不因新实现多领请求槽。

Codex 即使通过 `complete` 调用也使用原 SSE，因此 `provider_timeout` 的收尾余量不能只由 `streamed_completions` 布尔值判断。应按真实 transport 是否 SSE 计算，与 OpenAI SSE 同样保留收尾余量；用途外层硬上限仍生效，不能扩大 speech 60 秒等业务期限。若硬上限不留足额余量，明确保留取消/失败语义，不假称已完整响应。

## 会话、恢复与切回

宿主提供的 `ModelSession` 是唯一身份来源，主对话、后台任务、审查、发言和识图分别沿现有隔离键。适配器不从 prompt、模型回复或配置中的姓名推断身份，不创建全局 cache key。

`request_fingerprint` 同步只读编码实际 Responses body 和规范端点，使用独立 domain `codex-responses-wire-v1`。它不访问网络，不取得轮次状态锁，不读/写服务端 token，也不包含凭据或 `x-client-request-id`。`complete` 和 `stream` 的实际 body 相同，故指纹相同；JSON 专用请求不属于 Runtime 的 `ResponseMode`。

HTTP 重试复用当前 request ID；进程恢复时仅相同指纹可复用检查点中的待请求序号。输入、工具、配置、端点或适配器改变，指纹随之改变，宿主分配新序号并保留已完成工具，不重跑整轮。服务端 `x-codex-turn-state` 只在同一 Provider 内按会话和轮次保存，切换或重启后重新取得，不借用别的会话。

切入 Codex 时，在一个配置事务中同时设置角色 provider 和有效 Responses 协议/地址；切回时同样一次恢复该角色的 `openai` 和原 Chat 协议/地址。正在执行的一轮沿用已捕获 Provider，新一轮按新配置解析。不得自动取消旧轮、清空会话或强制重启来完成切换。两个适配器长期 ID来自相同宿主身份，wire fingerprint 的 domain 不同；工具去重与已发送消息的恢复都继续由 Eve 原账本负责。

## 直接构造旁路

`qq_files::DescribeImageTool` 目前直接调用 `OpenAiProvider::describe_image_with_session`，不经过核心 resolver。切换实现时必须将图片转成普通 `ModelRequest` 后交给共同构造入口，或明确返回“此入口尚不支持所选适配器”，不能悄悄仍走 OpenAI。Codex mapping 已支持内嵌图片输入，输出只接收可表达为 Eve 文本契约的结果。模型目录查询是另外的能力，不应为列候选暗中构造旧适配器执行推理。

`idle_small` 是已有独立匿名 Chat 线索筛选器，保留其独立设置与说明；其提示改写包装器无法同步预测额外模型输出，因此保持 fingerprint=None，让恢复使用新请求序号。Jev、语义 embedding、模型目录等也各自沿公共契约，不宣称全部已经换成 Codex。

## 接线后的验收

需要真实运行后才可标记完成：原 Chat 和原 Responses 回归；主/辅每个可用组合；Codex+Chat 配置保存/请求前拒绝；原凭据拒绝与匿名边界；分用途输出上限；旧配置加载；无探测的状态查询展示 adapter；原 HTTP/SSE 完整批次；缓存未知/零；取消和重试期限；并发准入；相同持久请求恢复、输入变化和切回后新请求序号；主工具零重放；识图旁路的实际实现一致。

独立适配器已有 23 个验收用例和 1 个忽略的恢复子进程入口，均尚未执行。新增指纹用例通过本地捕获的实际请求核对摘要，并验证请求序号、服务端 token 和凭据不改变 body 指纹；不是单纯重复实现计算结果。

原包来源检查覆盖 577 文件、6171502 字节、24 个本地 package，Apache-2.0 与 NOTICE 保留。上游原 prompt 的一处行尾空白也忠实保留。这里使用的是 Codex 原 Responses 请求、HTTP、重试和 SSE 代码，不等于整个 Codex core：OAuth 刷新、WebSocket continuation、加密 reasoning 的持久历史、原生非函数工具和全部多模态输出不在当前 Eve 契约范围内。
