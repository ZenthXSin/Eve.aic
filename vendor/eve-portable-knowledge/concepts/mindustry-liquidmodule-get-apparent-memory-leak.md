# Mindustry `LiquidModule#get` 假性内存泄漏排查

> Source: 2026-08-06 对 Mindustry、DeepSpace 与本地示例 Mod 源码的联合分析

## 结论

`LiquidModule#get(Liquid)` 本身不分配对象，只执行 `float[]` 按 `liquid.id` 索引并返回 `float`。在热点路径里观察到堆占用高速上涨时，分配源通常是同一表达式或调用栈中的 Kotlin 捕获 lambda、数组或任务队列，而 profiler 将样本归因到 lambda 内实际执行的 `get()` 行。

## 源码证据

- Mindustry `LiquidModule.java:22`：液体量存放在预分配的 `float[] liquids`
- Mindustry `LiquidModule.java:102-104`：`get()` 仅返回 `liquids[liquid.id]`
- 原版 `ConsumeLiquids.java:43-58`：每 tick 的 `update()`/`efficiency()` 同样调用模块方法；原版实现不动态创建数组
- DeepSpace `ConsumeLiquids.kt:62-65,91-106`：热点消耗路径遍历固定 `consLiquids`，其中的 `get()` 不分配
- DeepSpace `绿藻池.kt:94-119`：每次 `draw()` 都创建 `drawCell` 捕获 lambda、`SglDraw.drawTask` 的捕获 lambda，以及 `Angles.randLenVectors` 的捕获 lambda；这些对象都可能逐帧分配
- `SglDraw.java:92-108,1057-1086`：任务缓存会保留当帧 lambda 与目标，`flush()` 后只将计数归零，并不清空数组引用；这会延长闭包存活，堆图表现得像泄漏

## 判断方法

1. 看 allocation flame graph / JFR 的对象类型，而不是只看源码行：若是 `绿藻池$...$1`、`FunctionN`、`Cons`、数组，根因是闭包或任务缓存；若没有对象分配记录，`get()` 无罪
2. 强制 GC 后 retained heap 回落：分配抖动或延迟回收；持续不回落：再沿 GC roots 检查 `SglDraw.DrawTask.tasks[]`
3. 临时将 `get()` 结果缓存到局部变量不会消除闭包分配；应把绘制 lambda 提升为 `DrawBlock` 字段，并只通过 `target` 读取建筑状态

## 修复方向

- 将 `drawCell` 和传给 `drawTask` 的回调做成非捕获、可复用字段
- 将 `cap` 改为从回调参数 `e.block.liquidCapacity` 获取，避免捕获 `draw()` 局部变量
- 必要时在 `DrawTask.flush()` 后清空已用的 `tasks[]` 与 `dataTarget[]`，避免闭包跨帧被旧数组槽位持有
- `Angles.randLenVectors` 内层 lambda若仍捕获 `b/move/cap`，同样需要复用回调或改成显式循环

## Related

- [Mindustry Building/Tile/Power/Liquid 源码验证](mindustry-building-tile-power-liquid.md)
- [Arc Time & Timer](arc-time-timer.md)
