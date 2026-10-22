# Mindustry 回答准确度强化规则

> Source: 源码验证 + 知识库交叉校验，2026-06-08 更新

## 高频幻觉区域与源码事实

### 1. GenericCrafter 字段（源码验证）

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `outputItem` | `@Nullable ItemStack` | null | 单输出，写入 outputItems 单元素数组 |
| `outputItems` | `@Nullable ItemStack[]` | null | 多输出，**覆盖** outputItem |
| `outputLiquid` | `@Nullable LiquidStack` | null | 单液输出 |
| `outputLiquids` | `@Nullable LiquidStack[]` | null | 多液输出，**覆盖** outputLiquid |
| `drawer` | `DrawBlock` | `DrawDefault()` | 绘制器，不是 `draw` |
| `craftEffect` | `Effect` | `Fx.none` | 合成完成特效 |
| `updateEffect` | `Effect` | `Fx.none` | 持续更新特效 |
| `craftTime` | `float` | 80 | 合成时间(tick) |
| `warmupSpeed` | `float` | 0.019f | 预热速度 |
| `dumpExtraLiquid` | `boolean` | true | 多余液体倾倒 |
| `ignoreLiquidFullness` | `boolean` | false | 忽略液体满 |

**关键初始化**（构造函数中）：`update=true, solid=true, hasItems=true, drawArrow=false`

**load() 中的回写**：
- 若 `outputItem!=null && outputItems==null` → 自动转为单元素数组
- 若 `outputLiquid!=null && outputLiquids==null` → 同上
- 若 `outputItems!=null` → 强制 `hasItems=true`
- 若 `outputLiquids!=null` → 强制 `hasLiquids=true`

### 2. Block 关键布尔字段（源码验证）

```
hasItems=false, hasLiquids=false, hasPower=false
outputsLiquid=false, consumesPower=true, outputsPower=false
connectedPower=true, conductivePower=false
outputsPayload=false, acceptsPayload=false, acceptsItems=false
update=false, solid=false, rotate=false
floating=false, squareSprite=true, absorbLasers=false
drawArrow=true, drawTeamOverlay=true, enableDrawStatus=true
size=1, health=-1(自动计算), armor=0
```

### 3. 内容目录命名规则（源码验证）

`ContentType.name()` 小写 + 若不以s结尾则加s：
- `block` → `blocks/`
- `item` → `items/`
- `unit` → `units/`
- `liquid` → `liquids/`
- `status` → `statuses/`
- `weather` → `weathers/`
- `sector` → `sectors/`
- `planet` → `planets/`
- `team` → `teams/`
- `unitCommand` → `unitcommands/`
- `unitStance` → `unitstances/`

**易错**：`content/block/` 不会被扫描，必须是 `content/blocks/`

### 4. JSON `research` 字段（源码验证）

`readFields()` 中 `research` 被特殊处理，**不是**普通反射字段：
- 字符串形式：`"research": "parent-name"` → 仅指定父节点
- 对象形式：`"research": {"parent": "...", "requirements": [...], "objectives": [...], "planet": "...", "root": true, "name": "...", "requiresUnlock": true}`

查找父节点的逻辑（按优先级）：
1. `t.content.name.equals(researchName)`
2. `t.content.name.equals(currentMod.name + "-" + researchName)`
3. `t.content.name.equals(SaveVersion.mapFallback(researchName))`

### 5. DrawBlock 完整列表（源码验证）

`mindustry.world.draw` 包中所有 DrawBlock 子类（共 34 个）：
- DrawDefault, DrawRegion, DrawMulti, DrawMultiWeave
- DrawFlame, DrawPlasma(extends DrawFlame), DrawCrucibleFlame
- DrawArcSmelt, DrawBubbles, DrawCells, DrawCircles
- DrawBlurSpin, DrawFade, DrawFrames, DrawGlowRegion
- DrawHeatInput, DrawHeatOutput, DrawHeatRegion
- DrawLiquidOutputs, DrawLiquidRegion, DrawLiquidTile
- DrawParticles, DrawPistons, DrawPower
- DrawPulseShape, DrawPumpLiquid, DrawShape
- DrawSideRegion, DrawSoftParticles, DrawSpikes
- DrawTurret, DrawWarmupRegion, DrawWeave
- DrawBlockParts, DrawCultivator

**Drawer 字段名是 `drawer`，不是 `draw`**

### 6. BulletType 完整列表（源码验证）

`mindustry.entities.bullet` 包中所有 BulletType 子类（共 25 个）：
- BasicBulletType, LightningBulletType, ShrapnelBulletType
- LiquidBulletType, PointBulletType, PointLaserBulletType
- RailBulletType, LaserBulletType, EmptyBulletType
- SapBulletType, ContinuousBulletType, ExplosionBulletType
- FireBulletType, MultiBulletType, SpaceLiquidBulletType
- **ArtilleryBulletType**, **BombBulletType**, **ContinuousFlameBulletType**, **ContinuousLaserBulletType**
- **EmpBulletType**, **FlakBulletType**, **InterceptorBulletType**, **LaserBoltBulletType**
- **MassDriverBolt**, **MissileBulletType**

**注意**：MassDriverBolt 和 InterceptorBulletType 在 ClassMap 中也有注册

**JSON 中 `type` 解析逻辑**（ContentParser）：
```java
Class<?> alternate = resolve(Strings.capitalize(data.getString("type", "basic")) + "BulletType", ...);
Class<?> bc = alternate == Object.class ? resolve(data.getString("type", ""), BasicBulletType.class) : alternate;
```
→ 先尝试 `XxxBulletType`，不存在则直接 resolve

### 7. Ability 完整列表（源码验证）

`mindustry.entities.abilities` 包（共 16 个）：
- ArmorPlateAbility, EmptyDataAbility, EnergyFieldAbility
- ForceFieldAbility, LiquidExplodeAbility, LiquidRegenAbility
- MoveEffectAbility, MoveLightningAbility, RegenAbility
- RepairFieldAbility, ShieldArcAbility, ShieldRegenFieldAbility
- SpawnDeathAbility, StatusFieldAbility, SuppressionFieldAbility
- UnitSpawnAbility

**注意：JSON 中 Ability 必须指定 type，无默认值**

### 8. ShootPattern 完整列表（源码验证）

