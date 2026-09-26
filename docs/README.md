# EVE.AIC 文档中心

这里保存项目的长期设计、开发计划、执行记录和技术文档。

## 文档分类

- [企划案](./企划案.md)：项目愿景、目标和范围。
- [开发计划](./开发计划.md)：阶段目标、优先级和完成条件。
- [执行书](./执行书.md)：当前阶段的具体执行顺序和工作规则。
- [架构设计](./架构设计.md)：Rust Runtime、Plugin 和内核边界。
- [决策记录](./决策记录.md)：已经确认的技术和架构决策。
- [待确认问题](./待确认问题.md)：尚未决定、需要逐步讨论的问题。

## 当前状态

项目处于第一阶段设计期，暂不实现 AGI 认知层。

当前优先目标是建立稳定的 Rust 插件 Runtime：

```text
Plugin Registry
    ↓
Dependency Resolution
    ↓
Plugin Lifecycle
    ↓
Context
    ↓
Event / Service / State / Task
    ↓
Cleanup / Shutdown / Recovery
```

