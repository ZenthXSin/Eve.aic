# Tianlu Mod 科技树问题分析


Tianlu 这个 mod 里同时存在两套“科技树”概念：

1. 原版战役科技树：`mod.content.TianluNatural_StarTechTree`
2. 自定义研究表：`mod.content.LaboratoryTechTree` + `special.LTechTree`

真正明显有问题的是第二套。它不是原版 `mindustry.content.TechTree` 节点树，只是一个自定义数组表，没有和原版研究状态、UI、存档写回形成闭环。

## 关键结论

- `mod.content.TianluNatural_StarTechTree` 结构上能工作：给 `mod.content.TianluPlanets.Natural_Star.techTree` 赋值，并使用 `mindustry.content.TechTree.nodeRoot/node/nodeProduce` 建树。
- `mod.content.LaboratoryTechTree` 不是原版科技树，只是研究条件缓存。
- `mod.Tianlu.update()` 只在 `mindustry.game.EventType.WorldLoadEvent` 触发一次，因此 `LaboratoryTechTree.update()` 也只同步一次，不会持续刷新研究前置状态。
- `special.LTechTree` 没有“已研究结果写回到 `UnlockableContent`”的逻辑，也没有存档逻辑，因此即使条件满足，也不等于内容真正解锁。

## 证据

### 原版科技树实现

- `mod.content.TianluNatural_StarTechTree`
- `mod.content.TianluPlanets`
- 知识页：[Mindustry 科技树节点解锁流程](../concepts/mindustry-block-tech-tree-json.md)

### 自定义研究系统实现

- `mod.content.LaboratoryTechTree`
- `special.LTechTree`
- `mod.Tianlu`

## 具体问题

1. `LaboratoryTechTree.update()` 只做 `PresetTech[ii].unlocked()` 的读取，不做任何解锁提交。
2. `WorldLoadEvent` 只调用一次 `mod.Tianlu.update()`，研究进度不会随游戏过程持续变化。
3. `LaboratoryTechTree.load()` 里 `LaTechTree[0]` 和 `LaTechTree[1]` 都指向同一个研究目标 `HighTechnologyBlocks.IntermediateResearchLaboratory`，像重复占位。
4. 这套自定义研究表没有接原版 `mindustry.content.TechTree.TechNode`、研究 UI、存档系统，所以更像半成品。

## 排查优先级

1. 先区分你要修的是原版星球科技树，还是实验室自定义研究系统。
2. 如果是“战役里节点不显示/不连通”，先查 `mod.content.TianluNatural_StarTechTree` 的父子挂接和根节点选择。
3. 如果是“实验室研究后没解锁”，根因在 `mod.content.LaboratoryTechTree` 这套系统没有真正把结果写回原版解锁状态。

## Related

- [Mindustry 科技树节点解锁流程](../concepts/mindustry-block-tech-tree-json.md) — 原版科技树的节点、条件、支付与解锁流程
- [Mindustry Mod 生命周期与解析器边界](../concepts/mindustry-mod-lifecycle-parser-boundary.md) — `loadContent()`、运行期事件与内容注册边界
