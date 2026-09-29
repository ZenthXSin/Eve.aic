# 最小 LLM 工具调用契约

本文是 Issue #31 的实现前契约。它规定 Eve 的宿主、Provider 适配器和插件能力如何协作；不表示真实模型、会话恢复或流式交互已经实现。

## 分层

| 层 | 职责 | 不负责 |
| --- | --- | --- |
| `eve-llm-api` 定义层 | 厂商无关的消息、上下文、工具、Provider、错误契约 | 插件生命周期、网络传输、具体模型格式 |
| Kernel 实现层 | 插件状态、依赖预检、Scope、Service、权限检查与收尾 | 模型请求、工具循环、会话语义 |
| Runtime 组合层 | 选择 Provider、绑定插件 Service、执行工具循环、设置超时和轮数 | 工具的业务实现、Memory 算法 |
| Context、Memory、Tool、Channel 插件 | 提供上下文快照、工具定义与执行、输入输出能力 | 直接访问 Kernel 私有状态或厂商 API |

`eve-llm-api` 是独立 crate，可依赖 `plugin-api` 的身份与权限类型；依赖不可反向。Provider 适配器和使用 LLM 协议的插件依赖该定义 crate，Kernel 不依赖它。

## 内部协议

宿主和 `LlmProvider` 之间使用 Eve 内部协议。每个适配器自行映射模型原生结构化调用或文本标记；映射无法保留必需字段时，必须返回 `Unsupported`，不得丢弃调用 ID、工具名、参数或工具结果。

- `ChatMessage`：枚举表示 `system`、`user`、`assistant`、`tool` 四种角色。文本消息保存 `String`；assistant 工具调用保存完整、有序的 `Vec<ToolCall>`，tool 消息保存同序的 `Vec<ToolResult>`，不能仅用工具名或文本代替关联信息。
- `ModelRequest { messages: Vec<ChatMessage>, tools: Vec<ToolDefinition> }`：消息有序，工具按名称稳定排序。模型名称、密钥和厂商配置由适配器持有，不进入消息协议。
- `ToolDefinition { name: String, description: String, argument_schema: Value, required_permissions: Vec<Permission>, concurrency: Option<ToolConcurrency> }`：名称非空且在宿主内唯一；参数契约是描述对象参数的 JSON Schema 对象。首版由工具的 `validate_arguments` 实现所声明的约束，不宣称提供通用 JSON Schema 引擎。`ToolConcurrency::ParallelSafe` 表示实现可与其他调用同时运行；`Serial { scope: String }` 表示同一非空作用域中的本地 Tool Future 按模型返回顺序互斥。省略声明等价于 `Serial { scope: "eve.default" }`。声明共享可变资源的工具必须使用同一串行作用域；不同显式作用域可以在宿主并发上限内同时运行。该声明不是远端资源锁：超时或取消后已发出的请求仍可能继续产生副作用，工具须自行保证幂等、互斥或补偿。
- `ToolCall { id: String, name: String, arguments: Value }`：ID 与名称非空，参数必须是 JSON 对象。
- `ToolResult { call_id: String, output: ToolOutput }`：`ToolOutput::Success(Value)` 或 `Failure { code: ToolFailureCode, message: String }`；ID 必须与请求相同，失败说明必须可安全回传。一个 `ToolCalls` 批次后若继续请求 Provider，必须追加恰好一个同长度、有相同顺序的 `ToolResults` 消息：第 `i` 个结果的 `call_id` 必须等于第 `i` 个调用的 ID，包括宿主合成的所有失败结果。
- `ModelResponse`：`Final { text: String }` 或 `ToolCalls { calls: Vec<ToolCall> }`。调用批次不能为空，批次内调用 ID 必须唯一；调用顺序是结果回传和串行调度的规范顺序。空白最终文本、空白 ID/名称、重复 ID、JSON 解析失败或非对象参数为协议错误；合法对象未通过工具约束则是 `InvalidArguments` 工具失败，两者不可混用。
- `LlmError`：至少区分配置错误、上下文失败、后端失败、Provider 失败、Provider 超时、协议错误、不支持和轮数耗尽；不复用通用 `PluginError` 表达模型协议。

