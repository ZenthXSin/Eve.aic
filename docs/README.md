# EVE.AIC 文档中心

这里保存项目的长期设计、开发计划、执行记录和技术文档。

## 文档分类

- [企划案](./企划案.md)：项目愿景、目标和范围。
- [开发计划](./开发计划.md)：阶段目标、优先级和完成条件。
- [执行书](./执行书.md)：当前阶段的具体执行顺序和工作规则。
- [架构设计](./架构设计.md)：Rust Runtime、Plugin 和内核边界。
- [决策记录](./决策记录.md)：已经确认的技术和架构决策。
- [待确认问题](./待确认问题.md)：尚未决定、需要逐步讨论的问题。

## 第一阶段总览图

[打开 EVE.AIC 总览 SVG](./diagrams/eve-aic-overview.svg)

这张图把第一阶段需要讨论和验收的内容放在同一张图中：定义层、实现层和组合层的架构；核心功能关系；插件启动、失败回滚和逆序停止流程；从架构决策到 PR 的实现流程；Provider 与 Consumer 插件的协作效果；以及与《开发计划》一致的 Phase 0–5 路线。

![EVE.AIC 第一阶段总览图](./diagrams/eve-aic-overview.svg)

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
