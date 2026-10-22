# VoidShield 数值平衡方案（参考原版按功能分层）

> Source: conversation + VoidShield 源码 + Mindustry 原版源码

基于 `voidshield.content.VSBlocks`、各建筑实现类，以及原版 `mindustry.content.Blocks`、`mindustry.world.blocks.defense.ForceProjector`、`mindustry.world.blocks.defense.BaseShield`、`mindustry.world.blocks.defense.turrets.PointDefenseTurret` 做的一版功能导向平衡方案。

## 额外约束

用户已明确：**建筑 `size` 不能改**，因为与贴图绑定。故本方案只调整：
- `requirements(...)`
- `consumePower(...)`
- `consumeLiquid(...)`
- `heatingRate`
- `specificHeat`
- `maxTemperature`
- `defaultHeat`
- `maxFissureCount`
- `maxArea`
- `health`

不调整任何建筑的 `size`。

## 目标

把现在 `VSBlocks` 里明显失衡的地方拉回到可玩的梯度：
- 冷却器做出早期/中期/后期分层
- 热传输件按功能复杂度定成本
- voidshield 立场类建筑按拦截能力和覆盖规模拉开价位

## 已确认的当前问题

1. `air-cooler` 在 `voidshield.content.VSBlocks` 里 `consumePower(300f)`，远高于原版同级功能块，和 `evaporative-cooler` 的 `consumePower(8f)` 形成反梯度。
2. `heat-catheter`、`heat-router`、`heat-crossover` 全都接近同价，但功能复杂度不同。
3. `micro-void`、`velum-solvent`、`cor-vacuum` 都只吃接近同一档材料，不符合它们的尺寸和功能。
4. `cor-vacuum` 当前实现类 `voidshield.world.blocks.voidshield.CorVacuum` 只有 `HeatBlock` 基础行为，属于大体积空壳，不能给太夸张的实战价值假设。

## 原版锚点

### 防御系
- `mindustry.world.blocks.defense.ForceProjector`
  - 原版块：`mindustry.content.Blocks.forceProjector`
  - `size = 3`
  - `radius = 101.7f`
  - `shieldHealth = 750f`
  - `consumePower(4f)`
- `mindustry.world.blocks.defense.BaseShield`
  - 原版测试块：`mindustry.content.Blocks.shieldProjector`
  - `size = 3`
  - `consumePower(5f)`
- `mindustry.world.blocks.defense.turrets.PointDefenseTurret`
  - 原版块：`mindustry.content.Blocks.segment`
  - `size = 2`
  - `range = 180f`
  - `consumePower(8f)`
  - `bulletDamage = 30f`
  - `reload = 8f`

### 工艺/冷却参考
- `mindustry.content.Blocks.cryofluidMixer`
  - `size = 2`
  - `consumePower(1f)`
  - `consumeLiquid(Liquids.water, 12f / 60f)`
- `mindustry.content.Blocks.phaseWeaver`
  - `size = 2`
  - `consumePower(5f)`

这些锚点说明：原版 2~3 格中后期功能块的常见耗电区间大多在 `1f~8f`，`300f` 这种值没有参考基础。

## 建议数值

### 一、冷却器

#### heat-sink
路径：`voidshield.content.VSBlocks.heatSink`

建议：
- 保持 `size = 2`
- 保持 `hasPower = false`
- `heatingRate = -0.4f`
- 材料改为：`copper 80, lead 50`

理由：
- 这是最早期的被动散热块。
- 无电意味着必须弱，不能接近主动冷却器效率。

#### air-cooler
路径：`voidshield.content.VSBlocks.airCooler`

建议：
- 保持 `size = 3`
- `consumePower(2.5f)`
- `heatingRate = -1.5f`
- 材料改为：`copper 120, lead 90, graphite 60, silicon 40`

理由：
- 功能定位应接近“中期主动冷却设备”。
- 功耗应略高于 `cryofluidMixer`，但低于 `segment` 的 8f。
- 比 `heat-sink` 强约 3~4 倍，形成明确升级。

#### evaporative-cooler
路径：`voidshield.content.VSBlocks.evaporativeCooler`

建议：
- 保持 `size = 4`
- `consumePower(4.5f)`
- `consumeLiquid(Liquids.water, 0.2f)`
- `consumeLiquid(Liquids.cryofluid, 0.08f)`
- `heatingRate = -3.5f`
- 材料改为：`lead 180, silicon 80, titanium 90, metaglass 60`

理由：
- 这是后期高效冷却器，应明显强于 `air-cooler`。
- 但当前 `water 2f + cryofluid 0.8f` 太夸张，后勤压力远超常规原版液体机器。
- 参考 `cryofluidMixer` 的流量，建议降到当前值的十分之一量级，否则只是“很强但根本供不起”。

### 二、热传输件

#### heat-catheter
路径：`voidshield.content.VSBlocks.heatCatheter`

建议：
- 保持 `size = 1`
- 保持 `specificHeat = 1f`
- 材料改为：`copper 12, lead 8`
- `health = 60`

理由：
- 这是最基础的直线导热件，成本必须接近原版管线件，而不是 150/100 这种建筑级成本。

#### heat-router
路径：`voidshield.content.VSBlocks.heatRouter`

建议：
- 保持 `size = 1`
- 保持 `specificHeat = 1f`
- `health = 90`
- 材料改为：`copper 20, lead 15, graphite 10`