`mindustry.entities.pattern` 包（共 8 个）：
- ShootPattern（基类）, ShootSpread, ShootAlternate
- ShootSine, ShootHelix, ShootBarrel
- ShootMulti, ShootSummon

**关键默认值**：shots=1, firstShotDelay=0, shotDelay=0（均以 tick 计）

### 9. Consume 类型（源码验证）

`mindustry.world.consumers` 包（注意是 `consumers` 不是 `consume`）：
- Consume(abstract), ConsumePower
- ConsumeItemDynamic, ConsumeItemEfficiency, ConsumeItemExplode
- ConsumeItemExplosive, ConsumeItemFilter, ConsumeItemFlammable
- ConsumeItemList, ConsumeItemRadioactive, ConsumeItems, ConsumeItemCharged
- ConsumeLiquid, ConsumeLiquidBase, ConsumeLiquidFilter
- ConsumeLiquidFlammable, ConsumeLiquids, ConsumeLiquidsDynamic
- ConsumeCoolant, ConsumePayloadDynamic, ConsumePayloadFilter
- ConsumePayloads, ConsumePowerCondition, ConsumePowerDynamic

### 10. BuildVisibility（源码验证）

`mindustry.world.meta.BuildVisibility` 是 **class 不是 enum**，12 个静态实例各含 `Boolp` lambda：

| 实例 | 可见条件 |
|------|---------|
| `hidden` | 永远不可见 |
| `shown` | 始终可见 |
| `debugOnly` | 永远不可见 |
| `editorOnly` | `Vars.state.rules.editor` |
| `coreZoneOnly` | `Vars.indexer.isBlockPresent(Blocks.coreZone)` 或非游戏中 |
| `worldProcessorOnly` | editor 或 `allowEditWorldProcessors` |
| `sandboxOnly` | `infiniteResources` |
| `campaignOnly` | `isCampaign()` 或非游戏中 |
| `legacyLaunchPadOnly` | 战役 + legacyLaunchPads + advancedLaunchPad 已解锁 |
| `notLegacyLaunchPadOnly` | 非游戏中 / infiniteResources / 战役中非 legacy |
| `lightingOnly` | lighting / 战役 / 非游戏中 / infiniteResources |
| `fogOnly` | fog / editor / 非游戏中 |

### 11. Category 与 BlockGroup 区别（源码验证）

| 概念 | 类型 | 用途 | 值 |
|------|------|------|----|
| `Category` | enum（`mindustry.type.Category`） | 放置菜单分类标签 | turret, production, distribution, liquid, power, defense, crafting, units, effect, logic（10个） |
| `BlockGroup` | enum（`mindustry.world.meta.BlockGroup`） | 建造替换分组 | none, walls, projectors, turrets, transportation, power, liquids, drills, units, logic, payloads, heat（12个） |

**关键区别**：Category 决定方块在 UI 菜单中的显示位置，BlockGroup 决定建造时是否会替换已有方块。`BlockGroup.anyReplace=true` 的组：walls, projectors, turrets, transportation, logic, payloads, heat

### 12. BlockFlag（源码验证）

见 `mindustry.world.meta.BlockFlag`，共 18 个值，其中 `allLogic = {core, storage, generator, turret, factory, repair, battery, reactor, drill, shield}`（10 个对外公开）

### 13. 所有 ContentParser JSON type 可注册类（ClassMap）

来源：`mindustry.mod.ClassMap`，是**生成类**，自动注册 JSON `type` 到 Java 类的映射。

关键完整注册分布：
- **AI 类型**：16 个（AssemblerAI ~ SuicideAI）
- **Ability**：16 个（Ability ~ UnitSpawnAbility）
- **BulletType**：25 个（ArtilleryBulletType ~ SpaceLiquidBulletType）
- **Effect**：8 个（ExplosionEffect ~ WrapEffect）
- **DrawPart**：12 个（DrawPart ~ ShapePart）
- **ShootPattern**：8 个（ShootAlternate ~ ShootSummon）
- **DrawBlock**：34 个（DrawArcSmelt ~ DrawWeave）
- **Objective**：6 个（Objectives ~ SectorComplete）
- **类型**：Category, CellLiquid, Item, ItemSeq, Liquid, Planet, Sector, StatusEffect, TeamEntry, UnitType, Weapon, Weather

### 14. BulletType 核心默认值（源码验证）

关键字段默认值（`mindustry.entities.bullet.BulletType`）：
```
lifetime=40, speed=1, damage=1, hitSize=4, drawSize=40
drag=0, accel=0, pierceCap=-1, ammoMultiplier=2
splashDamage=0, knockback=0, recoil=0
statusDuration=60*8（8秒）, hitEffect=hitBulletSmall
collidesAir=true, collidesGround=true, collidesTeam=false
collidesTiles=true, hittable=true, reflectable=true, absorbable=true
keepVelocity=true, impact=false, pierce=false, pierceBuilding=false
homingPower=0, homingRange=50, homingDelay=-1（禁用）
fragBullet=null, fragBullets=9, fragVelocityMin=0.2, fragVelocityMax=1
intervalBullet=null, intervalBullets=1, intervalDelay=-1
healPercent=0, healAmount=0, makeFire=false
puddles=0, lightning=0, weaveMag=0, rotateSpeed=0
```

BasicBulletType 额外字段：
```
width=5, height=7, shrinkX=0, shrinkY=0.5, spin=0
backColor=bulletYellowBack, frontColor=bulletYellow
sprite="bullet"
```

### 15. Layer 渲染层级（源码验证）

完整 Layer 常量值（从小到大）：
```
min=-11, background=-10
floor=0, scorch=10, debris=20
blockUnder=29.5, block=30, blockCracks=30.1, blockAfterCracks=30.2
blockAdditive=31, blockProp=32, blockOver=35
blockBuilding=40, turret=50, turretHeat=50.1
groundUnit=60, power=70, legUnit=75
darkness=80, plans=85
flyingUnitLow=90, bullet=100, effect=110, flyingUnit=115
overlayUI=120, buildBeam=122, shields=125
weather=130, light=140, playerName=150
fogOfWar=155, space=160, end=200, endPixeled=210, max=220
```

### 16. BlockFlag 枚举（源码验证）

