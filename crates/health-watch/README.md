# 健康事件通知

巡检只记录新出现的异常事实，模型是否发言由上层决定。读取 `snapshot` 不认领消息；准备实际发送时用 `claim_id(id, expected_at_ms, owner)` 精确认领审视过的事件，返回 `None` 时不能改拿另一条通知。原 `claim(owner)` 保留兼容。

`suppress(id, expected_at_ms, owner)` 仅将指定的 `Pending` 事件记为 `Suppressed`，保留决策归属身份，没有发送尝试或平台回执。同一身份重复决定幂等；发送中或未知事件不能改成安静。普通 `delivered` 不能写入 `Suppressed`。

首次记录 `Suppressed` 时账本升级为版本 2；旧版本 1 仍可读取，普通操作不强制升级，版本与状态不符则保留原文件并拒绝打开。重启只把 `Sending` 改为 `Unknown`，`Pending` 和 `Suppressed` 保持原义。

账本达到容量时可回收最早的 `Sent`、`Failed` 或 `Suppressed`。`Unknown` 继续保留，不自动重发或被静默覆盖。同一个异常持续存在不会重复创建事件；消失后再次出现会获得新 ID，先前的安静决定不会覆盖新事件。
