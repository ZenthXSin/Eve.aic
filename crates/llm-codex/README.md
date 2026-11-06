# 原 Codex 模型适配器

`eve-llm-codex` 独立实现 `LlmProvider`，直接调用 [vendor 原文件](../../vendor/codex-0.162.0/README.md) 中的 Codex 0.162.0 HTTP、重试和 SSE parser。原 `eve-llm-openai` 保留，选择入口由宿主配置负责。该版本 Codex 只支持 Responses；配置为 Chat 时必须在请求前报错。

每次请求必须携带宿主创建的 `ModelSession`。`session-id`、`thread-id`、`prompt_cache_key` 随用途和会话保持稳定，审查和各后台任务分别隔离。`x-client-request-id` 来自持久轮次和请求序号，HTTP 重试复用原值。模型参数不能指定或替换这些身份。

`request_fingerprint` 只读编码实际 Responses 正文和端点，供宿主判断待恢复请求是否未变；不连接网络、不读写轮次 token、不包含凭据或请求序号。切换实现、输入、工具或实际模型配置时不能沿用旧请求 ID。主/辅共同构造、识图入口及显式切换工具已编写，见[接线设计](../../docs/Codex适配器接线设计.md)，尚待 Rust 1.95 的锁文件解析、构建和实际验收。

同一 provider 内以会话和轮次隔离 `x-codex-turn-state`，工具往返复用原服务器值；新轮次不复用。状态只保存在内存，最多缓存 1024 个轮次，优先淘汰无人持有的最旧项；全部在途时明确失败。新建 provider、进程重启或淘汰后保留宿主持久会话身份，服务器 token 从新响应取得，绝不借用其他会话。Eve 持久历史、任务恢复和工具去重由原宿主负责，适配器不执行或重放工具。

胶水层将 Eve 历史、图片、函数定义与回执转为原 Codex 类型。仅追加 Eve 的 `max_output_tokens`、JSON 对象输出格式，并在 reasoning 未配置时省略该字段；永远不发送 temperature。原源码无需为兼容服务改动。原 `stream_request` 会以 thread ID 覆盖 request ID，且原请求类型没有输出上限字段，所以 Eve 选用同一原客户端的公开 `stream(Value, headers, ...)` 入口。两入口共享原 `stream_encoded`、HTTP、重试和 SSE；稳定用户会话与持久请求序号属于 Eve 的桥接策略，不能声称 HTTP 默认参数与完整 Codex 完全相同。

只有收到原 parser 的完成事件且已添加的输出项都有完成回执，才返回文本或函数批次。reasoning 和 commentary 不冒充终态回复；不支持的原生工具明确失败。原 `SseTelemetry` 回调仅做只读合同校验，阻止损坏或 incomplete 事件被原宽容解码跳过后交付部分工具；完成事件实际提供 `output` 时，核对其与原 done 项数量和内容一致，不补造缺失项。不产生或修改事件、不另写 SSE parser。合法引用标注不妨碍普通正文交付，但 Eve 文本契约不单独返回 annotation 结构。Eve 当前通用协议不能持久化完整 Codex 原生 reasoning、加密上下文、非文本输出或混合最终文本与函数批次，因此此模块不是整个 Codex core/runtime。

原 parser 的文本增量不携带输出项身份和 phase，Eve 的 sink 又必须恰等最终正文，所以本适配器仍按原 SSE 持续接收与校验，正文在完整响应验证后一次交给宿主，工具批次不交付 commentary。当前不提供逐字预览，避免将过渡说明混入回复或触发宿主的完整性错误。

缓存计数读取原 parser 保留的真实 usage metadata：字段缺失为未知，真实零仍为零。HTTP 原请求日志关闭，Eve 诊断只记录固定阶段、时长、字节数和错误分类；原 SSE 模块的 TRACE 会输出服务端内容，宿主不得为该原模块启用 TRACE。

最低 Rust 版本为 1.95，保持原 Codex manifest 的版本要求。锁文件完成后验收命令为 `cargo +1.95.0 test -p eve-llm-codex --test responses --locked`。23 个有效用例全部使用本地 TCP/SSE 替身；恢复用例另起隔离测试子进程，读取临时持久请求，验证原会话不变且服务器 token 重新取得。工具回执在此是合成输入，真实执行与去重由宿主验收覆盖。另覆盖原重试、完整工具批次、缓存字段、取消、限额和期限。原文件校验命令为 `python3 scripts/verify-codex-upstream.py`；文件校验不代替运行测试。

宿主另有 4 个 QQ/独立 Web 实际进程场景，通过用户入口切换后捕获真实 HTTP/SSE，覆盖在途冻结、身份、重启去重、high 和无 temperature。运行 `python3 connectors/qqbot/test/model_adapter_test.py` 与 `python3 scripts/test-web-model-adapter.py`，分别可用 `EVE_QQBOT_BINARY`、`EVE_WEB_BINARY` 指向同一源码构建产物；当前尚未执行。
