# Mindustry 电力系统源码验证

> Source: `core/src/mindustry/world/blocks/power/` v8，2703行

## 继承链

```
Block
  └── PowerBlock           (update, solid, hasPower, group=power)
        ├── PowerDistributor  (consumesPower=false, outputsPower=true)
        │     ├── Battery     (outputsPower=true, consumesPower=true, canOverdrive=false, update=false)
        │     └── PowerGenerator (sync=true, baseExplosiveness=5f, flags={generator})
        │           ├── ConsumeGenerator (itemDuration=120, warmupSpeed=0.05f)
        │           │     └── HeaterGenerator (rotate, rotateDraw=false, canOverdrive=false, drawArrow)
        │           ├── ThermalGenerator (noUpdateDisabled, attribute=heat)
        │           ├── SolarGenerator (flags=空集, envEnabled=any)
        │           ├── NuclearReactor (itemCapacity=30, liquidCapacity=30, rebuildable=false)
        │           ├── ImpactReactor (outputsPower=consumesPower=true, warmupSpeed=0.001f)
        │           └── VariableReactor (powerProduction=20, maxHeat=100, rebuildable=false)
        ├── PowerNode        (configurable, consumesPower=false, outputsPower=false, update=false)
        │     └── LongPowerNode (drawRange=false, glow)
        └── BeamNode         (consumesPower=outputsPower=false, underBullets, priority=transport)
```

**独立于继承链**：
- `PowerDiode → Block`（rotate, update, insulated, group=power, noUpdateDisabled）
- `LightBlock → Block`（hasPower, update, configurable, saveConfig，非 power 块）

## PowerGraph（393行）

### 结构
```java
Seq<Building> producers, consumers, batteries, all;  // 4个独立列表
WindowedMean powerBalance = new WindowedMean(60);     // 60帧滑动平均
float lastPowerProduced, lastPowerNeeded, lastPowerStored;
private int graphID;                                   // 自增唯一ID
```

### 分类规则（add()）
| 条件 | 添加到 |
|------|--------|
| `outputsPower && consumesPower && !consPower.buffered` | producers **+** consumers |
| `outputsPower && consumesPower`（即 buffered） | batteries |
| `outputsPower` | producers |
| `consumesPower && consPower != null` | consumers |

### 核心逻辑
- **电量传输**：`transferPower(float)` → 正值 `chargeBatteries`，负值 `useBatteries`
- **电池充电**（`chargeBatteries`）：按各电池剩余空间比例同步充电，全部充满为止
- **电池放电**（`useBatteries`）：按各电池当前存量比例同步放电
- **配电**（`distributePower`）：buffered 的 consumer 累积 status；非 buffered 的设 `status = coverage`
- **覆盖度**（`coverage`）：`min(1, produced / needed)`，非 buffered 的第次全量分配
- **满意度**（`getSatisfaction`）：`lastPowerProduced / lastPowerNeeded`（clamp 0~1）
- **warmup**：返回 `power.status`，图级别统一
- **BFS reflow**：从指定 tile 出发，`getPowerConnections` 遍历所有连接点重建图

### 关键常量
- `satisfaction = 0` 当 `lastPowerProduced ≈ 0`
- `satisfaction = 1` 当 `lastPowerNeeded ≈ 0`
- **幂等**：`producers.size == 0 && consumers.size == 0 && batteries.size == 0` 时不更新

## PowerNode（519行）

### 关键字段
```java
float laserRange = 6;           // 连接范围(tiles)
int maxNodes = 3;               // 最大连接数
boolean autolink = true;        // 自动连接
boolean drawRange = true;       // 显示范围
boolean sameBlockConnection = false; // 同种方块互连
float laserScale = 0.25f;
Color laserColor1 = Color.white, laserColor2 = Pal.powerLight;
```

### 构造特点
```java
consumesPower = false;
outputsPower = false;
canOverdrive = false;
update = false;                 // 不更新
destructible = true;
drawDisabled = false;
```

### 连接验证（linkValid）
不满足**任一**即不连接：
- `tile != link`（不能连自己）
- `link != null && link.block.hasPower && link.block.connectedPower`
- `tile.team == link.team`
- `!sameBlockConnection || tile.block == link.block`
- 范围检查（双向）：`overlaps(tile, link, range)` 或 `overlaps(link, tile, node.range)`
- 对方 node 连接数未满（或已包含）
- **不被绝缘**：`World.raycast` 检查 `isInsulated()`
- **不连接相邻格**的建筑（`Structs.contains(Edges.getEdges(size))`）

### 自动连接（placed）
`autolink && !net.client()` 且 `power.links.size == 0` → `getPotentialLinks`

### 绘制规则
```java
Draw.z(Layer.power);
// 每个 link 只画一次：if(link.block instanceof PowerNode && link.id >= id) continue;
// 颜色按 satisfaction 渐变
```

### 其他
- `static float maxRange`：记录全图最大 `laserRange`，用于 `getNodeLinks` 搜索
- `ignoreResizeConfig = true`，`swapDiagonalPlacement = true`
- `schematicPriority = -10`

