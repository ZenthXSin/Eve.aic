# Mindustry 回答验证与低幻觉策略

> Source: conversation on 2026-06-07

这次调整把 Mindustry 相关回答的约束进一步硬化，目标是降低幻觉、缩短输出，并把验证动作前置到真正回答之前。

## Key Points

- Mindustry 相关内容输出前，必须先查看源码或知识库，未经验证不能直接回答
- 验证顺序仍以 [MDT / Mindustry 源码优先规则](../concepts/mdt-mindustry-source-first-rule.md) 为主：LMM / 知识库 → 源码 → 示例 mods
- 最终答复前必须发生至少一次工具调用，避免“纯脑补收尾”
- 默认输出尽量控制在 20 字内，非必要不展开，硬上限 150 字，少换行
- 完成任务前不要先发文字结论；如果仍在处理中，应继续以工具推进而不是提前总结
- 该约束不仅适用于单次回答，也应通过定时自优化任务持续强化

## Related

- [MDT / Mindustry 源码优先规则](../concepts/mdt-mindustry-source-first-rule.md) — Mindustry 问题的验证顺序基线
- [Mindustry 深度问答汇总](../concepts/mindustry-deep-qa-compendium.md) — 需要持续压低幻觉的高密度问答区域