三个能力 Trait 均为 `Send + Sync`，异步方法返回不依赖 Tokio 的 `LlmFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, LlmError>> + Send + 'a>>`：

- `LlmProvider::complete(&self, ModelRequest) -> LlmFuture<'_, ModelResponse>`。
- `ContextAssembler::assemble(&self, TurnInput) -> LlmFuture<'_, ContextSnapshot>`；`TurnInput { text: String }` 表示本轮用户输入。
- `Tool::definition(&self) -> ToolDefinition`、`validate_arguments(&self, &Value) -> Result<(), ToolValidationError>`、`execute(&self, ToolCall, ToolExecutionContext) -> ToolFuture<'_, Value>`。`ToolValidationError { message: String }` 只描述可安全回传的参数问题；`ToolExecutionError { kind: Failed | Cancelled, message: String }` 只描述可安全回传的执行失败或工具主动确认的取消；宿主分别统一映射为 `InvalidArguments`、`ExecutionFailed` 和 `Cancelled`，不让工具自行伪造 Provider、超时或协议错误。`ToolExecutionContext` 只为本次调用公开 `is_cancel_requested()`，不能读取或改变其他调用的状态。

工具插件实现 `Tool` 的定义、参数校验和异步执行。为了通过现有 `ServiceRegistry` 存取，插件用有大小的 `ToolService(Arc<dyn Tool>)` 包装工具对象；上下文插件同样用 `ContextService(Arc<dyn ContextAssembler>)` 暴露服务。Runtime 使用显式 `ToolBinding` 表绑定工具名、Service ID 和预期所有者，不尝试枚举 ServiceRegistry。

`ToolFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ToolExecutionError>> + Send + 'a>>` 同样不绑定 Tokio，以免工具将业务失败伪装成 Provider 或宿主错误。`LlmProvider::complete` 与 `Tool::execute` 的同步调用部分不得阻塞、访问网络或产生外部副作用，所有这类工作必须在返回的 Future 中完成，才能受超时和取消约束。调用上下文的取消标记是协作式信号：工具应尽快停止尚未完成的工作并避免开始新的外部副作用，但宿主无法证明远端工作、计费或既有副作用已经停止。工具在初始期限内主动确认取消时返回 `Cancelled`；宿主超时后即使工具在收尾窗口内返回，也固定生成 `TimedOut`。调用者直接取消整轮时没有可回传的 `ToolResult`。

## 装配、权限与生命周期

`LlmHost` 构造时显式接收 Provider、`Arc<dyn ServiceRegistry>`、`Arc<dyn RuntimeInspector>`、`Arc<dyn PermissionChecker>`、`ContextBinding { service_id, expected_owner }`、工具绑定和配置。组合层在创建 Kernel 前克隆 `KernelServices.registry`、`KernelServices.permissions` 的同一组 `Arc` 给宿主；Inspector 使用同一 Kernel 的公开实现，不能为宿主另建一套服务或权限后端。

定义层为 `RuntimeInspector` 新增 `plugin_manifest(&PluginId) -> PluginResult<PluginManifest>`：返回独立只读快照，未注册返回新增的 `PluginError::PluginNotFound`，不等待生命周期锁。宿主对工具所有者的 Manifest 调用 `PermissionChecker::check`，工具定义所列权限必须逐项通过；这是可信插件的声明检查，不是用户授权系统或操作系统沙箱。

`ToolBinding { name: String, service_id: ServiceId, expected_owner: PluginId }` 必须拒绝空名、重复名和空标识；Context 绑定同样指定 Service ID 与预期所有者。启动所需插件后，每轮在首次 Provider 请求前完成装配预检：核对所有绑定的服务、类型、所有者、Active 状态，以及工具定义名称和参数契约。预检失败不调用 Provider 或工具。工具定义保存为本轮快照，调用前重新获取服务并核对名称、所有者、状态、权限和参数；定义漂移按工具不可用处理，不能临时扩大本轮权限或替换已公布的契约。

