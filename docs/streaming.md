# 流式输出协议（实施草案）

对应 #45。定义层在 eve-llm-api，厂商 SSE 对象只存在于 eve-llm-openai；内置宿主负责工具循环，通道插件实现异步事件接收器。

## 状态与顺序

每个接收器绑定一次 run_turn 调用；会话模式所有事件带持久化 turn_id，普通模式为 None（调用方负责把接收器绑定自己的请求标识）。请求编号从 1 开始。

ProviderStarted → TextDelta* → ResponseCompleted。函数调用只在正常终态验证整个输出后返回，不发布参数片段；宿主先完整预检再运行工具。ToolBatchStarted → ToolResult（按调用原顺序）→ 第二个 ProviderStarted → TextDelta* → ResponseCompleted → TurnCompleted。无工具的请求直接 TurnCompleted。

TextDelta 是暂时显示；ResponseCompleted 是某次模型请求结束；TurnCompleted 是完整轮次已生成；SessionSaved 只在会话完整 transcript 成功持久化后发送。失败发送 Failed（尽力投递），不会发布 SessionSaved。保存失败保留最终输出和原 Pending。保存成功但最后通知投递失败单独返回 Delivery，携带完整输出，不可重试本轮。

## 背压与取消

公开接收器是返回 Future 的异步 trait。宿主逐次 await，不建无限队列；插件可桥接有界通道，接收端断开时返回 Cancelled。每次投递有期限，超时/异常终止生成和工具，保留已有副作用诊断。丢弃 run_turn Future 会丢弃网络读取并取消工具任务；生命周期准入锁持续覆盖工具 Future 析构及会话提交。

回调不得执行阻塞 I/O、重入同 Kernel 生命周期操作或同步等待当前轮次；队列容量和字节限制由通道插件明确设置。SSE 总字节预算默认 8 MiB（含协议帧），单帧不超过该预算；不自动重连或重试。请求期限包括读取和消费者背压。

## 配置与兼容

普通配置插件选择 complete / stream，默认 complete。Provider 默认 stream 返回 Unsupported；不静默降级。两种模式共享完整输出验证、固定提示前缀、工具排序、历史及会话提交，增量不重建提示、不单独写历史。

## Responses 支持边界

仅 assistant output_text 与 function_call。验证 SSE 事件类型/名称、序列（存在时）、response/item 标识、输出顺序、增量与 done/最终 output 一致。拒绝重复或矛盾终态、混合文本和工具、reasoning、refusal、annotations、namespace 和未知事件。UTF-8 在完整行组装后解码，支持 LF/CRLF/CR、注释、多行 data。未收到 completed 的 EOF、坏 JSON 或超时均失败。

官方参考（2026-09-30 复核）：
- https://developers.openai.com/api/docs/guides/streaming-responses
- https://developers.openai.com/api/docs/guides/function-calling
- https://developers.openai.com/api/reference/resources/responses/streaming-events

## 验收与边界

实施后补充 Mock、本地 HTTP、慢消费者/断开、提交失败、跨进程恢复及 Windows/Ubuntu CI。真实 API 独立验收，凭据只在宿主环境。#29 的消息分类、旧回复抑制与 Jev 路由后续交付。本地取消不保证远端停止或副作用回滚。