`mindustry.world.meta.BlockFlag`：
- core, storage, generator, turret, factory, repair, battery, reactor
- extinguisher, drill, shield
- launchPad, unitCargoUnloadPoint, unitAssembler, hasFogRadius, steamVent, blockRepair, synced

**allLogic（逻辑处理器可查询）**：core, storage, generator, turret, factory, repair, battery, reactor, drill, shield

### 17. StatusEffect 字段（源码验证）

```
damageMultiplier=1, healthMultiplier=1, speedMultiplier=1
reloadMultiplier=1, buildSpeedMultiplier=1, dragMultiplier=1
transitionDamage=0, disarm=false, damage=0
intervalDamageTime=0, intervalDamage=0, intervalDamagePierce=false
effectChance=0.15, permanent=false, reactive=false, dynamic=false
show=true, color=Color.white, effect=Fx.none, applyEffect=Fx.none
applyExtend=false, applyColor=Color.white, outline=true
affinities=空集, opposites=空集
```

### 19. Effect 系统（源码验证）

`mindustry.entities.Effect`（332行，entities包直接存放）：
```
lifetime=50, clip=50, startDelay=0, baseRotation=0
followParent=true, rotWithParent=false
layer=Layer.effect（110）
```
- Effect.all 顺序注册，id 从 0 自增
- JSON 可用 Effect 子类（ClassMap 注册，共 8 个）：ExplosionEffect, MultiEffect, ParticleEffect, RadialEffect, SeqEffect, SoundEffect, WaveEffect, WrapEffect
- 原版 Fx.java 约 266 个 Effect 静态实例
- Effect 本身不是抽象类，构造函数有 3 种重载

### 20. DrawPart 系统（源码验证）

`mindustry.entities.part.DrawPart` 基类字段：
```
turretShading=false, under=false, weaponIndex=0, recoilIndex=-1
```

PartProgress 接口（8 种）：
```
reload（开枪后 1→0）, smoothReload（平滑版无跳跃）
warmup（不发射时 0，射击中 1，默认值）
charge（充能 0→1）, recoil（无曲线）, heat（刚发射 1→冷却 0）
life（导弹生命周期 0→1）, time（Time.time 原始值）
```

6 个子类（ClassMap 注册）：RegionPart, ShapePart, HaloPart, HoverPart, FlarePart, EffectSpawnerPart

关键约定：progress 默认为 PartProgress.warmup，clampProgress 默认为 true

### 21. UnitType 子类（源码验证）

继承树：UnitType → {ErekirUnitType(→TankUnitType), NeoplasmUnitType, MissileUnitType}

| 子类 | 关键差异 |
|------|---------|
| ErekirUnitType | outlineColor=Pal.darkOutline, envDisabled=Env.space, researchCostMultiplier=10f |
| TankUnitType(extends ErekirUnitType) | squareShape=true, omniMovement=false, rotateMoveFirst=true, rotateSpeed=1.3f, envDisabled=Env.none |
| MissileUnitType | playerControllable=false, createWreck=false, flying=true, constructor=TimedKillUnit, controller=MissileAI, useUnitCap=false, drawCell=false, allowedInPayloads=false |
| NeoplasmUnitType | 直接继承 UnitType（非 Erekir） |

### 22. BlockStatus（源码验证）

`mindustry.world.meta.BlockStatus` enum（6 个值）：
- active（绿#5ce677）, noOutput（橙）, noInput（红#Pal.remove）
- logicDisable（紫#8a73c6）, inactiveUnitFactory（灰，悬浮显示计时器）, inactive（灰）

### 23. Weapon 完整字段（源码验证，56 个公开字段）

`mindustry.type.Weapon` 关键字段默认值：
```
mirror=true, alternate=true, top=true, rotate=false, flipSprite=false
reload=1（tick），recoil=1.5, recoils=-1, recoilTime=-1, recoilPow=1.8, cooldownTime=20
shootX=0, shootY=3, x=5, y=0
xRand=0, yRand=0, shootCone=5, rotationLimit=361f
rotateSpeed=20, targetInterval=40, targetSwitchInterval=70
velocityRnd=0, extraVelocity=0, shadow=-1（禁用阴影）, inaccuracy=0
minWarmup=0, shootWarmupSpeed=0.1, smoothReloadSpeed=0.15
predictTarget=true, useAttackRange=true, controllable=true, aiControllable=true
alwaysShooting=false, autoTarget=false, continuous=false, alwaysContinuous=false
shootStatusDuration=60*5（5秒）, shootOnDeath=false, shootOnDeathEffect=null
```

**继承链**：Block → BaseTurret → ReloadTurret → Turret → {ItemTurret, PowerTurret, LiquidTurret, ContinuousTurret, ...}

**关键区分**：
- ItemTurret: `ammoTypes` = ObjectMap<Item, BulletType>，构造函数设 `hasItems=true`
- PowerTurret: `shootType` = 单个 BulletType，构造函数设 `hasPower=true`（**不是** ammoTypes）
- LiquidTurret: `ammoTypes` = ObjectMap<Liquid, BulletType>，构造函数设 `hasLiquids=true`
- ContinuousTurret: `shootType`，**无 reload**，始终射击
- ContinuousLiquidTurret: 继承 ContinuousTurret + 液体弹药

**shoot 字段**：类型是 `ShootPattern`，不是 `shootPattern`
**drawer 默认值**：Turret 构造函数中 `drawer = new DrawTurret()`（不是 DrawDefault）
**BaseTurret 默认值**：`update=true, solid=true, attacks=true, flags=BlockFlag.turret`

## 硬性规则（每次回答 Mindustry 问题前必须检查）

