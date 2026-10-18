# Mindustry 物流/液体/存储系统源码验证

> Source: `distribution/` 3911行 + `liquid/` 500行 + `storage/` 1297行，v8

## 物流方块（distribution/）

### Conveyor（490行）

**字段**：
```java
float speed = 0f;                 // 移动速度
float displayedSpeed = 0f;        // UI 显示速度
boolean pushUnits = true;         // 推送单位
@Nullable Block junctionReplacement, bridgeReplacement;
```

**构造**：`rotate=true, update=true, group=transportation, hasItems=true, itemCapacity=3, priority=transport, conveyorPlacement=true, underBullets=true, unloadable=false, noUpdateDisabled=false`

**关键机制**：
- 内部并行数组：`ids[3]`/`xs[3]`/`ys[3]`，`len < 3`
- `blendbits` / `blending`：Autotiler 邻接融合状态
- `clogHeat`：堵塞度，`>0.5` 时停止推单位 + 换帧
- `updateTile()`：若 `len==0 && timeScale==1` → `sleep()` 跳过更新
- `acceptItem`：从正面（direction==0）要求 `minitem >= itemSpace`；从侧面要求 `minitem > 0.7`
- 侧插：`xs=±1, ys=0.5, mid` 位置（不从头开始）
- 物品位置 `ys[i]` 从 0→1 表示行程进度
- `clogHeat` 计算：`minitem < itemSpace + (blendbits==1?0.3:0)` → 逼近 1
- `getReplacement`：十字交叉时自动替换为**Junction**
- `overwrote()`：继承旧传送带物品状态（ids/xs/ys/len/clogHeat）

### Router（129行）

```java
float speed = 8f;                 // 分发速度（帧/物品）
```

**构造**：`solid=false, underBullets=true, update=true, hasItems=true, itemCapacity=1, group=transportation, unloadable=false, noUpdateDisabled=true`

**关键**：
- **轮流分发**：`rotation` 做计数器，`(i + counter) % proximity.size`
- 单物品缓存：同时只能 hold 1 个物品
- `acceptItem`：`team == source.team && lastItem == null && items.total() == 0`
- 支持 `ControlBlock`（`BlockUnitc`），可被玩家控制
- `getTileTarget` 跳过 `from == input`（防回流）且跳过 `OverflowGate`
- `acceptStack` 返回 0（不能接受堆叠）

### Junction（110行）

```java
float speed = 26;                  // 通过时间（帧）
int capacity = 6;                  // 每方向缓存容量
float displayedSpeed = 13f;        // UI 显示
```

**构造**：`update=true, solid=false, underBullets=true, group=transportation, unloadable=false, noUpdateDisabled=true`

**关键**：
- 4 方向独立缓存：`DirectionalItemBuffer buffer`
- `acceptItem`：按 `source.relativeTo(tile)` 存到对应方向队列
- 每个 tick 遍历 4 方向，倒计时到则送到 `nearby(i)`
- 如果目标不接受则阻塞等待，不丢物品

### Sorter（173行）

```java
boolean invert;                    // 反转模式
```

**构造**：`update=false, destructible=true, underBullets=true, instantTransfer=true, configurable=true, unloadable=false, saveConfig=true, clearOnDoubleTap=true`

**关键**：
- `instantTransfer=true`：物品瞬间通过
- `update=false`：无 updateTile
- 匹配物品 → 直行，不匹配 → 向两侧分流（轮流）
- 特殊限制：`!(block.instantTransfer && source.block.instantTransfer)` 防止 3 链直连
- `getTileTarget` 返回目标建筑，`handleItem` 直接 `getTileTarget(...).handleItem(this, item)`

### OverflowGate（99行）

```java
float speed = 1f;
boolean invert = false;
```

**构造**：`hasItems=true, update=false, destructible=true, instantTransfer=true, unloadable=false, canOverdrive=false, itemCapacity=0`