Active 快照不是执行租约，服务 `Arc` 也不能阻止 Scope 被清理。首版的 `LlmHost` 是相关插件的唯一组合门面，私有持有 Kernel 的生命周期与注册入口，并以同一个异步 gate 串行 `run_turn`、启动、停止和卸载：先启动，再等待本轮返回，最后停止。相关插件必须在构造宿主前完成注册；首版不暴露构造后的注册入口，后续若加入，必须进入同一个 gate。宿主不能同时暴露可绕过 gate 的 Kernel 操作入口。后台任务不得替换本轮服务；在这个约束外直接并发调用 Kernel，不承诺消除竞态。

工具 Future 由宿主直接等待，不会自动成为 TaskManager 中的任务。每个调用在通过分项预检、取得并发槽位且即将首次轮询其 Future 时开始计算 `LlmHostConfig.tool_timeout`；排队中的调用尚未开始，不计入该期限或“已开始工具次数”。期限届满时，宿主先设置该调用的取消标记，在正数 `tool_cancellation_grace` 内继续轮询供工具收尾；期限在同一轮询点与正常完成同时就绪时，期限优先。收尾窗口结束后丢弃尚未完成的 Future，固定生成 `TimedOut` 失败结果，其他同批调用继续。调度器只有在 Future 正常完成或已在收尾窗口后被丢弃时才释放其本地串行作用域；它不保证超时后远端副作用不与后续同作用域调用重叠。调用者丢弃 `run_turn` 时，宿主为所有未完成调用设置取消标记并立即丢弃本地 Provider 与 Tool Future，停止整轮，不再发起第二次 Provider 请求或保证取得诊断；随后 gate 可以释放，后续生命周期操作同样不构成对未确认远端副作用的锁。这不保证工具或远端系统已经停止，也不回滚已发生的副作用。工具应异步、响应取消且可结束。可打断会话和执行租约留待后续契约处理，不增加 Kernel 的模型职责。

首个 `LlmHost` 直接接收 `TurnInput`、返回 `TurnOutput` 或 `TurnFailure`，不在 `eve-llm-api` 定义 Channel Service。Channel 插件是组合层调用者，负责把输入转成 `TurnInput` 并将最终文本送回用户；它不参与 Provider 或工具协议。Memory 插件同样由 Context 插件按自己的公开依赖读取，宿主只接受已经组成的动态快照。

## 上下文布局

固定系统规则与输出格式由 `LlmHostConfig` 保存，配置版本非空；工具定义来自装配预检后的快照。Context 插件只返回 `ContextSnapshot { revision: String, profile: String, memories: Vec<String>, history: Vec<ChatMessage> }`，revision 非空，其他动态槽位允许为空。首版 history 只接受已完成的 user/assistant 文本消息，不接受 system 指令或未配对的工具历史。Runtime 是唯一的布局执行者，顺序固定为：

1. 系统规则和输出格式。
2. 名称稳定排序后的工具定义。
3. 版本化用户档案。
4. 相关记忆。
5. 会话历史。
6. 本轮用户输入。
7. 循环中新增的 assistant 工具调用和 tool 结果。

一轮只 assemble 一次，配置、工具和动态快照在第二次 Provider 请求中保持不变；只追加本轮调用及结果。memories 和 history 保留插件给定次序，不进行内容排序。固定前缀在同一配置版本内不可由动态插件重排或改写。该布局规定逻辑顺序，适配器分别映射消息和 tools 字段，不承诺所有厂商的缓存命中率。首版不规定压缩、检索或记忆持久化算法。

## 单轮工具循环

首个实现的 `max_tool_rounds` 固定为 1，其他值作为配置错误拒绝。一轮至多两次 Provider 请求、一个工具调用批次；批次内可有多个调用。`max_parallel_tool_calls` 和 `tool_timeout` 必须为正数。未知工具或校验失败也消耗对应调用机会，不能无限修复重试：