1. **字段名精确匹配**：`outputItem`≠`outputItems`，`outputLiquid`≠`outputLiquids`，`drawer`≠`draw`
2. **类型存在性先验证**：先用 mindustry-api-parser 或 grep 源码确认类/字段存在
3. **不要猜测默认值**：不确定就查源码，不要凭印象说"默认是..."
4. **JSON type ≠ 运行时类型**：`type` 是反序列化入口，解析后会被 `remove("type")`
5. **目录名带 s**：`content/blocks/` 不是 `content/block/`
6. **consume 在 consumers 包**：不是 `mindustry.world.consume`
7. **research 是特殊字段**：在 `readFields()` 中被 `remove` 后单独处理，不是普通反射
8. **DrawBlock 是 drawer 字段**：不是 `draw`，也不是 `drawBlock`
9. **能力在 abilities 包**：不是 `ability`
10. **验证顺序**：LMM/知识库 → 源码 → 示例 mod，不可跳步
11. **PowerTurret 用 shootType**：不是 ammoTypes，PowerTurret 没有 ammoTypes
12. **ContinuousTurret 无 reload**：持续光束无弹药消耗、无装填概念
13. **ItemTurret/LiquidTurret 自动设 hasItems/hasLiquids**：JSON 不需要手动写
14. **ShootPattern 类型首字母大写**：`ShootSpread` 不是 `shootSpread`
15. **Turret 默认 drawer 是 DrawTurret**：不是 DrawDefault，不要搞混
16. **UnitType.flying 默认 false**：不是 true，地面单位不需要设置 flying=false
17. **Weapon.mirror 默认 true**：手动写两个武器会创建4个实例（每个镜像复制一次）
18. **UnitType.range 默认 -1**：表示自动从武器计算，不是 0
19. **UnitType.buildSpeed 默认 -1**：表示禁用建造，不是 0
20. **UnitType.mineTier 默认 -1**：表示禁用采矿，不是 0
21. **UnitType.itemCapacity 默认 -1**：表示按 hitSize 计算，不是 0
22. **UnitType.canHeal 在 init() 自动设置**：根据是否有治疗武器，不需要手动设置
23. **UnitType.naval 在 init() 自动设置**：根据环境标志，不需要手动设置
24. **Weapon.reload 默认 1**：极快射速，通常需要手动设置
25. **UnitType.targetAir/targetGround 默认 true**：默认攻击所有目标
26. **UnitType constructor 自动选择**：根据 flying/mech/legs/segments 自动选择 Unit 构造器，JSON 不需要手动指定
27. **Planet.parent 可 null**：顶级行星（如太阳）parent 为 null
28. **PowerGraph 是运行时网络**：不是 Block 字段，是 Building 连接后动态形成的网络
29. **Conveyor.next 是运行时连接**：next 指针在 proximityUpdate 时根据方向计算
30. **TechNode 在 content.techNode**：每个 UnlockableContent 有 techNode 字段指向其节点
31. **EventType 是空接口**：事件类型是空类，靠 Events.fire() 的泛型分发
32. **DrawPart.progress 是 ProgressType 枚举**：不是 float，是枚举如 progress/warmup/charge 等
33. **Tile 坐标是 short**：`public short x, y`，不是 int
34. **Tile.build 可为 null**：纯地面无建筑时 build == null，访问前必须判空
35. **Tile.block() 是方法**：`block` 字段是 protected，必须通过 `block()` 方法访问
36. **Building 模块按 Block 字段创建**：`if(block.hasItems) items = new ItemModule()`，不是 Building 自己的字段
37. **PowerModule.status 是 0~1 浮点数**：不是百分比，不是 boolean
38. **PowerModule 没有 stored/capacity**：储能由 PowerGraph 的电池逻辑管理
39. **LiquidModule 内部用 ObjectFloatMap**：容量由 Block.liquidCapacity 决定，LiquidModule 自身无 capacity
40. **ItemModule 内部用 int[] 数组**：按 item.id 索引，容量由 Block.itemCapacity 决定（默认 10）
41. **Liquid.temperature 默认 0.5f**：不是 0，0.5 是中性温度
42. **Liquid.coolant 默认 true**：大部分液体可作冷却剂
43. **Building.update() 调用顺序**：先 updateConsumption() 算效率，再 updateTile() 执行子类逻辑
44. **edelta() = efficiency * delta()**：工厂进度用 edelta()，不是直接用 delta()
45. **Block.hasItems/hasLiquids/hasPower 是 @NoPatch**：在 Block 类上，不在 Building 上
46. **Tile 有 3 个独立 data 字节**：data、floorData、overlayData，分别用于不同层
47. **Liquid.boilPoint 默认 2f**：温度超过此值蒸发，不是 100f 或其他值
48. **Planet.parent 可为 null**：恒星/太阳的 parent 为 null，orbitRadius 此时为 0
49. **Planet.orbitRadius 是自动计算的**：parent.totalRadius + parent.orbitSpacing + totalRadius，不可手动设置
50. **Planet.orbitTime 基于开普勒定律**：pow(orbitRadius, 1.5) * 1000，不可手动设置
51. **Planet.generator 在 init() 中运行**：不是 load() 或构造函数
52. **Planet.defaultEnv 是位标志**：用 `|` 组合，不是枚举比较
53. **Sector 不是 UnlockableContent**：Sector 是普通类，SectorPreset 才继承 UnlockableContent
54. **Sector.threat 是 float 0~1**：不是 int，不是百分比
55. **SectorPreset.generator 是 FileMapGenerator**：不是 PlanetGenerator，类型不同
56. **SectorPreset.captureWave 默认 0**：表示不限制，不是第 0 波
57. **Weather.duration 默认 10 分钟**：`10f * Time.toMinutes`，不是 1 分钟
58. **Weather.statusDuration 默认 2 秒**：`60f * 2`，不是 1 秒
59. **ParticleWeather.force 默认 0**：无推力，不是有风力
60. **PayloadBlock 和 PayloadConveyor 都直接继承 Block**：两者无继承关系
61. **PayloadConveyor 默认 size=3**：不是 1，是 3x3 建筑
62. **Payload 是接口不是类**：BuildPayload 和 UnitPayload 是两个独立实现
63. **Planet.launchCapacityMultiplier 默认 0.25f**：不是 1f
64. **LogicBlock.instructionsPerTick 默认 1**：不是 10 或 100，普通处理器每 tick 只执行 1 条指令
65. **LogicBlock.maxInstructionsPerTick 是特权处理器专用**：默认 40，普通处理器用 instructionsPerTick
66. **LogicBlock.range 默认 80**：`8 * 10`，不是无限
67. **LogicBlock.maxLinks 是 6000**：不是无限
68. **LExecutor.maxInstructions 是 1000**：不是无限，超过会中断执行
69. **MemoryBlock.memoryCapacity 默认 32**：不是 64 或 1024
70. **MemoryBlock 存储类型是 double[]**：不是 float[] 或 int[]
71. **MemoryBlock 越界返回 NaN**：不是 0，不是抛异常
72. **MemoryBlock.canOverdrive = false**：内存块不能被超速
73. **AIController 更新循环**：updateVisuals → updateTargeting → updateMovement
74. **LogicAI 有 controlTimer 超时**：超时后自动恢复默认 AI，不是永久控制
75. **SuicideAI 继承 GroundAI**：不是直接继承 AIController
76. **MissileAI 是独立 AI**：不是 FlyingAI 的子类
77. **Effect.lifetime 默认 50 tick**：不是 60 tick（1 秒），是 50/60 ≈ 0.83 秒
78. **Effect.layer 默认 Layer.effect (110)**：不是 Layer.flyingUnit (115)
79. **Effect.followParent 默认 true**：特效默认跟随父对象
80. **Effect.shouldCreate() 检查 headless**：服务器端不渲染特效
81. **内容加载顺序固定**：先原版(createBaseContent) → 再mod(loadContent) → 再load() → 再init()
82. **Mod 先 Java 后 JSON**：Java mod 的 loadContent() 先执行，再扫描 content/ 目录
83. **content/ 目录名必须以 s 结尾**：`blocks/` 不是 `block/`，`units/` 不是 `unit/`
84. **GameState.wave 默认 1**：不是 0，第一波是 wave 1
85. **GameState.wavetime 是 tick**：不是秒，需要除以 60
86. **GameState.tick 是 double**：不是 int/float，可以非常大
87. **Rules.waves 默认 false**：不是 true，需要手动开启
88. **Rules.winWave 默认 0**：<=0 表示禁用，不是第 0 波获胜
89. **Rules.waveSpacing 默认 2 分钟**：`2 * Time.toMinutes`（7200 tick）
90. **Rules.loadout 默认 100 铜**：不是空的
91. **Rules.deconstructRefundMultiplier 默认 0.5f**：拆建筑返还 50%
92. **Rules.enemyCoreBuildRadius 默认 400f**：不是无限
93. **Rules.env 是位标志**：用 `|` 组合，不是枚举
94. **存档格式是二进制**：不是 JSON，不是文本，版本 1~12
95. **mod 自定义数据在 Custom Chunks**：不破坏存档兼容性
96. **每个队伍有独立 TeamRule**：通过 teams.get(team) 访问
97. **Renderer 有 6 个子渲染器**：blocks/fog/minimap/overlights/lights/pixelator，不是单一渲染器
98. **渲染管线按 Layer 常量排序**：floor→block→shadows→turret→units→bullets→effects→shields→weather→light→fog
99. **animateShields 默认 false**：需要在设置中开启，否则护盾无动画
100. **bloom 可为 null**：不是所有设备都支持泛光效果
101. **InputHandler.rotation 默认 1**：不是 0
102. **InputHandler.isBuilding 默认 true**：默认处于建造模式
103. **InputHandler 是抽象类**：DesktopInput 和 MobileInput 是两个平台实现
104. **ConnectPacket 包含 mods 列表**：用于 mod 同步检查，不匹配会断开
105. **ConnectPacket 包含 uuid/usid**：用于身份验证
106. **minZoom 是缩小（视野大）**：值越大视野越大（zoom out），maxZoom 是放大（视野小）
107. **landTime 从正数递减到 0**：正值表示正在着陆，0 表示已完成
108. **effectBuffer 用于水波和护盾**：不是所有特效都用 effectBuffer
109. **FloorRenderer 是 BlockRenderer 的成员**：不是独立的渲染器类
110. **ContentType 枚举顺序不可重排**：ID 与存档绑定，重排会破坏存档兼容
111. **ContentType 有 5 个废弃条目**：mech_UNUSED, effect_UNUSED, loadout_UNUSED, typeid_UNUSED, ammo_UNUSED
112. **目录名全小写**：`unitCommand` → `unitcommands/`，不是 `unitCommand/`
113. **覆盖原版 block 时不能写 type**：写 type 会创建新 block，不写才能修改原版字段
114. **JSON block type 默认是 Block**：不写 type 时默认 `Block`，不是 `GenericCrafter`
115. **resolve() 首字母自动大写**：`"mechanicalDrill"` → `"MechanicalDrill"`
116. **BulletType type 默认 basic**：先找 `BasicBulletType`，找不到直接 resolve
117. **DrawBlock type 默认 DrawDefault**：数组形式自动创建 DrawMulti
118. **Ability 必须指定 type**：无默认值，不写会报错
119. **Block.health 默认 -1**：自动按 `size * size * scaledHealth` 计算
120. **Block.scaledHealth 默认 40**：再按需求物品 healthScaling 缩放
121. **Block.liquidCapacity 默认 -1**：自动按消费者计算
122. **Block.buildTime 默认 -1**：自动按需求物品 cost 计算
123. **Block.init() 中 consumers 分类**：optional/nonOptional/update 三组
124. **变体纹理从 1 开始**：`name + (i + 1)`，不是 `name + i`
125. **队伍纹理命名**：`name + "-team-" + team.name`
126. **consumes 是特殊字段**：在 readFields 之前单独处理
127. **block 名自动加 mod 前缀**：`mod名-文件名`，避免冲突
128. **CoreBlock.unitType 默认 alpha**：不是 null，核心默认生成 alpha 单位
129. **CoreBlock.landDuration 默认 160 tick**：约 2.67 秒，不是 1 秒
130. **CoreBlock.captureInvicibility 默认 15 秒**：`60f * 15f`
131. **核心不能被拆除/替换/拾取**：canBreak/canReplace/canPickup 都返回 false
132. **LaunchPad.launchTime 默认 1f tick**：不是 1 秒！是 1 tick，需要手动设置
133. **LaunchPad.outputsItems() 返回 false**：不输出物品到相邻建筑
134. **UnitFactory.plans 是 Seq<UnitPlan>**：不是数组
135. **UnitPlan.requirements 是 ItemStack[]**：不是 Seq
136. **UnitPlan.time 是 tick**：不是秒
137. **Reconstructor.upgrades 是 UnitType[] 数组的 Seq**：每对是 [输入, 输出]
138. **Reconstructor.constructTime 默认 120 tick（2 秒）**：不是 60 tick
139. **UnitAssembler 继承 PayloadBlock**：不是 UnitBlock
140. **UnitAssembler.areaSize 默认 11**：11x11 区域
141. **UnitAssembler.dronesCreated 默认 4**：不是 1
142. **AssemblerUnitPlan 有三种需求**：PayloadStack/ItemStack/LiquidStack，都可为 null
143. **MapObjective 有依赖链**：parents 必须全部完成才能执行
144. **目标 completionLogicCode 通过 LExecutor 执行**：不是直接调用
145. **UnitBlock 继承 PayloadBlock**：不是直接继承 Block
146. **Item.cost 默认 1f**：不是 0，影响建造时间计算
147. **Item.healthScaling 默认 0f**：影响建筑血量（scaledHealth *= scaling）
148. **Item.hardness 默认 0**：影响钻头采矿速度
149. **Item 是类不是 enum**：通过 Items.load() 注册
150. **Team 是运行时对象**：不是 UnlockableContent，ContentType.team 对应 TeamEntry
151. **Team 有 256 个**：前 7 个内置，其余占位
152. **neoplastic.ignoreUnitCap = true**：肿瘤队忽略单位上限
153. **TeamEntry 只用于显示**：是 UnlockableContent，用于内容数据库
154. **Attribute 是可扩展的**：mod 可通过 Attribute.add() 注册新属性
155. **Attribute.env() 从 state.envAttrs 获取**：不是从 Block 或 Tile
156. **Attributes 是 float 数组封装**：按 Attribute.id 索引
157. **Schematic.tiles 是 Seq<Stile>**：Stile 含 block/x/y/config/rotation
158. **TechNode.requiresUnlock 默认 false**：根节点默认不需要解锁
159. **TechNode.planet 从父节点继承**：不是手动设置
160. **TechNode.depth 从 0 开始**：根节点 depth = 0

