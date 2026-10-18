# Mindustry GenericCrafter 增加载荷输入/输出

## 结论
`GenericCrafter` 本身不直接支持 payload 作为标准 consume/produce 项，但可以通过**继承自定义 block**，在 `Build` 内补充 payload 的接收、缓存、投递逻辑，实现“像工厂一样吃/吐载荷”的方块。

## 适用场景
- 载荷作为一种生产材料输入
- 产物需要以 payload 形式输出给相邻方块
- 想复用 `GenericCrafter` 的配方、进度、效率、状态机

## 推荐实现思路
1. 继承 `GenericCrafter`
2. 在内部继承 `GenericCrafterBuild`
3. 增加：
   - 输入 payload 缓存
   - 输出 payload 缓存
   - 接收判断
   - 投递判断
   - 更新逻辑
4. 在 `updateTile()` / `moveOutPayload()` / `acceptPayload()` / `handlePayload()` 一类路径中处理 payload 流程

## 关键点
- **不要**把 payload 当成 `Item` 或 `Liquid` 直接塞进原有 consume 体系
- payload 的交互通常围绕 `Payloadc`、`PayloadBlock`、`canPickup`、`acceptPayload`、`handlePayload` 等接口/方法展开
- 如果要与邻接方块协作，最好显式实现“谁能接收 payload”的判定
- 输出时要注意：
  - 是否占用 `dump` 方向
  - 是否需要等待目标建筑空闲
  - 是否允许多 payload 堆积

## 设计建议
- 输入端：把载荷视为“原料包”，进场后存入 `payloadIn`
- 生产端：当进度完成后，把结果写入 `payloadOut`
- 输出端：优先尝试向前方/邻接建筑投递；失败则暂存

## 风险
- payload 逻辑比 item/liquid 更依赖建筑交互，容易和原有 `GenericCrafterBuild` 更新顺序冲突
- 如果只想表达“高级输入输出”，且不强依赖载荷实体，可能更适合自定义 `Block` 而不是硬改 `GenericCrafter`

## 相关页面
- [Mindustry 工厂输入/输出载荷生产](mindustry-load-production.md)
- [Mindustry Payload 输入/输出关键片段](mindustry-payload-io-snippets.md)
- [Mindustry 多合成工厂](mindustry-multi-recipe-crafter.md)

## 备注
本页总结的是：**继承 `GenericCrafter` 做 payload IO 扩展** 的可行方向与实现重点。
