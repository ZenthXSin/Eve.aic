# Mindustry 防御类方块源码验证

> Source: `core/src/mindustry/world/blocks/defense/`，v8

## 1. Wall（`defense/Wall.java`，140行，extends Block）

**字段**：
```java
float lightningChance = -1f;       // 受击闪电概率，-1 禁用
float lightningDamage = 20f;
int lightningLength = 17;
Color lightningColor = Pal.surge;
Sound lightningSound = Sounds.shootArc;
float chanceDeflect = -1f;         // 子弹反弹概率，-1 禁用
boolean flashHit;                   // 受击闪白
Color flashColor = Color.white;
Sound deflectSound = Sounds.none;
```

**构造**：`solid=true, destructible=true, group=walls, buildCostMultiplier=6f, canOverdrive=false, drawDisabled=false, crushDamageMultiplier=5f, priority=TargetPriority.wall, envEnabled=Env.any`

**关键**：反弹概率按 `chanceDeflect / bullet.damage()` 计算，大伤害更难反弹

## 2. ShieldWall（`defense/ShieldWall.java`，128行，extends Wall）

**字段**：
```java
float shieldHealth = 900f;
float breakCooldown = 60f * 10f;   // 破碎后冷却(ticks)
float regenSpeed = 2f;             // 恢复速度
Color glowColor;
```

**构造**：`update=true`
**逻辑**：shield 值实时变化，`breakTimer` 控制破碎状态

## 3. ForceProjector（`defense/ForceProjector.java`，361行）

**字段**：
```java
float phaseUseTime = 350f;
float phaseRadiusBoost = 80f;
float phaseShieldBoost = 400f;
float radius = 101.7f;             // 护盾半径(世界单位)
int sides = 6;                     // 多边形边数
float shieldRotation = 0f;
float shieldHealth = 700f;
float cooldownNormal = 1.75f;      // 正常冷却
float cooldownLiquid = 1.5f;       // 液体冷却倍率
float cooldownBrokenBase = 0.35f;  // 破碎后基础冷却
float coolantConsumption = 0.1f;
boolean consumeCoolant = true;
float crashDamageMultiplier = 2f;
```

**构造**：`update=true, solid=true, group=projectors, hasPower=true, hasLiquids=true, hasItems=true, envEnabled|=Env.space, flags=shield`

**关键**：
- `realRadius() = (radius + phaseHeat * phaseRadiusBoost) * radscl`
- 护盾吸收子弹：`buildup += bullet.type.shieldDamage(bullet)`
- 破碎条件：`buildup >= shieldHealth + phaseShieldBoost * phaseHeat`
- 冷却：`buildup -= delta() * scale`（scale 受冷却液加成）
- `shieldConsumer` 使用 `Intersector.isInRegularPolygon` 进行多边形命中检测

## 4. MendProjector（`defense/MendProjector.java`，175行）

**字段**：
```java
float reload = 250f;
float range = 60f;                 // 治疗范围(世界单位)
float healPercent = 12f;           // 每次治疗百分比
float phaseBoost = 12f;            // 相位物品额外百分比
float phaseRangeBoost = 50f;       // 相位物品额外范围
float useTime = 400f;
```

**构造**：`solid=true, update=true, group=projectors, hasPower=true, hasItems=true, emitLight=true, suppressable=true, envEnabled|=Env.space, flags=blockRepair`

**关键**：
- `indexer.eachBlock` 遍历范围内受损建筑，`heal(maxHealth * (healPercent + phaseHeat * phaseBoost) / 100f * efficiency)`
- `suppressable = true` 可被抑制弹压制
- `phaseHeat = lerpDelta(phaseHeat, optionalEfficiency, 0.1f)`

## 5. OverdriveProjector（`defense/OverdriveProjector.java`）

**典型字段**（非完整验证）：
```java
float range = 80f;
float speedBoost = 1.6f;
float phaseRangeBoost = 40f;
float useTime = 300f;
boolean hasBoost = true;
```

## 关键误区

- **Wall.chanceDeflect** 默认 -1（禁用），非 0；触发时伤害越大概率越低（`chanceDeflect / damage`）
- **Wall.lightningChance** 默认 -1（禁用）
- **ForceProjector.radius** 是 101.7（约 12.7 tiles），不是整数
- **ForceProjector** `coolantConsumer` 使用 `ConsumeCoolant`，需 `consumeCoolant=true`（默认 true）
- **ShieldWall** 有 `update=true`，Wall 默认无 update
- **MendProjector** `suppressable = true`（可被抑制），ForceProjector 无此标记

## 相关知识页

- [炮塔系统源码验证](mindustry-turret-system.md)
- [Building/Tile/Power/Liquid 源码验证](mindustry-building-tile-power-liquid.md)