### 存档系统（源码验证）

161. **存档 header 是 MSAV**：4 字节 `{'M', 'S', 'A', 'V'}`
162. **当前最新版本是 Save12**：有 12 个版本（Save1~Save12）
163. **存档格式有 7 个区域**：meta→patches(v12+)→content→map→entities→markers(v8+)→custom
164. **CustomChunk 接口用于 mod 自定义数据**：write/read/shouldWrite/writeNet
165. **addCustomChunk 是静态方法**：在 mod 初始化时调用，注册到 SaveVersion.customChunks
166. **存档使用 InflaterInputStream 压缩**：读取时解压，写入时压缩
167. **fallback 映射处理旧版本方块名**：如 "dart-mech-pad" → "legacy-mech-pad"
168. **Saves.load() 并行加载存档**：使用 mainExecutor 并行读取
169. **SaveMeta.mods 是 String[]**：从 tags 的 "mods" 读取，不是 Seq
170. **存档保存时先备份再写入**：写入失败则恢复备份
171. **Block.java 完整字段验证**（394行，`mindustry.world.Block`）：
    - hasItems/hasLiquids/hasPower 都有 @NoPatch 注解，初始化后不能再改
    - consumesPower=true（即使无电力模块也默认 true）
    - update=false, destructible=false, solid=false, rotate=false
    - health=-1（使用 scaledHealth=40/size），size=1, itemCapacity=10
    - liquidCapacity=-1（自动按 ConsumeLiquid 调整），buildTime=-1（自动按需求算）
    - category=Category.distribution（不是 none）, group=BlockGroup.none
    - buildVisibility=hidden, buildCostMultiplier=1f
    - requirements={}, envRequired=0, envEnabled=Env.terrestrial, envDisabled=0
    - flags=EnumSet.of(), priority=TargetPriority.base
    - placeEffect=Fx.placeBlock, breakEffect=Fx.breakBlock, destroyEffect=Fx.dynamicExplosion
    - researchCostMultiplier=1, mapColor=黑(0,0,0,1), rebuildable=true
    - absorbLasers=false, hasShadow=true, emitLight=false, fogRadius=-1
    - ambientSound=Sounds.none, canOverdrive=true, enableDrawStatus=true
    - autoResetEnabled=true, allowDiagonal=true, squareSprite=true, drawCracks=true
    - underBullets=false, fillsTile=true, drawTeamOverlay=true
    - salvageRepairDuration=30*60（30秒）
