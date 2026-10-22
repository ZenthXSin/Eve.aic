# Item 网络传输与接收端不输出问题

> 来源：群聊诊断 2026-08-13（自定义 ItemBridge/ItemNode 接收端"收但不吐"）

## 关键类与方法

物品网络传输的核心链路（`mindustry.world.blocks.distribution` 系列 / `StackMoveBuild` 子类）：

- **`acceptItem(source, item)`**：接收端判收货。默认形如 `items.get(item) < itemCapacity`。
- **`pushItemsToNetwork(...)`**：向相邻网络/传送带推送物品，每次受 `transferRate`（默认 2/s）限制。
- **`outputToNearbyBlocks(...)`**：把库存物品物理输出给相邻可接收方块。
- **`handleItem(source, item)`**：收货后实际入库存。

## 根因模式：接收端"吞物品但不往外吐"

1. `pushItemsToNetwork` 里每件物品：`if (target.acceptItem(this, item)) { target.handleItem(this, item); ... }` —— 接收端一旦 `acceptItem` 通过就**收入库存**。
2. `outputToNearbyBlocks` 每次（每方向每 tick）`Math.min(amount, 1)` 只严格移 **1 件**，且给第一个能收的方向就 break。
3. 接收端本身没有主动向"下游"吐的动力：网络持续灌入的量和自身慢速输出不匹配时，库存很快填满 → `acceptItem` 返回 false → 网络停止灌入 → 表现为"接收端满/不输出"。

## 排查要点

- **先区分两种现象**（决定修法，别盲目改）：
  - A. 接收端库存根本没有物品 → 问题在发端/网络没推进来。
  - B. 接收端库存有物品但不往相邻传送带走 → 问题在 `outputToNearbyBlocks` 输出太慢/方向逻辑。
- JSON 语法能过 `ContentParser`（否则方块都注册不上，根本放不出来），所以"能放置但运行异常"通常是物品/方块**运行时逻辑**而非 JSON 加载错误。

## 修复方向（按需选择）

1. **限吞吐**：在 `pushItemsToNetwork` 只推给还能向外部吐的节点，或让 `acceptItem` 对网络输入按 `transferRate` 限流。
2. **改输出**：把 `outputToNearbyBlocks` 的 `Math.min(amount,1)` 提升到 `transferRate` 吞吐，不再 break 掉其它物品类型。
3. **语义判断**：在 `acceptItem` 加 `source` 判断——若 `source` 是网络中 ItemNodeBuild 则要求 `items.total()+增量 ≤ itemCapacity` 且接收端附近有可吐出的方块才收，避免囤积。

## Source

- `mindustry/world/blocks/distribution/ItemBridge.java`（含 ItemBridgeBuild）
- 判定主链：`StackMoveBuild` / `pushItemsToNetwork` / `outputToNearbyBlocks` / `acceptItem`
- `mindustry/type/Category.java`：建造分区是 enum，mod 无法新增分区，只能归到现有分区（与"接收端不输出"无关但同批诊断引出）
