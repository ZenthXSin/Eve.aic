# OpenAI Responses 接入

已实现 `eve-llm-openai`，依赖 `eve-llm-api` 的公开契约，通过 `reqwest 0.12.28` 发送 Responses HTTP 或 SSE 请求，默认非流式。Kernel 和业务插件不依赖厂商格式或 HTTP 客户端。当前完成转换、本地 HTTP、Runtime 工具循环和取消验收；真实模型验证见下方集成验收记录。

## 宿主装配

```rust
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};

// model 与 credential 由宿主装配，不进入 ModelRequest。
let provider = OpenAiProvider::new(OpenAiConfig::new(model), credential)?;
```

`OpenAiConfig::new` 要求显式选择模型，默认 URL 为 `https://api.openai.com/v1/responses`，传输期限 60 秒、最大响应 8 MiB。配置字段公开，宿主可替换；期限和上限必须为正。Provider 不读取文件或环境，不进行配置来源合并或静默回退。

可显式指定完整 HTTPS Responses URL；HTTP 仅允许 loopback 的 IP 或 `localhost`。拒绝 URL 用户信息、查询和片段；禁止 HTTP 重定向与所有客户端自动重试。本地服务器不经代理，生产 HTTPS 沿用客户端代理配置。自定义 URL 是宿主作出的目的地选择，凭据会发送给该目的地。

凭据独立传入构造函数，保存在 sensitive HeaderValue；Provider 不实现 Debug，也不将原始 HTTP 错误、URL、响应体或解析错误内容返回给宿主。此实现仍在进程内持有明文凭据，不是密钥库或凭据代理；#32 的密钥引用、2FA、插件信任继续独立推进。下方 openai_responses 示例已接入 #39 的普通配置快照；旧 openai_tool_loop 示例仍以环境变量显式自举。

## 支持的子集

- 请求：有序 system/user/assistant 文本；assistant 函数调用批次与恰好配对的结果；名称排序后的函数定义。适配器仅接受 1–64 字节的 ASCII 字母/数字/下划线/连字符函数名，不通过改名修正。
- 映射：`ToolCall.id` 保存 `call_id`，而非输出 item 的 `id`；对象参数序列化为 arguments 字符串。成功值序列化为 JSON 字符串；失败为 `{"error":{"code":"InvalidArguments","message":"…"}}` 形式。批次调用与结果顺序保持不变。
- 每次发送完整上下文；设置 `parallel_tool_calls: true`、`strict: false`、`store: false`，`stream` 由宿主模式决定（默认 false），不使用 `previous_response_id`。不补写或删除插件 Schema 的 required 字段，参数仍由工具本地校验。
- 回复：单个 assistant 消息中的纯 output_text，或仅包含 function_call 的完整批次。同消息文本分块按序拼接；批次完整解析并校验后才返回宿主，不提前执行部分调用。
- 消息的 `phase: final_answer` 映射为 Eve 的 `Final`，assistant 文本历史以 final_answer 回传；system/user 不添加 phase。缺失或 null phase 保持兼容；commentary 和未知 phase 明确拒绝。
- reasoning、内置工具、混合文本/函数调用、refusal、非空 annotations、函数 namespace 和多个文本消息返回 `Unsupported`。这些信息不能被当前 Eve 协议保留；后续需扩展适配器私有续传状态或通用契约，当前不会静默丢弃。

不预设固定模型名或保证所有 OpenAI 模型兼容。模型返回上述不支持的内容时会明确失败；reasoning 续传另行交付；流式文本与函数子集见流式输出协议。`store: false` 只控制响应状态保存，不代表服务端零保留。

## 错误、期限与取消

| 情况 | 结果 |
| --- | --- |
| 无效模型/URL/凭据/期限/上限 | `Configuration`，不发请求 |
| Eve 消息非法、空回复、坏 JSON、重复 JSON 键、非对象参数、空或重复调用 ID | `Protocol` |
| HTTP 非 2xx（含重定向）、响应未 completed、输出项未完成、网络失败、响应超过上限 | `Provider`，只保存安全类别或 HTTP 状态码 |
| HTTP 客户端传输期限届满 | `ProviderTimeout` |
| 当前协议不能完整映射的输出或函数名 | `Unsupported` |

按 Content-Length 和实际 chunk 字节双重限制响应大小。传输期限包含读取响应体；`LlmHost.provider_timeout` 仍可更早终止整轮。`complete` 的同步部分不启动 HTTP 请求；Future 被丢弃后本地等待停止，Kernel 运行准入锁可释放。以上不保证远端停止、停止计费或撤销工具副作用，不保存会话状态或自动重试。

## 验收

不需要真实凭据或外网：

```bash
cargo test -p eve-llm-openai --locked
cargo test -p eve-runtime --test openai_provider --locked
cargo run -p eve-runtime --example openai_offline --locked
```

离线示例和真实示例共用验收流程，验证直接文本和一个插件 echo 调用。本地 HTTP/Runtime 测试另覆盖批次中的成功/失败结果、完整上下文前缀、HTTP 错误与截断、超时、取消、重定向拒绝、响应上限、无效请求不发 HTTP、诊断脱敏、不支持输出不执行任何工具。

手动真实验收：在本机宿主环境设置 `OPENAI_MODEL` 和 `OPENAI_API_KEY` 后运行：

```bash
cargo run -p eve-runtime --example openai_tool_loop --locked
```

可选 `EVE_OPENAI_RESPONSES_URL` 指定完整端点。示例先完成一次文本请求，再完成两次请求的工具循环；若没有真正执行一次成功工具调用，则验收失败。模型/网络/协议失败后也停止已启动插件。示例会显示模型回复，避免使用敏感测试文本。该原始示例保留显式环境自举；通过配置插件运行的真实回执验收使用下述 openai_responses 示例。