172. **AIController 更新循环 + 完整字段**（`mindustry.entities.units.AIController`）：
    - 核心字段：unit, fallback(AIController), target(Teamc), bomberTarget, noTargetTime, turningAway
    - timer=Interval(4): timerTarget=0, timerTarget2=1, timerTarget3=2, timerTarget4=3
    - 更新顺序：useFallback() → updateVisuals() → updateTargeting() → updateMovement()
    - target()/findMainTarget()/findTarget() 三个目标查找方法
    - targetFlag()/targetFlagActive() 按 BlockFlag 查找
    - pathfind() 3个重载（pathTarget, stopAtTargetTile, avoidance）
    - faceTarget()/faceMovement()，默认 updateMovement/updateVisuals 为空
    - shouldFire()/shouldShoot() 默认返回 true
173. **AIController 子类继承树**（14 个）：
    - AIController → {GroundAI→SuicideAI, FlyingAI→FlyingFollowAI, MissileAI, CommandAI, LogicAI, HugAI, AssemblerAI, BuilderAI, MinerAI, CargoAI, DefenderAI, PrebuildAI, RepairAI, BoostAI}
174. **UnitType 控制器分配链**：
    - aiController（默认 `!flying ? GroundAI : FlyingAI`）
    - controller（`!playerControllable || team.isAI() && !rtsAi ? aiController : CommandAI`）
    - createController(unit) → controller.get(unit)
