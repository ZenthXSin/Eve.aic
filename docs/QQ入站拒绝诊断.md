# QQ 入站拒绝诊断

`bridge_invalid_route_or_text` 表示 Node 桥接在生成 Rust 入站帧之前拒绝了消息。拒绝发生在回执、主模型和工具调用之前；同一条消息不会进入原文档案。旧版本把信封校验与归一化空正文共用一个固定码，并在进程内按码永久去重；旧日志的一条警告只能证明至少一次拒绝，无法恢复具体字段、次数或原文。

新协议保持 `version:1`、`type:warning`、`code:invalid_route_or_text`，增加独立版本为 1 的 `ingress` 对象。Rust 仅转存白名单，旧桥接只有固定码时继续兼容；未知诊断版本、任意字符串和额外字段不回显。

| 原因 | 校验位置 |
| --- | --- |
| `message_id`、`sender_id` | 平台消息编号与发送者标识的类型、格式或容量 |
| `reply_target`、`target_scope`、`target_message_id`、`target_id` | SDK 回复路由缺失、范围或消息编号不一致、目标标识无效 |
| `c2c_target`、`group_target` | 回复目标与同条消息的发送者或群不一致 |
| `content_type`、`content_bytes` | SDK `content` 不是字符串，或超过 32 KiB |
| `normalized_text_empty`、`normalized_text_bytes` | 规范化正文不合规且没有本条可接收的附件 |

Rust 日志中 `ingress_stage` 仅为 `envelope` 或 `normalized_text`。`ingress_reason` 是按校验顺序遇到的第一个失败原因，不表示已穷尽该事件的所有问题。

每种原因第一次立即报告，此后最多每分钟一条汇总；到期无需新的入站消息也会输出。`ingress_count` 是自上次报告以来的拒绝数，`ingress_total` 是本桥接进程内该原因的累计数，`ingress_elapsed_ms` 为距上次报告的单调时间。计数限制为 JavaScript 最大安全整数；溢出后保留饱和值并将 `ingress_counts_saturated` 置为 true，不能再视为准确总数。原因集合固定，最多 12 个计数器和计时器，不按用户或消息编号建立状态。

`ingress_last_*` 只描述这次汇总里最后一条拒绝的形状，不代表整个批次：包括固定事件分类、字段类型、字符串 UTF-8 字节数、顶层附件与元素数量，以及成功筛选出的本条附件数。非字符串字节数为 `null`；归一化尚未运行时其字节数也为 `null`。原始类型、名称、编号、URL、正文、引用、凭据和 SDK 异常均不写入诊断。

计数和最近形状仅驻留内存，重启从零开始，不恢复被拒绝事件、不重放消息，也不建立新授权或回复机会。正常停止会尽力冲刷尚未报告的计数并清理计时器；硬退出、Rust 已停止读取或日志后端失败时仍可能丢失未写出的汇总，不能承诺跨崩溃精确计数。

纯自身 @ 被剥空后丢弃是合成事件可确定复现的问题，现已保留真实提及文本进入会话。不能据此推断任何缺少原文的历史拒绝一定由纯 @ 导致。正常文字、图片、文件和原生命令继续使用同一条 SDK 消息的原始路由与 `original_text`。

离线验证使用 `npm --prefix connectors/qqbot test`，覆盖实际 SDK 1.0.4 的事件分派、纯提及、多提及、附件、空白、引用、每个路由失败分支，以及限频计数、停止冲刷与诊断脱敏。Rust `bridge_warning` 单测核对日志白名单；进程验收使用本地 QQ 与模型替身，不需要真实 QQ 或模型凭据。
