# Mindustry HeatNode 连接配置与快速热交换

> Source: conversation + Kotlin source patch for `HeatNode`

这类 `HeatNode` 的连接失效，常见原因是：

- `config(Point2)` / `config(Int)` 直接写入 `link`，没有在写入前重新校验目标 tile 是否仍然合法。
- 配置点击时使用了默认 `linkValid(tile, other.tile)`，它会做“双向已连接”检查，导致点中一个已经指向当前方块的节点时，重新切换/接管连接容易失败。
- `config()` 在 `link == -1` 时直接 `Point2.unpack(-1)`，可能产生异常配置偏移或无效状态。

## 修复思路

### 1. 配置时只写入合法连接

`config(Point2)`：把相对坐标换算成目标 tile 后，先 `linkValid(build.tile, target)`，合法才写入 `target.pos()`，否则回退到 `-1`。

`config(Int)`：

- `-1` 直接表示断开
- 其他值先转 tile，再校验合法性后写入

这样可以避免脏存档、非法目标或点击无效坐标导致的连接状态错误。

### 2. 手动点击连接时放宽“双向检查”

在 `onConfigureBuildTapped` 中，对手动点击目标使用 `linkValid(tile, other.tile, false)`。

含义是：

- 允许玩家点中一个已经连向自己的 HeatNode
- 当前节点可以顺利接管连接，随后把对方清空
- 再次点击已连接目标时可直接断开

这比默认的“禁止双向连接”判断更适合交互层。

### 3. 无连接时返回空偏移

`config()` 在 `link == -1` 时返回 `Point2()`，不要解包 `-1`。

## 偏游戏性的热交换简化

目标不是严格物理模拟，而是：

- 玩家更容易理解
- 升温/降温反馈更快
- 性能和稳定性更好

### 简化规则

统一采用：

`change = (other.temperature - temperature) * rate * Time.delta`

特点：

- 只看温差，不做更复杂的中间状态运算
- 每次交换都向温度均衡靠拢
- 调参时只需要改 `rate`

### 推荐速率

桥接传热：

- `bridgeRate = clamp(min(conductivityA, conductivityB) / 200f, 0.04f, 0.18f)`

邻接传热：

- `adjacentRate = clamp(conductivity / 400f, 0.015f, 0.08f)`

这样会比原先 `0.18f` / `0.08f` 的乘法方案更直观：

- 数值随导热率变化，但被限定在可控区间
- 游戏内反馈更快，但不至于一帧拉平

## 实现注意点

- 仍保留 `if (pos() >= other.pos()) return`，避免双边重复结算。
- 交换后双方都应 `clampTemperature`。
- 一端发生热交换时，可同时把双方 `moved = true`，便于动画/状态反馈一致。
- 链接目标失效时，可在 `updateTile()` 中把 `link` 自动清成 `-1`，避免残留无效连接。

## Related

- [Mindustry 电力系统持久化](../concepts/mindustry-power-system-persistence.md) — 连接类结构在存档/恢复时的状态处理
- [Mindustry Building 与 Tile 坐标关系](../concepts/mindustry-building-tile-coordinate-relationship.md) — 配置与连接判断依赖 Tile/Building 坐标关系