175. **TractorBeamTurret extends BaseTurret**：不继承 Turret，无 ammoTypes/shootType，纯力场牵引
176. **TractorBeamTurret.targetGround 默认 false**：默认只对空，不对地
177. **TractorBeamTurret.shootCone 默认 6f**：不是 Turret 的 8f
178. **PointDefenseTurret extends ReloadTurret**：不继承 Turret，自动拦截附近子弹
179. **UnitType constructor 10 种默认映射**：flying→UnitEntity, mech→MechUnit, legs→LegsUnit, naval→UnitWaterMove, payload→PayloadUnit, missile→TimedKillUnit, tank→TankUnit, hover→ElevationMoveUnit, tether→BuildingTetherPayloadUnit, crawl→CrawlUnit
180. **Wall.chanceDeflect 默认 -1f**：禁用偏转，不是 0
181. **Wall.lightningChance 默认 -1f**：禁用闪电反击
182. **GenericCrafter 构造函数设 update=true, solid=true, hasItems=true, drawArrow=false**：不需要 JSON 手动写
183. **BaseTurret.range 默认 80f**：不是 0
184. **BaseTurret.rotateSpeed 默认 5**：不是 1
185. **BaseTurret.priority = TargetPriority.turret**：不是默认的 TargetPriority.base
186. **BaseTurret.group = BlockGroup.turrets**：建造时自动替换其他炮塔
187. **LaserTurret extends PowerTurret**：不是直接继承 Turret，hasPower=true
188. **LiquidTurret 构造函数设 hasLiquids=true, loopSound=Sounds.loopSpray**：不需要 JSON 手动写 hasLiquids
189. **ContinuousTurret 构造函数设 coolantMultiplier=1f, displayAmmoMultiplier=false**
190. **BulletType.layer 默认 Layer.bullet (100)**：不是 Layer.effect (110)
191. **BulletType.targetBlocks 默认 true**：子弹默认攻击建筑
192. **BulletType.targetMissiles 默认 true**：子弹默认可击中导弹
193. **BulletType.collideFloor 默认 false**：子弹默认不与地面碰撞
194. **BulletType.laserAbsorb 默认 true**：子弹默认被激光吸收
195. **GenericCrafter.init() 逻辑顺序**：先转数组(outputItem→outputItems, outputLiquid→outputLiquids)，再回写outputLiquid=outputLiquids[0]，再设outputsLiquid/hasItems/hasLiquids，最后super.init()
196. **Block.init() 健康值计算**：health=-1时按size*size*scaledHealth计算，scaledHealth默认40再乘以物品healthScaling
197. **Block.init() 液体容量**：liquidCapacity<0时自动按消费者最大消耗量*10计算
198. **Block.init() 建造时间**：buildTime<0时按需求物品cost累加计算，再乘buildCostMultiplier
199. **Block.init() 消费者分类**：consumers分为optional/nonOptional/update三组，consPower从consumers中查找
200. **Block.init() 电力警告**：outputsPower=false且consPower.buffered=true时强制buffered=false并警告
201. **Block.init() 物品过滤器**：itemFilter和liquidFilter按content.items()/liquids().size创建boolean数组
202. **Block.init() 可配置逻辑**：如果configurations中有UnlockableContent类型则自动设logicConfigurable=true
203. **GenericCrafter.load()**：调用drawer.load(this)加载绘制器纹理
204. **GenericCrafter.setBars()**：outputLiquids非空时移除默认liquid bar，为每个输出液体单独添加bar
205. **GenericCrafter.rotatedOutput()**：液体输出方向检查，-1表示所有方向，否则只在指定方向输出
206. **DrawPart 6个子类**：RegionPart, ShapePart, HaloPart, HoverPart, FlarePart, EffectSpawnerPart
207. **DrawPart 基类字段**：under=false, weaponIndex=0, recoilIndex=-1, progress默认warmup
208. **Ability 16个子类**：ArmorPlate, EmptyData, EnergyField, ForceField, LiquidExplode, LiquidRegen, MoveEffect, MoveLightning, Regen, RepairField, ShieldArc, ShieldRegenField, SpawnDeath, StatusField, SuppressionField, UnitSpawn
209. **Ability 基类字段**：display=true，JSON必须指定type
210. **StatusEffect 默认值**：damageMultiplier/healthMultiplier/speedMultiplier/reloadMultiplier/buildSpeedMultiplier/dragMultiplier=1f, disarm=false, effectChance=0.15f, permanent=false, color=Color.white
211. **StatusEffect.affinities/opposites**：默认空ObjectSet，用于状态效果互斥/亲和
212. **ForceProjector 默认值**：radius=101.7f, sides=6, shieldHealth=700f, cooldownNormal=1.75f, cooldownLiquid=1.5f, cooldownBrokenBase=0.35f
213. **ForceProjector.phaseBoost**：phaseRadiusBoost=80f, phaseShieldBoost=400f, phaseUseTime=350f
214. **MendProjector 默认值**：reload=250f, range=60f, healPercent=12f, phaseBoost=12f, phaseRangeBoost=50f
215. **ShieldWall 默认值**：shieldHealth=900f, breakCooldown=600f(10秒), regenSpeed=2f
216. **Drill 关键字段**：hardnessDrillMultiplier=50f, drillTime=300, liquidBoostIntensity=1.6f, rotateSpeed=2f
217. **Drill.getDrillTime() 公式**：(hardnessDrillMultiplier + hardness) / (drillSpeed * efficiency) * drillTime
218. **Pump 默认值**：pumpAmount=0.2f, consumeTime=300f(5秒), drawer=DrawMulti(DrawDefault, DrawPumpLiquid)
219. **Conveyor 默认值**：speed=0f（需要设置），pushUnits=true
220. **Router 默认值**：speed=8f（单个速度）
221. **PowerNode 默认值**：laserRange=6, maxNodes=3, autolink=true, laserScale=0.25f
222. **NuclearReactor 默认值**：itemDuration=120, heating=0.01f, heatOutput=15f, coolantPower=0.5f, fuelItem=Items.thorium
223. **ImpactReactor 默认值**：warmupSpeed=0.001f, itemDuration=60f
224. **UnitFactory 默认值**：plans=Seq<UnitPlan>, createSound=Sounds.unitCreate
225. **Reconstructor 默认值**：constructTime=120(2秒), upgrades=Seq<UnitType[]>
226. **CoreBlock 默认值**：unitType=UnitTypes.alpha, landDuration=160f, captureInvicibility=900f(15秒)
227. **CoreBlock.incinerateNonBuildable 默认 false**：不可建造物品不销毁
228. **LaunchPad 默认值**：launchTime=1f（1 tick，不是1秒！）, lightStep=1f, liquidPad=2f
229. **LaunchPad.outputsItems() 返回 false**：不输出物品到相邻建筑
230. **MassDriver 默认值**：rotateSpeed=5f, minDistribute=10, knockback=4f, reload=100f, bulletSpeed=5.5f, bulletLifetime=200f
231. **LogicBlock 默认值**：instructionsPerTick=1, maxInstructionsPerTick=40, range=80(8*10)
232. **LogicBlock.maxInstructionScale 默认 5**：最大指令缩放倍数
233. **MemoryBlock 默认值**：memoryCapacity=32, 存储类型double[]
234. **Weather 默认值**：duration=10分钟(10f*Time.toMinutes), statusDuration=2秒(60f*2), hidden=false
235. **Weather.statusAir/statusGround 默认 true**：天气状态效果影响空中和地面单位
236. **Planet 默认值**：orbitSpacing=12f, minZoom=0.5f, maxZoom=2f, rotateTime=24*60, defaultEnv=terrestrial|spores|groundOil|groundWater|oxygen
237. **Planet.launchCapacityMultiplier 默认 0.25f**：发射台容量乘数，不是1f
238. **Planet.startSector 默认 0**：起始扇区ID
239. **Planet.allowLaunchSchematics/allowLaunchLoadout 默认 false**：默认不允许发射蓝图/装载
240. **Planet.allowSectorInvasion 默认 false**：默认不允许扇区入侵
241. **SectorPreset 默认值**：captureWave=0(不限制), startWaveTimeMultiplier=2f, addStartingItems=false, requireUnlock=true
242. **SectorPreset.rules 默认设置 winWave**：`rules -> rules.winWave = captureWave`
243. **Rules 关键默认值**：waveTimer=true, waveSending=true, wavesSpawnAtCores=true, attackMode=false, editor=false
244. **Rules.waveSpacing 默认 2分钟**：2*Time.toMinutes(7200 tick)
245. **Rules.winWave 默认 0**：<=0表示禁用
246. **Rules.loadout 默认 100铜**：ItemStack.list(Items.copper, 100)
247. **Rules.deconstructRefundMultiplier 默认 0.5f**：拆建筑返还50%
248. **Rules.enemyCoreBuildRadius 默认 400f**：核心禁区半径
249. **Rules.dropZoneRadius 默认 300f**：空投区半径
250. **Rules.dragMultiplier 默认 1f**：环境阻力乘数
251. **Rules.logicUnitControl 默认 true**：逻辑控制单位
252. **Rules.logicUnitBuild 默认 true**：逻辑建造
253. **Rules.logicUnitDeconstruct 默认 false**：逻辑拆除
254. **Rules.disableWorldProcessors 默认 false**：不禁止世界处理器
255. **InputHandler.rotation 默认 1**：不是0
256. **InputHandler.isBuilding 默认 true**：默认建造模式
257. **Renderer 6个子渲染器**：blocks, fog, minimap, overlays, lights, pixelator
258. **Renderer.minZoom/maxZoom 默认 1.5f/6f**：游戏内minZoomInGame=0.5f, maxZoomInGame=6f
259. **Renderer.animateShields 默认 false**：护盾动画需设置开启
260. **Renderer.effectBuffer 是 FrameBuffer**：用于水波和护盾效果

