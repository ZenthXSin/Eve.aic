# Mindustry 生产类方块源码验证

> Source: `core/src/mindustry/world/blocks/production/`，v8

## 1. GenericCrafter（`production/GenericCrafter.java`，374行）

**字段**：
```java
ItemStack outputItem;              // 单物品输出，写入 outputItems[0]
ItemStack[] outputItems;           // 多物品输出
LiquidStack outputLiquid;          // 单液体输出
LiquidStack[] outputLiquids;       // 多液体输出
int[] liquidOutputDirections = {-1}; // 液体输出方向，-1=全方向
boolean dumpExtraLiquid = true;
boolean ignoreLiquidFullness = false;
float craftTime = 80;              // 制造耗时(ticks)
Effect craftEffect = Fx.none;
Effect updateEffect = Fx.none;
float updateEffectChance = 0.04f;
float warmupSpeed = 0.019f;
DrawBlock drawer = new DrawDefault();
```

**构造**：`update=true, solid=true, hasItems=true, sync=true, drawArrow=false, flags=factory`

**注意**：`init()` 中若 `outputItems==null && outputItem!=null` 则写回；`outputsLiquid` 由 `outputLiquids!=null` 决定；`hasItems/hasLiquids` 在 `init/afterPatch` 中自动设为 `true`

## 2. Drill（`production/Drill.java`，402行）

**字段**：
```java
int tier;                        // 可挖掘最大硬度
float drillTime = 300;           // 挖掘1个的基础时间(frames)
float hardnessDrillMultiplier = 50f;
float liquidBoostIntensity = 1.6f;
float warmupSpeed = 0.015f;
Item blockedItem;
Seq<Item> blockedItems;
boolean drawMineItem = true;
Effect drillEffect = Fx.mine;
float drillEffectChance = 0.02f;
float rotateSpeed = 2f;
ObjectFloatMap<Item> drillMultipliers; // 每物品速度倍率
```

**构造**：`update=true, solid=true, group=drills, hasLiquids=true, hasItems=true, envEnabled|=Env.space`

**关键逻辑**：
- `getDrillTime(Item)` = `(drillTime + hardnessDrillMultiplier * item.hardness) / drillMultipliers.get(item, 1f)`
- `canMine(Tile)`: drops != null && drops.hardness <= tier && !blocked
- `countOre()` 统计范围内矿石，按优先级排序（lowPriority=false优先）
- `liquidBoostIntensity` 平方是加速倍率，不是线性

## 3. Pump（`production/Pump.java`，199行，extends LiquidBlock）

**字段**：
```java
float pumpAmount = 0.2f;         // 每格泵量
float consumeTime = 60f * 5f;    // 物品消耗间隔
float warmupSpeed = 0.019f;
DrawBlock drawer = new DrawMulti(new DrawDefault(), new DrawPumpLiquid());
```

**构造**：`group=liquids, floating=true, envEnabled=terrestrial`
**输出统计**：`60f * pumpAmount * size * size` (liquid/sec)

## 4. SolidPump（`production/SolidPump.java`，167行，extends Pump）

**字段**：
```java
Liquid result = Liquids.water;
float rotateSpeed = 1f;
float baseEfficiency = 1f;
Attribute attribute;              // 属性加成
```

**构造**：`hasPower=true, envEnabled=terrestrial`
**关键**：`canPump()` 检查 `!tile.floor().isLiquid`（固体地面可泵）
**更新**：`maxPump = min(liquidCapacity - typeLiquid(), pumpAmount * delta() * fraction * efficiency)`
**fraction**：`max(validTiles + boost + (attribute==null ? 0 : attribute.env()), 0)`

## 5. Fracker（`production/Fracker.java`，47行，extends SolidPump）

**字段**：
```java
float itemUseTime = 100f;
```
**构造**：`hasItems=true, envRequired |= Env.groundOil`
**逻辑**：`accumulator >= itemUseTime -> consume(); 累加器减掉；然后super.updateTile()`

## 6. Separator（`production/Separator.java`，203行）

**字段**：
```java
ItemStack[] results;             // 随机输出（概率 = amount/sum）
float craftTime;
DrawBlock drawer = new DrawDefault();
```

**构造**：`update=true, solid=true, hasItems=true, hasLiquids=true, sync=true`
**随机逻辑**：`Mathf.randomSeed(seed++, 0, sum-1)` 按 result.amount 权重随机
**注意**：`seed` 在 `created()` 中初始化，不同客户端会不同导致 desync

## 7. HeatCrafter（`production/HeatCrafter.java`，81行，extends GenericCrafter）

**字段**：
```java
float heatRequirement = 10f;
float overheatScale = 1f;
float maxEfficiency = 4f;
```
**接口**：`HeatConsumer`，需相邻 heater 块提供热量
**效率**：`efficiencyScale() = min(clamp(heat/req) + over/req * overheatScale, maxEfficiency)`
**注意**：`shouldConsume()` 除了父类条件外还需要 `heat > 0`

## 关键误区

- **Drill.getDrillTime** 公式是 `(drillTime + hardnessDrillMultiplier * hardness) / multiplier`，不是简单的 `drillTime / multiplier`
- **liquidBoostIntensity** 的平方才是实际加速倍率，不是线性 1.6x 每秒
- **Pump** 只输液体，不输出物品，`pumpAmount` 是每格每秒量
- **SolidPump** `canPump` 检查 `!isLiquid`，与 Pump 相反
- **Separator** 使用 `Mathf.randomSeed()` 基于位置种子，各客户端可能不同步
- **HeatCrafter** 效率可达 `maxEfficiency(4f)`，超过 heatRequirement 的部分按 `overheatScale` 额外加成
- **Fracker** 额外 `envRequired |= Env.groundOil`，无石油环境无法工作

## 相关知识页

- [GenericCrafter 增加载荷输入/输出](mindustry-genericcrafter-payload-io.md)
- [Building/Tile/Power/Liquid 源码验证](mindustry-building-tile-power-liquid.md)
- [Block 完整字段](mindustry-block-full-fields.md)