**关键**：
- `update=false, itemCapacity=0`：无缓冲，瞬间传递
- 逻辑：前方可接受 → 走前方；前方不可或 invert → 左右分流
- `relativeToEdge` 确定来源方向

### ItemBridge（533行）

**字段**：
```java
int range;                         // 范围
float transportTime;               // 传输时间
boolean fadeIn = true;
boolean moveArrows = true;
boolean pulse = false;
float bridgeWidth = 6.5f;
float arrowSpacing = 4f;
```

**构造**：`update=true, solid=true, underBullets=true, hasPower=true, itemCapacity=10, configurable=true, hasItems=true, unloadable=false, noUpdateDisabled=true, allowDiagonal=false, copyConfig=false, allowConfigInventory=false`

**关键**：
- 使用 `link = pos()` 配置，双向验证 `linkValid`
- `updateTransport` 基于 `transportCounter += edelta()`
- `warmup` 标签渐入效果，`hasPower` 标识供
- `shouldConsume()` = `linkValid && enabled`
- `checkIncoming` 清理失效 incoming 列表
- 绘图效果：箭头脉冲 + 桥体渐变

### Duct（274行）

```java
float speed = 5f;                  // 通过时间（帧）
boolean armored = false;
```

**构造**：`update=true, solid=false, hasItems=true, itemCapacity=1, noUpdateDisabled=true, underBullets=true, rotate=true, noSideBlend=true, isDuct=true, envEnabled=空间|陆地|水下`

**关键**：
- 单物品管道，`current` 表示当前持有物品
- `progress` 从 0→1 表示传输进度
- `recDir` 记录来源方向，用于内部绘制插值
- 装甲版（`armored`）限制接收方向
- 标准版不接受前方传入（`!(source.block.rotate && next == source)`）

### MassDriver（381行）

**字段**：
```java
float range;                       // 范围
float rotateSpeed = 5f;
float translation = 7f;
int minDistribute = 10;            // 最小发射量
float knockback = 4f;
float reload = 100f;               // 装填时间（帧）
float bulletSpeed = 5.5f;
float bulletLifetime = 200f;
```

**构造**：`update=true, solid=true, configurable=true, hasItems=true, hasPower=true, sync=true, envEnabled|=space`

**状态机**（`DriverState`）：
- `idle` → 无目标/无射手
- `accepting` → 接收子弹中
- `shooting` → 锁定目标

**关键**：
- 双端对准：发射端与接收端同时旋转对准
- 最小传输量 `minDistribute=10`
- 接收缓冲 `itemCapacity * 2` 上限
- `waitingShooters` 队列（OrderedSet），保证按序发射
- 子弹由 `MassDriverBolt` 实现，命中后调用 `handlePayload`

### 其他方块

| 类 | 行数 | 关键 |
|----|------|------|
| StackConveyor | 403 | 堆叠传送带，物品堆叠传输 |
| DirectionBridge | 254 | 方向桥梁 |
| BufferedItemBridge | 67 | 缓冲桥 |
| DirectionalUnloader | 161 | 方向卸货器 |
| StackRouter | 91 | 堆叠路由器 |
| DuctRouter | 207 | 管道路由器，有 itemCapacity |
| DuctJunction | 179 | 管道连接器 |
| DuctBridge | 69 | 管道桥 |
| OverflowDuct | 159 | 溢流管道 |
| DirectionLiquidBridge | 91 | 方向液体桥 |

## 存储方块（storage/）

### StorageBlock（140行）

```java
boolean coreMerge = true;          // 与核心合并
```

**构造**：`hasItems=true, solid=true, update=false, sync=true, destructible=true, separateItemCapacity=true, group=transportation, flags={storage}, allowResupply=true, envEnabled=any`

**关键**：
- `coreMerge=true`：自动检测附近核心，所有物品处理转发给核心
- `linkedCore != null` 时：`acceptItem/handleItem/removeStack` 全部委托给核心
- `getMaximumAccepted`：linked 时返回核心容量
- `explosionItemCap`：linked 时 `min(itemCapacity/60, 6)`，大幅降低爆炸伤害
- `canPickup()`：linked 时 false
- `outputsItems()` 返回 false——存储块不出物品