```text
Channel 输入
  -> ContextAssembler 生成快照
  -> Runtime 按固定槽位构建 ModelRequest
  -> LlmProvider 返回 Final 或 ToolCalls
  -> ToolCalls: 分项状态/所有者/权限/参数校验，按并发策略执行 Tool
  -> Runtime 按原调用顺序追加 ToolResults
  -> LlmProvider 返回 Final
  -> Channel 输出
```

初次 `Final` 直接结束。结构正确的批次先完整执行所有可能产生宿主后端错误的读取和分项预检，再按下表逐项处理：工具失败、超时或取消也会追加对应 assistant 调用及失败 `ToolResult`，再交给第二次 Provider 请求生成最终回复。调度器先按模型返回顺序为每个串行作用域建立队列，再以 `max_parallel_tool_calls` 限制所有正在执行的调用；不同结果的完成先后不得改变回传顺序。若预检后仍发生无法安全降级的宿主后端错误，调度器必须向所有已开始调用发出取消、丢弃其本地 Future，移除排队调用并终止整轮；不得向第二次 Provider 请求发送不完整的 `ToolResults`。第二次再返回任意工具调用批次时立即报轮数耗尽，绝不执行第二批工具。

| 条件 | 结果 | 是否继续请求 Provider |
| --- | --- | --- |
| 装配时服务缺失、类型/所有者/状态不符、名称冲突、Schema 形状不符 | 宿主配置错误 | 否，首次请求也不发出 |
| Context 失败、上下文格式不合法 | 宿主上下文错误 | 否 |
| Registry/Inspector 读取失败，或权限后端返回非拒绝类错误 | 宿主后端错误 | 否，不伪装成工具缺失或权限拒绝 |
| 模型请求未绑定的工具 | 该调用为 `UnknownTool` | 是 |
| 调用时服务缺失、类型不符或定义漂移 | 该调用为 `Unavailable` | 是 |
| 调用时服务所有者不符、插件不存在或不再 Active | 该调用为 `OwnerMismatch` / `Inactive` | 是 |
| 权限明确拒绝 | 该调用为 `PermissionDenied` | 是 |
| 对象参数不满足工具校验 | 该调用为 `InvalidArguments` | 是 |
| 工具执行返回错误或发生受支持的 unwind panic | 该调用为 `ExecutionFailed` | 是 |
| 单项工具超过期限或工具明确确认取消 | 该调用为 `TimedOut` / `Cancelled` | 是 |
| 调用者取消本轮 | 终止整轮，不生成不完整的结果批次 | 否 |
| Provider 错误、超时、不支持、协议损坏或轮数耗尽 | 对应 `LlmError` | 否 |

`ToolFailureCode` 是公开枚举，测试比较错误码而不是说明文本。批次内一个 `ToolResult` 的失败不得抹除其余结果；只有无法安全继续整轮的宿主后端错误、Provider 错误、协议错误或调用者取消才终止整轮。工具校验/执行 panic 转为该调用的 `ExecutionFailed`，装配阶段定义回调或上下文 panic 转为宿主错误；均隔离同步调用与 Future 轮询，沿用只捕获 unwind、不承诺恢复 abort/双重/析构 panic 的边界。原始后端错误或 panic 载荷不直接回传模型。

`LlmHostConfig` 的 Provider 超时、工具超时、工具取消收尾窗口和最大并发数均为正数；`max_parallel_tool_calls` 默认值为 `10`。组合层在本轮开始前从配置能力取得并校验这些值，形成不可变快照；确定性 Mock 切片可直接显式构造已校验配置。配置服务的命名空间、来源合并、自举和不可用策略由 Issue #32 定义，不能由 LLM 宿主私自解析环境变量或静默回退。每次 Provider 请求在 `complete` 返回且即将首次轮询其 Future 时独立开始计时；期限在同一轮询点与完成同时就绪时，期限优先，宿主丢弃该 Future 并返回 `LlmError::ProviderTimeout`，不自动重试。调用者丢弃 `run_turn` 时同样丢弃在途 Provider Future。上述本地取消均不能证明远端已停止或不会计费；也不能回滚已经完成的工具副作用。

