# 流式输出协议

对应 #45。定义层在 eve-llm-api，厂商 SSE 对象只存在于 eve-llm-openai；内置宿主负责工具循环，通道插件实现异步事件接收器。

## 状态与顺序

每个接收器绑定一次 run_turn 调用；会话模式所有事件带持久化 turn_id，普通模式为 None（调用方负责把接收器绑定自己的请求标识）。请求编号从 1 开始。

ProviderStarted → TextDelta* → ResponseCompleted。函数调用只在正常终态验证整个输出后返回，不发布参数片段；宿主先完整预检再运行工具。ToolBatchStarted（收到完整调用批次，尚不代表已执行）→ ToolResult（整批结果收集后按调用原顺序）→ 第二个 ProviderStarted → TextDelta* → ResponseCompleted → TurnCompleted。无工具的请求直接 TurnCompleted。

TextDelta 是暂时显示；ResponseCompleted 是某次模型请求结束；TurnCompleted 是完整轮次已生成；SessionSaved 只在会话完整 transcript 成功持久化后发送。失败发送 Failed（尽力投递）；begin 失败的事件无轮次 ID，且不发模型请求。调用方直接丢弃 Future 时无法承诺额外发送失败事件，不会发布 SessionSaved。保存失败保留最终输出和原 Pending。保存成功但最后通知投递失败单独返回 Delivery，携带完整输出，不可重试本轮。

## 背压与取消

公开接收器是返回 Future 的异步 trait。宿主逐次 await，不建无限队列；插件可桥接有界通道，接收端断开时返回 Cancelled，并实现 closed() 断开信号，使正在等待网络或工具时也能取消；默认 closed 永久等待，只有 emit 报错不能保证在下一事件前发现断开。closed Future 必须可安全丢弃和重新订阅。每次投递有期限，超时/异常终止生成和工具，保留已有副作用诊断。丢弃 run_turn Future 会丢弃网络读取并取消工具任务；生命周期准入锁持续覆盖工具 Future 析构及会话提交。

回调不得执行阻塞 I/O、重入同 Kernel 生命周期操作或同步等待当前轮次；队列容量和字节限制由通道插件明确设置。SSE 总字节预算默认 8 MiB（含协议帧），单帧不超过该预算，宿主拼接的文本另有默认 8 MiB 上限；不自动重连或重试。请求期限包括读取和消费者背压。

## 配置与兼容

普通配置插件 runtime.llm.response_mode 选择 complete / stream，默认 complete；环境变量 EVE_LLM_RESPONSE_MODE，文件覆盖环境，类型化消费者拒绝未知模式。LlmHostConfig.response_mode 使用公开 ResponseMode 枚举，每次构造宿主固定配置。Provider 默认 stream 返回 Unsupported；不静默降级。两种模式共享完整输出验证、固定提示前缀、工具排序、历史及会话提交，增量不重建提示、不单独写历史。

## Responses 支持边界

仅 assistant output_text 与 function_call。验证 SSE 事件类型/名称、序列（存在时）、response/item 标识、输出顺序、增量与 done/最终 output 一致。拒绝终态前重复/矛盾事件和与最终 output 不一致的片段，以及混合文本和工具、reasoning、refusal、annotations、namespace 和未知事件。UTF-8 在完整行组装后解码，支持 LF/CRLF/CR、注释、多行 data。未收到 completed 的 EOF、坏 JSON 或超时均失败。首个有效 completed 后立即关闭本地流，不等 EOF；后续连接数据不会触发二次执行。

官方参考（2026-09-30 复核）：
- https://developers.openai.com/api/docs/guides/streaming-responses
- https://developers.openai.com/api/docs/guides/function-calling
- https://developers.openai.com/api/reference/resources/responses/streaming-events

## 验收与边界

已加入 Mock、本地 HTTP、慢消费者/断开、提交失败、跨进程恢复及 Windows/Ubuntu CI 验收。真实 API 独立验收，凭据只在宿主环境。#29 的消息分类与 Jev 路由后续交付；显式取消和旧回复过滤见[任务控制](./任务控制.md)。本地取消不保证远端停止或副作用回滚。

