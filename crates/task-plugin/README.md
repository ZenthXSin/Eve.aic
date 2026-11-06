# 任务通知与恢复

通知候选由 `delivery_snapshot`、`progress_snapshot` 和指定身份的 `get_owned` 只读产生。模型审视候选、决定等待或保持安静时不认领通知，也不消耗发送次数。通道准备写出模型决定的正文时才调用以下接口。

- `claim_notice_expected(owner, id, finished_at_ms, attempt_count, channel, now_ms)` 在同一账本锁内核对完整身份、任务结束时间和认领前的尝试数，随后追加一次发送尝试。认领成功后，调用者保留同一结束时间及认领后的尝试总数。
- `record_delivery_expected(owner, id, finished_at_ms, attempt_count, result, now_ms)` 只结算该结束批次最后一条尚未结算的尝试。阻塞任务接续后再次结束，即便任务 ID 相同，旧回执也无法结算新通知。
- `TaskProgressToken::from_task` 生成下一序号和固定长度事实摘要。摘要覆盖身份、状态、取消意图、结果、检查点、恢复与期限信息，以及通知状态；不复制大段上下文。`claim_progress_expected` 在同一锁内重新计算并比较，同时检查现有间隔和范围预算。事实变化时无写入、无序号消耗，调用者重新审视。

平台确认带实际消息 ID 的 `Sent` 才能结算送达。`Unknown` 即时持久化，缺少 ID 的 `Sent` 同样记为 `Unknown`，保留未知结果且不重发。认领成功但回执未结算时退出，重启把该尝试记为未知；只读候选不会因此变为未知。提交确认丢失会关闭当前实例，重新打开后只依据持久账本恢复，冷目录和内存账本遵守同一规则。

原 `claim`、`record_delivery`、`claim_progress` 保留兼容；新自主发言链路必须使用精确版本接口，不能在新接口不可用时回退到仅按 ID 认领。替换实现未覆盖新契约时默认返回 `Unavailable`。本地收件箱仍使用独立的 `record_local_delivery` 和 `record_local_progress`，不会伪造平台回执。

事实摘要采用带版本前缀的确定性 SHA-256 编码。升级摘要规则使旧候选失效并重新审视，不需要改写任务执行账本。

冷任务在容量压力下可以保留为历史，运行工作集与历史容量、格式 5 和失败恢复约定见[任务工作集与历史归档](./WORKING_SET.md)。
