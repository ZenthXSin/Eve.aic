# Mindustry 关键系统源码验证字段

> Source: 源码验证，2026-06-08 更新

## 1. UnitType 单位类型（源码验证）

关键字段（`mindustry.type.UnitType`）：
```
speed=1.1f, hitSize=6f, itemCapacity=0, mineTier=-1, mineSpeed=1f
health=200, armor=0, buildSpeed=1f, commandLimit=24
rotateSpeed=5f, baseRotateSpeed=5f, lockLegBase=false
isEnemy=true, playerControllable=true, allowedInPayloads=true
createWreck=true, createScorch=true, destroyOnDeath=true
flying=false, canBoost=false, boostMultiplier=1.5f
mechLandShake=0f, mechSideSway=0.54f, mechFrontSway=0.1f
legCount=4, legGroupSize=2, legLength=10f, legSpeed=0.6f
segments=0, segmentPhase=5f, segmentMag=1.5f
constructor: 根据flying/mех/legs等自动选择
aiController: flying ? FlyingAI() : GroundAI()
controller: 玩家或AI根据team判断
abilities: Seq<Ability>, weapons: Seq<Weapon>
immunities: ObjectSet<StatusEffect>
```

## 2. Weapon 武器（源码验证）

关键字段（`mindustry.type.Weapon`）：
```
bullet=Bullets.placeholder, mirror=true, rotate=false
reload=1f, inaccuracy=0f, recoil=1.5f, shootCone=5f
x=5f, y=0f, shootX=0f, shootY=3f
shoot=new ShootPattern(), velocityRnd=0f
continuous=false, alwaysContinuous=false
top=true, layerOffset=0f, shadow=-1f
alternate=true, flipSprite=false
ejectEffect=Fx.none, shootSound=Sounds.shoot
```

## 3. Planet 行星（源码验证）

关键字段（`mindustry.type.Planet`）：
```
radius=1f, orbitRadius=0f, orbitTime=1f, rotateTime=24f*60f
tidalLock=false, accessible=true, visible=true, bloom=false
defaultEnv=Env.terrestrial|Env.spores|Env.groundOil|Env.groundWater|Env.oxygen
startSector=0, sectorSeed=-1
launchCapacityMultiplier=0.25f
allowLaunchSchematics=false, allowLaunchLoadout=false
allowSectorInvasion=false, allowLegacyLaunchPads=false
clearSectorOnLose=false, prebuildBase=true
allowWaves=false, allowLaunchToNumbered=true
defaultCore=Blocks.coreShard
parent: @Nullable Planet, solarSystem: Planet
children: Seq<Planet>, sectors: Seq<Sector>
generator: @Nullable PlanetGenerator, grid: @Nullable PlanetGrid
techTree: @Nullable TechNode
```

## 4. PowerGraph 电力网络（源码验证）

关键字段（`mindustry.world.blocks.power.PowerGraph`）：
```
producers: Seq<Building>, consumers: Seq<Building>
batteries: Seq<Building>, all: Seq<Building>
```
关键方法：
- `getPowerProduced()`, `getPowerNeeded()`, `getBatteryStored()`
- `getSatisfaction()`, `getPowerBalance()`
- `transferPower(float)`, `distributePower(...)`
- `add(Building)`, `remove(Building)`, `reflow(Building)`

## 5. Conveyor 传送带（源码验证）

关键字段（`mindustry.world.blocks.distribution.Conveyor`）：
```
speed=0f, displayedSpeed=0f, pushUnits=true
junctionReplacement: @Nullable Block, bridgeReplacement: @Nullable Block
```
Building 内部类 `ConveyorBuild`：
```
ids: Item[], xs: float[], ys: float[], len=0
next: @Nullable Building, nextc: @Nullable ConveyorBuild
aligned: boolean, clogHeat=0f, minitem=1
blendbits, blending, blendsclx, blendscly
```

## 6. TechNode 科技树节点（源码验证）

关键字段（`mindustry.content.TechTree.TechNode`）：
```
depth=0, parent: @Nullable TechNode, rootNode: @Nullable TechNode
content: UnlockableContent, requirements: ItemStack[]
finishedRequirements: ItemStack[], children: Seq<TechNode>
objectives: Seq<Objective>, planet: @Nullable Planet
requiresUnlock=false, researchCostMultipliers: ObjectFloatMap<Item>
icon: @Nullable Drawable, name: @Nullable String（root节点用）
```

## 7. EventType 事件类型（源码验证）

关键事件（`mindustry.game.EventType`）：
- 游戏流程：`PlayEvent`, `ResetEvent`, `DisposeEvent`, `ClientCreateEvent`
- 世界加载：`WorldLoadEvent`, `WorldLoadBeginEvent`, `WorldLoadEndEvent`
- 存档：`SaveWriteEvent`, `SaveLoadEvent`
- 战斗：`WaveEvent`, `WinEvent`, `LoseEvent`, `TurnEvent`
- 区块：`SectorLoseEvent`, `SectorInvasionEvent`, `LaunchItemEvent`
- 内容：`ContentInitEvent`, `ModContentLoadEvent`, `ContentPatchLoadEvent`
- UI：`ResizeEvent`, `BlockInfoEvent`, `LineConfirmEvent`
- 音乐：`MusicRegisterEvent`
- 其他：`HostEvent`, `ServerLoadEvent`, `ClientLoadEvent`, `MapMakeEvent`, `MapPublishEvent`, `AtlasPackEvent`, `FileTreeInitEvent`

## 8. DrawPart 绘制部件（源码验证）

关键字段（`mindustry.graphics.DrawPart`）：
```
progress=ProgressType.progress, under=false, turretHeatLayer=Layer.turretHeat
outline=true, drawRegion=true
```
子类：`DrawTurret`, `DrawRegion`, `DrawMulti`, `DrawFlame`, `DrawCells`, `DrawLiquid`, `DrawHeat`, `DrawGlow`, `DrawPistons`, `DrawShape`, `DrawSpikes`, `DrawBlurSpin`, `DrawWeave`, `DrawCultivator`, `DrawBlockParts` 等

## 硬性规则补充

16. **UnitType constructor 自动选择**：根据 flying/mech/legs/segments 自动选择 Unit 构造器，JSON 不需要手动指定
17. **Weapon.mirror 默认 true**：武器默认镜像，单武器需设 `mirror=false`
18. **Planet.parent 可 null**：顶级行星（如太阳）parent 为 null
19. **PowerGraph 是运行时网络**：不是 Block 字段，是 Building 连接后动态形成的网络
20. **Conveyor.next 是运行时连接**：next 指针在 proximityUpdate 时根据方向计算
21. **TechNode 在 content.techNode**：每个 UnlockableContent 有 techNode 字段指向其节点
22. **EventType 是空接口**：事件类型是空类，靠 Events.fire() 的泛型分发
23. **DrawPart.progress 是 ProgressType 枚举**：不是 float，是枚举如 progress/warmup/charge 等