2026-09-30 复核的官方资料：

- [Function calling](https://developers.openai.com/api/docs/guides/function-calling)：函数定义、call_id、结果回传及 reasoning 续传要求。
- [Responses 迁移](https://developers.openai.com/api/docs/guides/migrate-to-responses)：输入/输出项、strict 与状态控制。
- [数据控制](https://developers.openai.com/api/docs/guides/your-data)：store 与其他保留控制的区别。
- [Reasoning 与 phase](https://developers.openai.com/api/docs/guides/reasoning)：commentary/final_answer 的含义及 assistant 历史回传要求。

## 配置插件与真实回执验收

TLS 使用 rustls 与系统证书库，兼容系统已信任的代理或私有 CA，并保留证书和主机名校验。`OpenAiConfig::with_base_url` 支持根地址、版本路径或完整 Responses URL；`reasoning_effort` 与 `max_output_tokens` 默认为 None，保持原请求参数。显式设置 none 只用于支持该参数的模型，仍拒绝返回 reasoning 项。
`openai_responses` 提供 `text` 和 `tool` 两种模式。它通过内置配置插件读取 `provider.openai` 和 `runtime.llm`，再构造本轮 Provider/Host；配置缺失或非法时失败，没有硬编码回退。示例插件声明对 `eve.config` 的依赖，配置先启动。

| 输入 | 用途 | 默认值 |
| --- | --- | --- |
| `EVE_OPENAI_BASE_URL` | API 地址 | `https://api.openai.com/v1` |
| `EVE_OPENAI_MODEL` | 模型名 | 必填 |
| `EVE_OPENAI_REASONING_EFFORT` | 可选 reasoning effort | 空，不发送 |
| `EVE_OPENAI_TIMEOUT_SECONDS` | 请求期限 | 120，范围 1–600 |
| `EVE_OPENAI_MAX_OUTPUT_TOKENS` | 最大输出 | 512，范围 1–16384 |
| `EVE_LLM_MAX_PARALLEL_TOOL_CALLS` | 工具并发上限 | 10，必须为正数 |
| `EVE_OPENAI_API_KEY` | 宿主运行凭据 | 必填，不进入配置 Schema |

非敏感配置沿用默认值 < 环境 < 文件的优先级。可在模式参数后传入独立配置目录，否则使用自动删除的临时目录。每轮装配时读取配置快照；运行中不自动重建 Provider，也没有新增配置文件监听器。

在宿主环境提供凭据与上述配置后执行：

```bash
cargo run -p eve-runtime --example openai_responses --locked -- text
cargo run -p eve-runtime --example openai_responses --locked -- tool
```

文本模式检查一轮回复包含 `EVE_TEXT_OK`。工具模式要求模型调用一次插件 echo；工具执行时在本地生成 receipt，模型第二次请求必须返回该 receipt。示例同时检查两次 Provider 请求、一次实际工具执行与成功结果，避免把“HTTP 连通”或模型自行生成工具结果算作闭环成功。成功输出只有模式、计数、耗时、并发上限和测试回复；错误不输出原始请求或响应正文。退出时停止所有插件并刷新日志。

CI 只使用回环 HTTP 夹具和确定性 Mock，不读取真实凭据。真实运行属于显式人工验收；本地丢弃请求不能保证远端停止处理或计费。

## 2026-09-30 真实模型集成记录

复用 PR #41 的 Provider，并组合 #39 的普通配置插件；在用户授权的兼容 Responses 服务上使用指定模型 `gpt-5.6-sol`。本次显式设置 `reasoning.effort: none`、`max_output_tokens: 512`，工具并发上限从配置插件读到 10。

| 场景 | 结果 | Provider 请求 | 实际工具执行 | 本轮耗时 |
| --- | --- | --- | --- | --- |
| 文本 | 回复 EVE_TEXT_OK | 1 | 0 | 7,439 ms |
| 插件工具往返 | 模型回复包含本地插件生成的 receipt | 2 | 1 | 22,972 ms |

工具结果只有执行后才产生 receipt；第二次模型请求成功返回同一值，计数与结果均由示例断言。发现并修复系统代理 CA 与 final_answer phase 的兼容问题；TLS 验证保持启用。最终代码通过工作区 174 项测试及文档测试、严格 Clippy、格式、Rust 1.89 所有目标与本地 HTTP 示例；其中适配器和 Runtime HTTP 相关测试共 24 项。

本次验证仅覆盖上述非流式、关闭 reasoning 的最小闭环，不证明 reasoning 续传、会话恢复、流式回复、消息打断或密钥引用已实现。凭据仅用于本地宿主环境，不进入配置文件、仓库、PR、Issue 或验收记录。


## 会话历史的离线验收

SessionLlmHost 已在文件后端重建后，用本地 Responses HTTP 服务器验收第二轮历史：保留旧 function_call / function_call_output、顺序、参数、成功回执和失败结果，assistant 文本携带 final_answer；旧工具执行次数为 0。该验证不使用 previous_response_id，也不代表已经完成真实 API 多轮兼容性验收。详见[会话恢复](./会话恢复.md)。

流式请求、严格 SSE 支持子集及事件消费详见[流式输出](./streaming.md)。2026-10-01 新增了核心入口非流式多轮及独立进程恢复的[真实验收记录](./主模型验收.md#2026-10-01-外部实测)：gpt-5.6-sol / none，三轮、一次 echo、恢复轮零工具和 revision 4→6 均通过。真实 SSE 仍待单独验收。