## 第3轮补充：电力系统高频误区（2026-06-14 源码验证）

### 1. PowerGraph 结构
- 4 个独立列表：`producers` / `consumers` / `batteries` / `all`，不是 grouped in all
- `satisfaction = lastProduced / lastNeeded`（clamp 0~1），不是即时电流
- 电池分类条件：`outputsPower && consumesPower && consPower.buffered` → batteries
- 生产者/消费者双角色（非 buffered）→ 同时加入 producers + consumers
- `transferPower()` 正值充电，负值放电，不是直接分配

### 2. PowerNode
- `consumesPower = false`（虽然有 connectedPower=true）
- `outputsPower = false`
- `update = false` — 不调用 updateTile，完全不更新
- `canOverdrive = false`
- 绝缘检测：`World.raycast` 全线检查 `isInsulated()`
- 不连接相邻格建筑（4 方向），`Structs.contains(Edges.getEdges(size))`
- `maxNodes=3` 不是 6

### 3. Battery
- `update = false` — 无 updateTile
- `consumesPower = true` — 同时是消费者
- `outputsPower = true`
- `warmup()` 返回 `power.status`，不是常规 warmup
- `canOverdrive = false`
- 绘制由 `DrawMulti(DrawDefault, DrawPower, DrawRegion("-top"))` 完成

### 4. BeamNode
- `consumesPower = outputsPower = false`
- `underBullets = true`
- 跳过 PowerNode：`!(other.block instanceof PowerNode)`
- 只在 `world.tileChanges` 变化时更新，不每帧扫描
- 4 方向独立，每方向只连第一个有电的非 PowerNode 方块
- 被绝缘块射线命中即停止搜索（方向独立）

### 5. ImpactReactor
- `warmupSpeed = 0.001f` — 极慢
- `productionEfficiency = pow(warmup, 5)` — 5 次方曲线
- 升温条件：`efficiency >= 0.9999f && power.status >= 0.99f`
- `consumesPower = true`（同时是生产者与消费者）
- 净输出 = `powerProduction * pow(warmup,5) - consPower.usage`

### 6. NuclearReactor
- `delta()` 上限 `min(delta, 4)` — 防止跳帧
- `heat >= 0.999f` → 过热爆炸
- `rebuildable = false`
- `shouldExplode()` 条件：fuel >= 5 || heat >= 0.5f
- 冷却液等比例带走：`heat -= min(amount, heat / coolantPower) * coolantPower`

### 7. ConsumeGenerator
- `generateTime = 1f` 重置（不是累积 time）
- `productionEfficiency = efficiency * efficiencyMultiplier`
- `itemDurationMultiplier` 只影响消耗频率，不影响发电量
- `consumeTriggerValid()` 检查 `generateTime > 0`

### 8. PowerDiode
- `insulated = true` — 不参与电力连接
- 传输量 = `(targetPercent * frontCapacity - frontStored) / 2` — 只移一半差额

### 9. SolarGenerator
- `flags = EnumSet.of()` — 空标志，无 generator 标志（AI 低优先级）

### 10. VariableReactor
- 依赖外部热量输入，不消耗物品
- `instability` 由 heat/冷却液决定
- 产量 = `powerProduction * efficiency * clamp(heat / maxHeat)`

### 11. LightBlock
- **直接 extends Block**，不是 PowerBlock
- 无电力网络连接（`consumesPower` 等字段不继承自 PowerBlock 体系）