## Battery（105行）

### 字段
```java
Color emptyLightColor = Color.valueOf("f8c266");
Color fullLightColor = Color.valueOf("fb9567");
```

### 构造
```java
outputsPower = true;
consumesPower = true;
canOverdrive = false;
update = false;                 // 电池不更新
flags = EnumSet.of(BlockFlag.battery);
```

### 运行
- `warmup()` 返回 `power.status`
- 状态判定：`status ≈ 0 → noInput`，`status ≈ 1 → active`，中间 `→ noOutput`
- `overwrote()` 继承旧电池电量：`power.status += other.status * other.capacity / this.capacity`

## BeamNode（295行）

### 字段
```java
int range = 5;                  // 射线范围(tiles)
float pulseScl = 7, pulseMag = 0.05f;
float laserWidth = 0.4f;
```

### 构造
```java
consumesPower = outputsPower = false;
allowDiagonal = false;
underBullets = true;
priority = TargetPriority.transport;
```

### 运行机制
- 4个方向独立存储：`Building[] links = new Building[4]`
- `updateTile()` 每帧检查 `world.tileChanges`，只在新变化时重新扫描
- 每个方向沿直线找**第一个有电的非 PowerNode 方块**（被绝缘块阻断则停）
- 自动连接/断开，触发 graph merge/split
- 绘制去重：`links[i].id > id && range >= node.range` 或 `range > node.range`

## ConsumeGenerator（174行）

### 关键字段
```java
float itemDuration = 120;       // 每单位燃料发电时长(ticks)
float warmupSpeed = 0.05f;
float effectChance = 0.01f;
@Nullable LiquidStack outputLiquid;
boolean explodeOnFull = false;
ObjectFloatMap<Item> itemDurationMultipliers;
```

### 运行逻辑
- `productionEfficiency = efficiency * efficiencyMultiplier`
- `generateTime -= delta() / (itemDuration * itemDurationMultiplier)`
- `generateTime <= 0f` → `consume()` + 重置 `generateTime = 1f`
- `outputLiquid` 每帧添加 `min(productionEfficiency * delta() * amount, 余量)` 并 dump
- `explodeOnFull`: `liquid >= capacity - 0.01f` → kill + 触发 GeneratorPressureExplodeEvent

## HeaterGenerator（77行）

### 构造差异
```java
rotate = true;
rotateDraw = false;             // 旋转不旋转贴图
canOverdrive = false;
drawArrow = true;
```

### 运行
- `heat = Mathf.approachDelta(heat, heatOutput * efficiency, warmupRate * delta())`
- 热量与消耗**无关**，仅由 `efficiency` 决定
- `HeatBlock` 接口实现

## ThermalGenerator（115行）

### 关键
```java
Attribute attribute = Attribute.heat;  // 默认热属性
float minEfficiency = 0f;
boolean noUpdateDisabled = true;
```

- `productionEfficiency = sum + attribute.env()`
- `sum` 在 `onProximityAdded()` 时通过 `sumAttribute()` 计算一次
- `displayEfficiencyScale` 用于 UI 显示
- `canPlaceOn` 检查 `sumAttribute > minEfficiency`

## ImpactReactor（136行）

### 构造
```java
hasPower = true;
hasLiquids = true;
liquidCapacity = 30f;
hasItems = true;
outputsPower = consumesPower = true;  // 既是生产者又是消费者
flags = {reactor, generator};
lightRadius = 115f;
warmupSpeed = 0.001f;                // 非常慢的预热
itemDuration = 60f;
```

### 运行
- **升温条件**：`efficiency >= 0.9999f && power.status >= 0.99f`（电力充足才预热）
- `productionEfficiency = pow(warmup, 5f)` — **5次方曲线**（前期极慢，后期极快）
- `warmup` 为 1 时产量达到 `powerProduction`
- 净输出 = `getPowerProduction() - consPower.usage`
- 触发事件：达到正净产出时 `Trigger.impactPower`

## NuclearReactor（196行）

### 字段
```java
float itemDuration = 120;          // 每燃料单位时长(ticks)
float heating = 0.01f;             // 每帧加热量 * fullness
float heatOutput = 15f;            // 最大热量输出
float coolantPower = 0.5f;         // 每单位冷却液带走的热量
float smokeThreshold = 0.3f;
float flashThreshold = 0.46f;
float ambientCooldownTime = 60f * 20f;  // 无燃料时自然冷却时间
int explosionRadius = 19;
int explosionDamage = 1250 * 4;
```

### 构造
```java
itemCapacity = 30;
liquidCapacity = 30;
rebuildable = false;               // 不能重建
flags = {reactor, generator};
```

### 运行
- `heat += fullness * heating * min(delta(), 4f)` — 帧上限 4
- `heat -= liquids.currentAmount() * coolantPower` — 冷却液等比例带走热量
- 无燃料时：`heat = max(0, heat - delta / ambientCooldownTime)`
- `heat >= 0.999f` → `Trigger.thoriumReactorOverheat` → kill + explode
- `shouldExplode()`：fuel >= 5 || heat >= 0.5f
- 绘制：`coolColor → hotColor` 按 heat 渐变，`flashThreshold` 以上闪烁