### Unloader（275行）

```java
float speed = 1f;                  // 帧/次
boolean allowCoreUnload = true;
```

**构造**：`update=true, solid=true, health=70, hasItems=true, configurable=true, saveConfig=true, itemCapacity=0, noUpdateDisabled=true, clearOnDoubleTap=true, unloadable=false`

**关键**：
- `itemCapacity=0`：不缓存物品
- `possibleBlocks` 通过 `onProximityUpdate()` 构建
- 排序器按 `(notStorage) > (canUnload && !canLoad) > (canUnload || !canLoad) > loadFactor > lastUsed`
- 无 `sortItem`（nulloader）时遍历 allItems 轮询（`rotations` 起始）
- 每次只移动 1 个物品

### CoreBlock（882行，已有知识页）

见 `knowledge/concepts/mindustry-campaign-unitfactory-objectives.md`

## 液体方块（liquid/）

### LiquidBlock（89行）

**构造**：`update=true, solid=true, hasLiquids=true, group=liquids, outputsLiquid=true, envEnabled|=space|underwater`

**关键**：
- `drawTiledFrames`：多格液体纹理拼接，按 size 逐个绘制
- `getAnimationFrame()` 获取动画帧

### Conduit（253行）

**构造**：
```java
rotate = true;
solid = false;
floating = true;                   // 可放在液体边缘
underBullets = true;
conveyorPlacement = true;
noUpdateDisabled = true;
canOverdrive = false;
priority = TargetPriority.transport;
```

**关键**：
- `Autotiler`：邻接混合贴图
- `capped`：前方无液体块时封口；`backCapped` 后方
- `acceptLiquid`：不接受来自自身输出方向（`(rot + 2) % 4 != rotation`）的液体
- `moveLiquidForward`：每 1 tick 向前推动
- `smoothLiquid`：平滑动画插值
- `sleep()`：无液体时跳过更新

### ArmoredConduit（29行）

继承 Conduit，护甲更高

### LiquidBridge（33行）

短程液体桥，继承方向逻辑

### LiquidJunction（51行）

液体交叉口，方向独立缓冲

### LiquidRouter（45行）

液体路由器，简单分发

## 高频误区总结

1. **Conveyor `updateTile` 可 sleep()**：`len==0 && timeScale==1` 时跳过，不每帧更新
2. **Conveyor `clogHeat`** 阈值 `0.5`：超过停止推单位和动画
3. **Router `itemCapacity=1`**：只能 hold 1 个物品
4. **Router `acceptStack` 返回 0**：不能作为堆叠目标
5. **Junction 每方向独立缓存**（DirectionalItemBuffer），不是全局队列
6. **Sorter `update=false` + `instantTransfer=true`**：纯传递无更新
7. **OverflowGate `itemCapacity=0` + `update=false`**：纯瞬时传递
8. **ItemBridge `transportCounter += edelta()`**：带 `timeScale` 的传输累计
9. **Duct `isDuct=true` + `noSideBlend=true`**：管道特有毒株
10. **Armored Duct 限制接收方向**，标准 Duct 不接受前方
11. **MassDriver 接收端用 `itemCapacity * 2`** 做超量缓冲
12. **MassDriver `minDistribute=10`**：小于此量不发射
13. **StorageBlock `update=false`**：不更新，纯存储
14. **StorageBlock `coreMerge=true`** linked 时委托核心，很弱
15. **Unloader `itemCapacity=0`**：无物品缓存
16. **Unloader `possibleBlocks` 排序器**：9 层优先级排序
17. **Conduit `floating=true`**：可液体边缘放置
18. **Conduit `canOverdrive=false`**：不可超速
19. **Conduit 液体推动每 1 tick 一次**（`timerFlow`）
20. **Conduit 封口**：`capped` / `backCapped` 两个方向独立