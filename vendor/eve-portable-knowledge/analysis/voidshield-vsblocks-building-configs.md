# VoidShield VSBlocks 建筑配置总览（不含动画）

## 来源
- 源码注册入口：`voidshield.content.VSBlocks`（`src/voidshield/content/VSBlocks.kt`）
- 基类：`voidshield.world.blocks.HeatBlock`（`src/voidshield/world/blocks/HeatBlock.kt`）
- 子类：`HeaterBlock`、`HeatCatheter`、`HeatCrossover`、`HeatRouter`、`VelumSolvent`、`MicroVoid`、`CorVacuum`

## 统一继承默认值
所有这些建筑都继承自 `voidshield.world.blocks.HeatBlock`。
基类默认配置：
- `update = true`
- `solid = true`
- `hasPower = true`
- `consumesPower = true`
- `maxTemperature = 2000f`
- `specificHeat = 10f`
- `overheatDamage = 0.5f`
- `overheatThreshold = 0.9f`
- `drawer = DrawDefault()`
- `rate = 0.9f`
- `cooldown = true`
- `warmupSpeed = 0.019f`

## VSBlocks 内已注册建筑

### 1. heat-sink
- 类型：`voidshield.world.blocks.heat.HeaterBlock`
- 注册变量：`VSBlocks.heatSink`
- 配置：
  - `size = 2`
  - `hasPower = false`
  - `maxTemperature = 2000f`
  - `heatingRate = -0.5f`
  - `overheatDamage = 1f`
  - `overheatThreshold = 0.9f`
  - `requirements(voidShield, copper 150, lead 100)`
- 含义：负 `heatingRate`，属于降温器。

### 2. air-cooler
- 类型：`voidshield.world.blocks.heat.HeaterBlock`
- 注册变量：`VSBlocks.airCooler`
- 配置：
  - `size = 3`
  - `consumePower(300f)`
  - `maxTemperature = 2000f`
  - `heatingRate = -2f`
  - `overheatDamage = 1f`
  - `overheatThreshold = 0.9f`
  - `requirements(voidShield, copper 150, lead 100)`
- 含义：吃电降温器。

### 3. evaporative-cooler
- 类型：`voidshield.world.blocks.heat.HeaterBlock`
- 注册变量：`VSBlocks.evaporativeCooler`
- 配置：
  - `size = 4`
  - `maxTemperature = 2000f`
  - `heatingRate = -5f`
  - `overheatDamage = 1f`
  - `overheatThreshold = 0.9f`
  - `requirements(voidShield, copper 150, lead 100)`
  - `consumePower(8f)`
  - `consumeLiquid(Liquids.water, 2f)`
  - `consumeLiquid(Liquids.cryofluid, 0.8f)`
- 含义：耗电 + 双液体降温器。

### 4. heat-catheter
- 类型：`voidshield.world.blocks.heat.HeatCatheter`
- 注册变量：`VSBlocks.heatCatheter`
- 配置：
  - `size = 1`
  - `hasPower = false`
  - `maxTemperature = 2000f`
  - `overheatDamage = 1f`
  - `overheatThreshold = 0.9f`
  - `specificHeat = 1f`
  - `requirements(voidShield, copper 150, lead 100)`
- 类默认补充：
  - `rotate = true`
  - `update = true`
  - `group = BlockGroup.logic`
  - `specificHeat = 1f`
- 含义：热导管，前后传热。

### 5. heat-crossover
- 类型：`voidshield.world.blocks.heat.HeatCrossover`
- 注册变量：`VSBlocks.heatCrossover`
- 配置：
  - `size = 1`
  - `health = 100`
  - `hasPower = false`
  - `specificHeat = 1f`
  - `requirements(voidShield, copper 150, lead 100)`
- 含义：热交叉节点。只覆写了状态条，不额外定义复杂配置。

### 6. heat-router
- 类型：`voidshield.world.blocks.heat.HeatRouter`
- 注册变量：`VSBlocks.heatRouter`
- 配置：
  - `size = 1`
  - `health = 100`
  - `specificHeat = 1f`
  - `hasPower = false`
  - `requirements(voidShield, copper 150, lead 100)`
- 类默认补充：
  - `rotate = false`
- 含义：四向热路由。

### 7. velum-solvent
- 类型：`voidshield.world.blocks.voidshield.VelumSolvent`
- 注册变量：`VSBlocks.velumSolvent`
- 配置：
  - `size = 5`
  - `requirements(voidShield, copper 150, lead 100)`
  - `hasPower = true`
  - `consumePower(50f)`
- 类字段默认值：
  - `maxFissureCount = 100`
  - `maxArea = 200`
  - `defaultHeat = 5f`
- 类 init 补充：
  - `update = true`
  - `solid = true`
  - `hasPower = true`
  - `consumesPower = true`
- 含义：可维护 `spaces` 立场区域的 voidshield 建筑。

### 8. micro-void
- 类型：`voidshield.world.blocks.voidshield.MicroVoid`
- 注册变量：`VSBlocks.microVoid`
- 配置：
  - `size = 5`
  - `requirements(voidShield, copper 150, lead 100)`
  - `hasPower = true`
  - `consumePower(50f)`
- 类字段默认值：
  - `maxFissureCount = 50`
  - `maxArea = 50f`
  - `defaultHeat = 5f`
- 基于 `HeatBlock` 的默认值继续生效：
  - `specificHeat = 10f`
  - `maxTemperature = 2000f`
  - `overheatDamage = 0.5f`
  - `overheatThreshold = 0.9f`
- 含义：小型虚空立场建筑，可生成圆形区，拦截敌方子弹。

### 9. cor-vacuum
- 类型：`voidshield.world.blocks.voidshield.CorVacuum`
- 注册变量：`VSBlocks.corVacuum`
- 配置：
  - `size = 8`
  - `requirements(voidShield, copper 150, lead 100)`
  - `hasPower = true`
  - `consumePower(50f)`
- 类 init 补充：
  - `update = true`
  - `solid = true`
  - `hasPower = true`
  - `consumesPower = true`
- 含义：当前类本体配置很少，主要是 HeatBlock 基类 + VSBlocks 注册值。

## 未正式挂到变量的测试建筑
`VSBlocks.load()` 末尾还有一个未保存到字段的测试块：
- 类型：`HeaterBlock("test")`
- 配置：
  - `size = 2`
  - `consumePower(50f)`
  - `maxTemperature = 2000f`
  - `heatingRate = 5f`
  - `overheatDamage = 1f`
  - `overheatThreshold = 0.9f`
  - `requirements(voidShield, copper 150, lead 100)`

## 结论
VSBlocks 里真正有配置的建筑共 9 个正式建筑 + 1 个测试建筑。
如果你说“配置情况”，不算动画，核心就是这几类：
- 建筑尺寸 `size`
- 建造需求 `requirements`
- 电力/液体消耗 `consumePower` / `consumeLiquid`
- 热系统参数 `maxTemperature` / `specificHeat` / `heatingRate` / `overheatDamage` / `overheatThreshold`
- 类级行为开关 `rotate` / `hasPower` / `consumesPower` / `update` / `solid`
