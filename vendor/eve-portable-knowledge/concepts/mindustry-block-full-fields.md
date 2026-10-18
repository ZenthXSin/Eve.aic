# Mindustry Block.java 完整字段（源码验证）

> Source: `core/src/mindustry/world/Block.java`，394行，v8

## 1. 模块标志（控制哪些子系统激活）

```java
boolean hasItems, hasLiquids, hasPower;       // 默认 false
boolean outputsLiquid = false;                 // 输出液体标志
boolean consumesPower = true;                  // 默认 true，即使无电力模块
boolean outputsPower, conductivePower = false;
boolean connectedPower = true;                 // 电力节点能否连接
boolean outputsPayload = false;
boolean acceptsUnitPayloads = false;
boolean acceptsPayload = false;
boolean acceptsItems = false;                  // 视觉混合用
```

## 2. 物品/液体容量

```java
int itemCapacity = 10;                        // 每种物品容量
float liquidCapacity = -1f;                   // <0 时用 ConsumeLiquid 的 max 值
float liquidPressure = 1f;
boolean separateItemCapacity = false;         // 每种物品独立容量而不是总和
boolean displayFlow = true;
```

## 3. 配置系统

```java
Object lastConfig;                            // 上次配置值
boolean saveConfig = false;                   // 新放置建筑继承上次配置
boolean copyConfig = true;
boolean configurable;                         // 是否可配置
boolean logicConfigurable = false;
boolean clearOnDoubleTap = false;
```

## 4. 物理/碰撞属性

```java
boolean update;                               // 是否有 tile entity 更新
boolean destructible;
boolean solid, solidifes;
boolean rotate;
boolean rotateDraw = true;
boolean teamPassable;
boolean underBullets;                         // 不会被子弹瞄准
boolean placeableLiquid = false;             // 可在任何液体上放置
boolean requiresWater = false;               // 只能在水中放置
boolean placeablePlayer = true;
boolean placeableOn = true;
boolean floating = false;                    // 可在液体边缘放置
boolean absorbLasers = false;
boolean insulated = false;
boolean fillsTile = true;                    // 缓存渲染时是否覆盖地面
boolean forceDark = false;
```

## 5. 大小/尺寸/视觉

```java
int size = 1;                                 // 多格大小
float offset = 0f;
float clipSize = -1f;                         // <0 自动计算
boolean squareSprite = true;
boolean drawCracks = true;
boolean drawTeamOverlay = true;
boolean drawArrow = true;
boolean drawLiquidLight = true;
boolean enableDrawStatus = true;
boolean drawDisabled = true;
int variants = 0;
boolean hasShadow = true;
boolean customShadow = false;
```

## 6. 生命值/防护

```java
float scaledHealth = -1;                      // 每格血量，-1 用 40
int health = -1;                               // 总血量，>0覆盖scaledHealth
float armor = 0f;
float baseExplosiveness = 0f;
float explosivenessScale = 1f;
float flammabilityScale = 1f;
float baseShake = 3f;
float crushDamageMultiplier = 1f;
boolean crushFragile = false;
```

## 7. 电力块特有

```java
// 见 PowerBlock.java:
// update = true, solid = true, hasPower = true
// group = BlockGroup.power
```

## 8. 建造/拆解

```java
ItemStack[] requirements = {};
Category category = Category.distribution;    // 默认是 distribution!
float buildTime = -1f;                         // -1 动态计算
BuildVisibility buildVisibility = BuildVisibility.hidden;
float buildCostMultiplier = 1f;
float deconstructThreshold = 0f;
boolean instantDeconstruct = false;
boolean instantBuild = false;
boolean rebuildable = true;
boolean ignoreBuildDarkness = false;
Sound placeSound = Sounds.unset;
Sound breakSound = Sounds.unset;
Sound destroySound = Sounds.unset;
```

## 9. BlockGroup / BlockFlag / BlockStatus

```java
BlockGroup group = BlockGroup.none;           // 同group可互相替换
EnumSet<BlockFlag> flags = EnumSet.of();
BlockStatus 6种：none(白) / active(绿) / noInput(黄) / noOutput(橙) / logicDisable(紫) / inactiveUnitFactory(特殊灰)
```

## 10. 关键误区

- **`category` 默认是 `Category.distribution`**，不是 `turret` 或 `crafting`
- **`consumesPower` 默认 true**，即使 `hasPower=false`（电力节点靠此判断是否可连接）
- **`liquidCapacity = -1f`**，不是 0，`init()` 中会根据 ConsumeLiquid 自动设置
- **`scaledHealth = -1`** → 实际用 40 * size²，`health > 0` 才覆盖
- **`buildVisibility = BuildVisibility.hidden`** 默认隐藏，需要显式设置
- **`group = BlockGroup.none`**，不是 `power` 或 `walls`
- `BlockGroup` 与 `Category` 是**两个不同枚举**，前者控制替换逻辑，后者控制UI分类
- `BlockFlag` 用于AI索引，`EnumSet.of()` 默认空集

## 相关知识页

- [Building/Tile/Power/Liquid 源码验证](mindustry-building-tile-power-liquid.md)
- [内容类型解析链](mindustry-content-type-resolution.md)
- [炮塔系统源码验证](mindustry-turret-system.md)

## 电力系统关键规则（源码验证）

### hasPower 设置机制
```java
// ConsumePower.apply() 自动设置
block.hasPower = true;

// 或 outputsPower 时设置
if(outputsPower) hasPower = true;
```

### 关键默认值
| 字段 | 默认值 | 说明 |
|------|--------|------|
| `hasPower` | **@NoPatch** | JSON无法修改，由ConsumePower.apply()自动设置 |
| `consumesPower` | true | 即使无电力模块也默认true |
| `outputsPower` | false | - |
| `connectedPower` | true | 电力节点能否连接 |

### 常见误区
- ❌ `hasPower` 需要手动设置 → ✅ 由 `ConsumePower.apply()` 自动设置
- ❌ `hasPower` 可以通过JSON修改 → ✅ `@NoPatch` 注解，JSON无法修改
- ❌ `consumesPower` 默认false → ✅ 默认true

## Block 关键默认值（源码验证）

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `size` | 1 | 方块尺寸 |
| `health` | -1 | 自动计算 |
| `armor` | 0f | 护甲 |
| `category` | Category.distribution | 默认分类 |
| `buildVisibility` | BuildVisibility.hidden | 默认隐藏 |
| `update` | false | 是否更新 |
| `destructible` | false | 是否可破坏 |
| `solid` | false | 是否实体 |
| `rotate` | false | 是否可旋转 |