理由：
- 四向分流比直通导管更强，应更贵。

#### heat-crossover
路径：`voidshield.content.VSBlocks.heatCrossover`

建议：
- 保持 `size = 1`
- 保持 `specificHeat = 1f`
- `health = 80`
- 材料改为：`copper 18, lead 18, silicon 10`

理由：
- 交叉件避免线路互相污染，功能复杂度高于基础导管。
- 和 `router` 应同一档略偏贵，取决于你更看重布线自由还是多向均热。

### 三、voidshield 立场系

#### micro-void
路径：`voidshield.content.VSBlocks.microVoid`
类：`voidshield.world.blocks.voidshield.MicroVoid`

建议：
- 保持 `size = 5`
- `consumePower(7f)`
- `maxFissureCount = 24`
- `maxArea = 28f`
- `defaultHeat = 4f`
- `specificHeat = 12f`
- 材料改为：`lead 180, silicon 120, titanium 90, phaseFabric 25`

理由：
- 它的功能更接近 `segment` 与小护盾混合体，应拿 `segment` 的 `8f` 作为主要功耗锚点。
- 因为贴图绑定，尺寸不能下调，所以改用压缩 `maxFissureCount` 和 `maxArea` 的方式保住“小型拦截立场”定位。

#### velum-solvent
路径：`voidshield.content.VSBlocks.velumSolvent`
类：`voidshield.world.blocks.voidshield.VelumSolvent`

建议：
- 保持 `size = 5`
- `consumePower(12f)`
- `maxFissureCount = 72`
- `maxArea = 144`
- `defaultHeat = 6f`
- `specificHeat = 14f`
- 材料改为：`lead 300, silicon 220, titanium 180, phaseFabric 80`

理由：
- 这是主力中大型区域立场建筑，参考 `ForceProjector` / `BaseShield` 做放大版定价。
- 相比 `micro-void`，它的覆盖规模和可维护空间明显更大，功耗和造价都该上一个台阶。
- `specificHeat` 提高一点，避免因为覆盖规模扩大导致自己过快热崩。

#### cor-vacuum
路径：`voidshield.content.VSBlocks.corVacuum`
类：`voidshield.world.blocks.voidshield.CorVacuum`

建议：
- 保持 `size = 8`
- `consumePower(18f)`
- `specificHeat = 18f`
- `maxTemperature = 2400f`
- 材料改为：`lead 700, silicon 500, titanium 350, phaseFabric 180, surgeAlloy 120`

理由：
- 8x8 体积必须对应终局价位。
- 但由于当前类实现几乎没有专属功能，不能把功耗和造价拉到“超级武器”级别。先定成“高端平台型底座”更稳。
- 如果以后给它真正的大范围吸弹/扭曲/反击逻辑，再继续上调。

## 一版推荐梯度总表

| 建筑 | 定位 | size | 功耗 | 其他消耗 | 关键热参数 | 建议材料 |
|---|---|---:|---:|---|---|---|
| heat-sink | 早期被动散热 | 2 | 0 | 无 | `heatingRate=-0.4` | copper 80, lead 50 |
| air-cooler | 中期主动散热 | 3 | 2.5 | 无 | `heatingRate=-1.5` | copper 120, lead 90, graphite 60, silicon 40 |
| evaporative-cooler | 后期高效散热 | 4 | 4.5 | water 0.2, cryo 0.08 | `heatingRate=-3.5` | lead 180, silicon 80, titanium 90, metaglass 60 |
| heat-catheter | 基础导热 | 1 | 0 | 无 | `specificHeat=1` | copper 12, lead 8 |
| heat-router | 四向路由 | 1 | 0 | 无 | `specificHeat=1` | copper 20, lead 15, graphite 10 |
| heat-crossover | 交叉布线 | 1 | 0 | 无 | `specificHeat=1` | copper 18, lead 18, silicon 10 |
| micro-void | 小型拦截立场 | 5 | 7 | 无 | `maxFissureCount=24`, `maxArea=28`, `specificHeat=12` | lead 180, silicon 120, titanium 90, phaseFabric 25 |
| velum-solvent | 主力区域立场 | 5 | 12 | 无 | `maxFissureCount=72`, `maxArea=144`, `specificHeat=14` | lead 300, silicon 220, titanium 180, phaseFabric 80 |
| cor-vacuum | 终局平台装置 | 8 | 18 | 无 | `specificHeat=18`, `maxTemperature=2400` | lead 700, silicon 500, titanium 350, phaseFabric 180, surgeAlloy 120 |

## 关键判断

- **不建议** 直接照原版功耗抄死，因为 VoidShield 这些块带自定义热系统，强度不是线性对应原版。
- **建议** 用原版做“量级锚点”，再按你这套热系统补偿。
- 第一轮平衡先改：`功耗 / 需求 / 冷却效率 / 区域上限`。
- 第二轮平衡再看实战：是否因为 `HeatBlock.transferHeat()` 传热太快，导致散热器全部超模。

## 相关

- [VoidShield](../entities/voidshield.md) — 项目实体页
- [VoidShield VSBlocks 建筑配置总览](voidshield-vsblocks-building-configs.md) — 当前原始配置
- [Mindustry 钍反防炸逻辑](../concepts/mindustry-thorium-reactor-safety-logic.md) — 热系统平衡时可参考的供给与安全冗余原则
