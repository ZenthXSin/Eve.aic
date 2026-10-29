# 统一主体账本

`eve-subject-api` 定义公开契约，`eve-subject-plugin` 持久化实现，宿主组合层绑定 QQ 等入口。一个 Eve 全局调度多个会话，原文、目标、检查点和结果始终绑定 `SubjectScope { channel, conversation_id, principal_id }`。`snapshot_for` 精确匹配三个字段；群和私聊即使属于同一人也不互读原文。控制句柄只交给可信宿主，不注册到模型可访问的服务目录。

## 装配

```rust,ignore
let plugin = SubjectPlugin::new()?;
let subject = plugin.controller();
kernel.register(Box::new(plugin))?;
kernel.start(&PluginId::new(SUBJECT_PLUGIN_ID)?).await?;
let driver = SubjectDriver::new(Arc::new(subject.clone()), "qq-subject")?;
```

入口以可信路由和消息 ID 调用 `enqueue`。宿主持续调用 `driver.run_one(now_ms, dispatch)`；dispatch 必须只做一次有限调度，以 `event.id` 为后台任务的幂等来源，持久化受理后返回 `SubjectDispatch::Started`。Driver 建立与任务关联的关注，然后释放全局租约。长模型执行继续使用宿主后台任务执行器；完成、阻塞与失败通过 `TaskExecutor` 来源重新排队，由原范围消费。同步旧入口可调用 `observe_completed` 记录已经处理的输入，这不代表主体驱动了该次执行。

`schedule_due` 每个关注至少相隔三十分钟生成一条自主检查事件，已有未处理检查时合并。它不发送通知。执行器根据真实进展、阻塞变化和用户许可判断是否交流；完成回执处理后应将对应关注更新为 Completed、Blocked 或新的 Active 目标。

## 调度与恢复

停止/取消、用户输入、恢复/任务回执、自主探索依次调度，同级先进先出。全局同时只发一个有限调度租约；高优先级到达时标记正在运行的事件，请执行器在安全边界检查、交回执行权。取消只影响精确范围，运行中的任务须由宿主合作式停止；取消不是对已经发生的副作用的回滚。

每次状态变更先提交持久账本，成功返回后才允许副作用。租约使用持久递增 generation 栅栏；到期旧 token 不能写入或了结。执行器必须在外部副作用前写 EffectPending，取得明确回执后写 Safe。启动时立即废弃旧进程租约，运行中到期也执行相同恢复：Safe 至多恢复三次，EffectPending 转 Unknown，未知事件永不自动重放。人工核对可用 `resolve_unknown` 了结，不能重新排队。租约只约束账本写入；外部系统仍须使用幂等键和自身的取消机制。

Driver 在外部派发前保存 EffectPending，取得返回后保存 Safe 调度回执。如果回执保存后、最终了结前退出，重启只补关注和本地账本了结，不再次调用外部派发闭包。闭包错误时立即记 Unknown 并释放租约，其他窗口继续调度；进程直接退出时由重启或租约超时恢复。回执保存无法确认时不能推断任务是否已经派发，保留未知结果。损坏 JSON、重复键、无效状态和未知版本拒绝启动并保留原数据。写入失败可能已经提交，所有句柄立即关闭，重启后以实际存储为准，不使用旧缓存覆盖。

## 容量

事件至多 256 条、关注至多 128 条、去重键至多 4096 条；原文至多 8 KiB、检查点至多 16 KiB，总账本至多 12 MiB。容量不足时仅回收最早已了结且结果明确的事件或完成/取消的关注；活动与 Unknown 事件不删除，没有可回收项就拒绝新增。已回收事件的去重键至少保留七天，窗口满时拒绝新增，不静默丢弃键。仍保留完整事件的键不因过期删除。旧来源命中墓碑会拒绝重建，窗口外且完整记录已回收的重复来源可能重新受理，宿主仍应使用平台幂等来源。后台任务执行与通知各自有独立持久账本，此处不取代它们的副作用回执。

## 本地验收

```sh
cargo test -p eve-subject-api -p eve-subject-plugin
cargo run -p eve-subject-plugin --example multi_window
cargo clippy -p eve-subject-api -p eve-subject-plugin --all-targets -- -D warnings
```

示例只运行本地双窗口模拟，不连接 QQ 或发送外部消息。集成测试覆盖优先级、范围隔离、并发后台派发与结果回流、检查合并、租约失效、崩溃恢复、未知不重放、容量和提交不确定。
