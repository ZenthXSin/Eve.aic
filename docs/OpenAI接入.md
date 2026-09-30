# OpenAI Responses 接入

已实现 `eve-llm-openai`，依赖 `eve-llm-api` 的公开契约，通过 `reqwest 0.12.28` 发送非流式 Responses HTTP 请求。Kernel 和业务插件不依赖厂商格式或 HTTP 客户端。当前完成转换、本地 HTTP、Runtime 工具循环和取消验收；**未使用真实 Key，真实模型 smoke test 仍待执行**。

## 宿主装配

```rust
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};

// model 与 credential 由宿主装配，不进入 ModelRequest。
let provider = OpenAiProvider::new(OpenAiConfig::new(model), credential)?;
```

`OpenAiConfig::new` 要求显式选择模型，默认 URL 为 `https://api.openai.com/v1/responses`，传输期限 60 秒、最大响应 8 MiB。配置字段公开，宿主可替换；期限和上限必须为正。Provider 不读取文件或环境，不进行配置来源合并或静默回退。

可显式指定完整 HTTPS Responses URL；HTTP 仅允许 loopback 的 IP 或 `localhost`。拒绝 URL 用户信息、查询和片段；禁止 HTTP 重定向与所有客户端自动重试。本地服务器不经代理，生产 HTTPS 沿用客户端代理配置。自定义 URL 是宿主作出的目的地选择，凭据会发送给该目的地。

凭据独立传入构造函数，保存在 sensitive HeaderValue；Provider 不实现 Debug，也不将原始 HTTP 错误、URL、响应体或解析错误内容返回给宿主。此实现仍在进程内持有明文凭据，不是密钥库或凭据代理；#32 的密钥引用、2FA、插件信任与 #39 的配置快照接线继续独立推进。环境变量仅供下述手动示例自举，不能视为完整配置管理。

## 支持的子集

- 请求：有序 system/user/assistant 文本；assistant 函数调用批次与恰好配对的结果；名称排序后的函数定义。适配器仅接受 1–64 字节的 ASCII 字母/数字/下划线/连字符函数名，不通过改名修正。
- 映射：`ToolCall.id` 保存 `call_id`，而非输出 item 的 `id`；对象参数序列化为 arguments 字符串。成功值序列化为 JSON 字符串；失败为 `{"error":{"code":"InvalidArguments","message":"…"}}` 形式。批次调用与结果顺序保持不变。
- 每次发送完整上下文；设置 `parallel_tool_calls: true`、`strict: false`、`store: false`、`stream: false`，不使用 `previous_response_id`。不补写或删除插件 Schema 的 required 字段，参数仍由工具本地校验。
- 回复：单个 assistant 消息中的纯 output_text，或仅包含 function_call 的完整批次。同消息文本分块按序拼接；批次完整解析并校验后才返回宿主，不提前执行部分调用。
- reasoning、内置工具、混合文本/函数调用、refusal、非空 annotations、消息 phase、函数 namespace 和多个文本消息返回 `Unsupported`。这些信息不能被当前 Eve 协议保留；后续需扩展适配器私有续传状态或通用契约，当前不会静默丢弃。

不预设固定模型名或保证所有 OpenAI 模型兼容。模型返回上述不支持的内容时会明确失败；支持 reasoning 续传与流式输出另行交付。`store: false` 只控制响应状态保存，不代表服务端零保留。

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

可选 `EVE_OPENAI_RESPONSES_URL` 指定完整端点。示例先完成一次文本请求，再完成两次请求的工具循环；若没有真正执行一次成功工具调用，则验收失败。模型/网络/协议失败后也停止已启动插件。示例会显示模型回复，避免使用敏感测试文本。真实验收仍未执行；下一步记录实际兼容模型及文本/工具 smoke test 结果，再推进会话恢复与完整凭据代理。

2026-09-30 复核的官方资料：

- [Function calling](https://developers.openai.com/api/docs/guides/function-calling)：函数定义、call_id、结果回传及 reasoning 续传要求。
- [Responses 迁移](https://developers.openai.com/api/docs/guides/migrate-to-responses)：输入/输出项、strict 与状态控制。
- [数据控制](https://developers.openai.com/api/docs/guides/your-data)：store 与其他保留控制的区别。