## VariableReactor（148行）

### 字段
```java
float maxHeat = 100f;
float unstableSpeed = 1f / 60f / 3f;  // ~0.00555/frame
float warmupSpeed = 0.1f;
```

### 运行（HeatConsumer）
- `heat = calculateHeat(sideHeat)` — 从四周接收热量
- `warmup = lerpDelta(warmup, efficiency > 0 ? 1f : 0f, warmupSpeed)`
- `updateEfficiencyMultiplier()`：
  - `target = clamp(heat / maxHeat)` — 接收到的热量占比
  - `efficiencyMet = clamp(efficiency / target)` — 冷却液满足比例
  - `instability` 趋近 0（满足时）或 1（不满足时）
  - `instability >= 1f` → kill
  - `efficiency *= target` — 按 heat 比例降效率

## PowerDiode（83行）

### 关键
```java
rotate = true;
update = true;
insulated = true;               // 绝缘，不参与电力连接
group = BlockGroup.power;
noUpdateDisabled = true;
```

### 逻辑（单向传输）
- 只有 `back → front` 方向传输电力
- 条件：`backStored/backCapacity > frontStored/frontCapacity`
- 传输量 = `(targetPercent * frontCapacity - frontStored) / 2`
- 只移动**一半**的差额

## SolarGenerator（36行）

```java
flags = EnumSet.of();  // 无 generator 标志（低优先级目标）
envEnabled = Env.any;

productionEfficiency = solarMultiplier * max(0, light.env() + (lighting ? 1 - ambientLight.a : 1));
```

## LightBlock（148行）

```java
hasPower = true;                 // 有电力模块
update = true;                   // 需要更新
configurable = true;
saveConfig = true;               // 保存颜色配置
// NOT extends PowerBlock! 直接 Block
```

- 亮度 = `brightness * efficiency`
- 颜色可配，Logic 可 `LAccess.color` 控制

## 继承链总结

| 类 | parent | 关键构造差异 |
|----|--------|------------|
| PowerBlock | Block | `update, solid, hasPower, group=power` |
| PowerDistributor | PowerBlock | `consumesPower=false, outputsPower=true` |
| Battery | PowerDistributor | `consumesPower=true, canOverdrive=false, update=false, flags={battery}` |
| PowerGenerator | PowerDistributor | `sync=true, baseExplosiveness=5f, flags={generator}` |
| ConsumeGenerator | PowerGenerator | `itemDuration=120` |
| HeaterGenerator | ConsumeGenerator | `rotate, rotateDraw=false, canOverdrive=false, drawArrow` |
| ThermalGenerator | PowerGenerator | `noUpdateDisabled` |
| SolarGenerator | PowerGenerator | `flags=空` |
| NuclearReactor | PowerGenerator | `itemCapacity=30, liquidCapacity=30, rebuildable=false` |
| ImpactReactor | PowerGenerator | `consumesPower=true, liquidCapacity=30` |
| VariableReactor | PowerGenerator | `powerProduction=20` |
| PowerNode | PowerBlock | `consumesPower=false, outputsPower=false, update=false` |
| LongPowerNode | PowerNode | `drawRange=false` |
| BeamNode | PowerBlock | `consumesPower=false, outputsPower=false, allowDiagonal=false` |

## 高频误区

1. **PowerGraph 有三个二级列表**：producers/consumers/batteries，不是 grouped in all
2. **satisfaction = lastProduced / lastNeeded**，不是即时电流
3. **Battery `warmup()` = `power.status`**，不是常规 warmup
4. **Battery `update=false`**，完全不调用 updateTile
5. **PowerNode `consumesPower=false`**，虽然 connectedPower=true
6. **PowerNode `update=false`** + `canOverdrive=false`
7. **BeamNode 跳过 PowerNode**：`!(other.block instanceof PowerNode)`
8. **ImpactReactor `warmupSpeed=0.001f`**极慢 + `pow(warmup, 5)`曲线，非简单线性
9. **ImpactReactor 升温需电力充足** (`power.status >= 0.99f`)
10. **NuclearReactor `delta()`上限 min(delta, 4)**，防止单帧跳过冷却
11. **ConsumeGenerator 的 `generateTime = 1f` 重置**，不是累积值
12. **PowerDiode 只移一半差额**，不是全部
13. **SolarGenerator `flags=空`**，无 generator 标志，AI 低优先级
14. **LightBlock 不是 PowerBlock**，直接 extends Block，无电力网络
15. **BeamNode 只在 tileChanges 变化时重新扫描**，不每帧遍历
16. **ConsumeGenerator `productionEfficiency`= `efficiency * efficiencyMultiplier`**，itemDurationMultiplier 只影响消耗频率
17. **NuclearReactor `heatOutput` = HeatBlock 输出**，不是爆炸值；爆炸由 `explosionDamage=1250*4` 决定
18. **VariableReactor 产电依赖热量输入**，不是消耗物品