## 调用与验收

Provider 的 stream(request, ModelTextSink) 返回与 complete 相同的完整 ModelResponse；默认实现 Unsupported。宿主 run_turn_with_events(input, TurnEventSink) 按配置选择请求方式；既有 run_turn 保留完整返回值，流式模式仍验证拼接文本，但不转发增量给外部。

事件投递默认期限 5 秒；超时按 Cancelled 终止。Provider 期限涵盖整次模型请求及其文本消费者等待。ToolResult 通知失败保留已收集的整个工具结果与开始次数。失败通知尽力投递，投递失败不会覆盖原执行错误。消费者 closed 信号就绪时，宿主丢弃模型读取、取消并等待剩余工具 Future 析构后记录稳定的开始数和结果，再写入失败状态。SessionSaved 通知成功仅表示接收器接受通知，不保证远端通道已送达；接收器返回投递失败时 SessionRunError::Delivery 保留已提交结果，不能自动重跑。

```sh
cargo run -p eve-runtime --example streaming_offline
cargo run -p eve-runtime --example session_recovery -- /tmp/eve-stream-state --stream
cargo run -p eve-runtime --example session_recovery -- /tmp/eve-stream-state --stream
cargo run -p eve-runtime --example session_recovery -- /tmp/eve-stream-state --cancel-stream
cargo run -p eve-runtime --example session_recovery -- /tmp/eve-stream-state --stream
```

四次启动分别生成工具回执、恢复历史、取消增量、恢复完成历史；轮次 1/2/3/4，历史消息 0/4/6/6，工具次数 1/0/0/0。第三轮保存 Failed/Cancelled，第四轮请求不带其失败输入。旧的 --leave-pending 验收仍覆盖 Pending → Interrupted。

真实 Provider 的示例通过配置插件选择模式，流式文本及工具进度异步写入 stderr，完整验收 JSON 写入 stdout：

```sh
EVE_LLM_RESPONSE_MODE=stream cargo run -p eve-runtime --example openai_responses -- text
EVE_LLM_RESPONSE_MODE=stream cargo run -p eve-runtime --example openai_responses -- tool
```

宿主另外提供 EVE_OPENAI_API_KEY、模型及可选 endpoint。文件中既有 response_mode 优先于环境。本切片没有重新调用真实 API；本地 SSE 通过不等于特定代理或模型兼容性已验证。

## Grok / Chat Completions 的模型流式接收

宿主启动时设置 `EVE_OPENAI_STREAM_COMPLETIONS=true`（也接受 `1`）可让主模型及其用途通过 SSE 接收，`complete` / `complete_json` 仍返回完整响应。默认关闭；`false` / `0` 关闭，其他值报配置错误。该开关独立于 `runtime.llm.response_mode`，QQ 保持 `complete`：完整回复保存成功后才投递，沿用原有回执、防重复和上下文压缩机制。旧版本忽略此环境变量，可继续使用完整 JSON 响应，无需回退持久化配置。

Chat SSE 支持 UTF-8 分片、文本、按 index 交错的函数名称与参数分片，以及 usage 附加帧。内部 reasoning 字段丢弃，工具说明草稿不投递。完整 `stop` / `tool_calls` 终态才交给原有响应校验；参数 JSON 重复键、调用 ID 冲突、截断、过滤、错误帧、提前 `[DONE]` / EOF 均失败。正常终态立即返回，无需等待连接关闭。为避免把工具前说明误作文本，Chat 的公开 `stream` 文本接收器也只在正常文本终态收到合并后的文本；Responses 保留逐片文本事件。

总响应字节预算和请求总期限仍生效，流式接收不会无限延长期限。对话完整响应可沿用原有最多三次有界瞬时错误重试，全部发生在返回模型结果之前，不执行未完成的工具调用；显式 `stream` 接收器不自动重试。流式接收只能避免等待整个 JSON 响应，并不能保证上游模型一定及时响应。