`run_turn` 返回 `Result<TurnOutput, TurnFailure>`；成功保存文本与诊断，失败保存 `LlmError` 与同一诊断。`TurnDiagnostics` 至少保存失败/完成阶段、已开始的 Provider 请求次数、工具批次数、已开始工具次数、每个调用 ID 与原始顺序、并发峰值和已生成的 `ToolResult`。诊断只在返回值内保存，不自动写日志或持久化，不保存密钥；调用者自行决定脱敏与留存。首版丢弃调用不保证取得报告。

首版不包含流式输出、跨批次并行、会话状态恢复、Jev 或新消息打断。首版含批次内的协作式取消和逐项超时，但不保证强制终止或副作用回滚。会话控制与重规划由 Issue #29 处理；真实 Provider、最小状态恢复和 VCPToolBox 缺口回顾在 Issue #24 的后续切片交付。

## 验收

确定性 Mock Provider 必须请求一批示例工具，再接收带相同调用 ID 的结果并返回最终文本。无网络集成测试按上表覆盖装配失败和调用失败两类 Service 场景，验证权限检查使用实际所有者 Manifest、后端失败保留、参数分层、工具错误与 panic、Provider 错误和超时、非法回复及轮数上限；并实际验证并行安全调用发生重叠、同一串行作用域按序不重叠、并发上限生效、完成先后变化仍按调用 ID 和原始顺序回传、单项失败/超时/取消不丢弃同批其他结果。同时验证固定前缀、动态槽位、本轮快照稳定和失败诊断。示例使用公开接口启动插件、等待整轮、再停止并确认清理，不写入真实 API 密钥。

## 首个 OpenAI 适配器（后续实现）

选择 Responses API 与独立 `eve-llm-openai` 实现 crate，使用 `reqwest` + `serde`/`serde_json` 的小型 HTTP 适配器；具体版本在实现时依据 Rust 1.89 的兼容检查锁定。理由是映射与网络层可独立测试，厂商类型只存在于适配器，Kernel、通用 plugin-api 和工具插件不依赖 HTTP 客户端。选择记录见 ADR-0029；实现前必须重新核对 OpenAI 官方文档中的字段、数据控制和支持模型。

Eve 的工具定义映射为 Responses 的 function 工具；`ToolCall.id` 对应 `call_id`，JSON arguments 解析后进入对象参数；成功值或结构化失败序列化为 `function_call_output.output`，保留同一 `call_id`。请求设置 `parallel_tool_calls: true`，适配器完整保留同一响应中的调用顺序与每个 `call_id`。首版显式 `strict: false` 以保留插件 Schema 原意，仍强制本地参数校验；不能替插件增删 required 字段。

首个适配器仅支持可无损映射的纯文本/函数调用批次子集。需要回传 reasoning 或其他额外输出项、混合文本和调用、内置工具，或无法保留完整调用批次及其顺序时，返回 `Unsupported`，不择一执行或静默丢弃字段。完整 reasoning 支持须另行设计适配器私有续传状态；不能把它当普通文本混入通用协议。空内容或坏 JSON 是 Protocol；非成功 HTTP 或未完成响应是 Provider 错误。

适配器每次发送完整 Eve 消息，显式 `store: false`，不依赖 `previous_response_id` 实现会话恢复；这不等同于服务端零保留承诺，实际数据控制政策与 API 行为以实现时的 OpenAI 官方文档为准。模型与密钥由宿主提供，首个真实验收模型必须实际验证支持上述子集，不在契约中固定模型名。转换 fixture 测试先覆盖工具定义、调用 ID、结果回传、错误和不支持输出，再进行真实文本与工具 smoke test。

实现前复核的官方依据：

- [Responses 迁移指南](https://developers.openai.com/api/docs/guides/migrate-to-responses)：API 选择、函数映射与显式状态控制。
- [Function calling](https://developers.openai.com/api/docs/guides/function-calling)：调用 ID、结果回传、strict、并行工具与 reasoning 项的回传要求。
- [数据控制说明](https://developers.openai.com/api/docs/guides/your-data)：响应状态与其他数据保留控制的区别。
