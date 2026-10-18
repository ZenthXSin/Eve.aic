# JSON 常见问题

> Source: conversation (drawer 绘制顺序与原理讨论)

本文汇总 Mindustry JSON 中经常遇到的典型问题、判断方向与常见误区，便于快速排查。

## Key Points

- **绘制顺序**：`drawer` / 绘制列表通常按书写顺序执行，后画的覆盖先画的。
- **`type` 误解**：JSON 中的 `type` 往往是反序列化入口，不一定等于运行时最终字段。
- **接口可用性**：不是所有类都支持 JSON 直写，是否生效取决于类的反序列化支持。
- **data patch vs JSON mod**：前者是改已有对象字段，后者是通过文件位置和 `type` 定义内容，两者不能混淆。
- **纹理/渲染异常**：常见于贴图路径、切片方式、绘制层次、shader 接入或 FrameBuffer 使用不当。
- **字段写了但没效果**：通常是字段名不对、类型不对、对象不支持、或被后续逻辑覆盖。
- **报错与运行异常**：优先看堆栈和加载阶段，判断是注册时机问题、资源缺失还是序列化失败。

## Common Pitfalls

- 把“看起来像”字段当成真实可用字段。
- 忽略绘制顺序，误以为引擎会自动帮忙排序。
- 只改 JSON，不验证底层类是否支持该配置。
- 将局部效果问题误判成全局加载错误。
- 不做最小化测试，导致排查范围过大。

## Quick Checks

1. 先确认 JSON `type` 是否对应正确类。
2. 再确认字段名、类型、层级是否正确——**字段名多一个s少一个s都不同**，如 `outputItem`（单物品）≠ `outputItems`（物品数组），`outputLiquid` ≠ `outputLiquids`。
3. 接着检查资源是否真的被加载。
4. 若是渲染问题，检查顺序、混合、遮挡、layer。
5. 最后用最小样例复现，逐项删减。

## Field Name Gotchas (被吞的最常见原因)

同一个语义的「单数 vs 复数」是不同字段，必须严格区分：
- `outputItem: ItemStack`（单个输出物品） vs `outputItems: ItemStack[]`（多个输出物品）
- `outputLiquid: LiquidStack`（单个输出液体） vs `outputLiquids: LiquidStack[]`（多个输出液体）
- 写数组值给单数字段 → 解析器静默吞掉不报错
- 写单数值给数组字段 → 也是静默吞掉

其他常见单复数陷阱：`consume` vs `consumes`、`input` vs `inputs`、`draw` vs `drawer`（部分类支持）

## Related

- [JSON 问题解决流程](json-problem-solving-flow.md) — JSON 问题的推荐排查顺序
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md) — 常见接口与字段入口参考
- [Mindustry JSON 中的 `type` 是否真实存在](mindustry-json-type-real-existence.md) — `type` 入口字段的含义说明
- [Mindustry Data Patch vs JSON Mod](mindustry-data-patch-vs-json-mod.md) — data patch 与 JSON mod 的区别
