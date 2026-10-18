# Mindustry 原版发射台评估系统

> Source: 对原版 Mindustry 155.4 源码的阅读与整理。

原版战役里的“发射台相关评估系统”，本质上不是单独的一套 AI 评分器，而是由 **发射台导出统计 + 星区信息滑动平均 + 战役回合离线结算 + 着陆台按需求拉货** 共同组成的。

## 核心结论

- `LaunchPad` 本身只负责把当前发出去的物品记为“出口统计”。
- `SectorInfo` 维护 `export`、`imports`、`production`、`rawProduction` 等统计，并用 `WindowedMean` 做滑动平均。
- `destination` 决定一个星区把导出物资发往哪个目标星区。
- `Universe.runTurn()` 在离线战役回合中，按 `export.mean * secondsPassed` 把物资结算到目标星区。
- `LandingPad` 不直接决定“该不该送”，而是按目标星区的可导入速率、冷却计时与配置的物品种类来触发着陆。
- 所谓“评估”，原版更接近 **吞吐量估算与离线模拟**，而不是复杂优先级求解器。

## 运作链路

### 1. 发射台发射时记录导出量

`LaunchPad.LaunchPadBuild.updateTile()` 在发射条件满足时：

- 检查 `launchCounter >= launchTime`
- 检查 `items.total() >= itemCapacity`
- `consume()`
- 生成 `LaunchPayload`
- 清空内部物品

真正把“发出了多少物资”记进战役统计的入口不在 `LaunchPad` 这个类里显式调用，而是在物资离开本星区/进入统计流程时，由 `SectorInfo.handleItemExport()` 记到：

- `SectorInfo.export[item].counter += amount`

这份 `counter` 不是最终速率，而是一个刷新周期内累计值。

### 2. 星区信息层维护统计量

`SectorInfo` 里有几组关键统计：

- `production`：核心收支后的净产出
- `rawProduction`：工厂原始产出
- `export`：发射台导出统计
- `imports`：着陆台实际收到的导入统计

它每隔 `refreshPeriod = 60` tick 刷新一次统计，并把本周期的 `counter` 放进 `WindowedMean(valueWindow = 60)`：

- `stat.means.add(max(stat.counter, 0))`
- `stat.mean = stat.means.rawMean()`

因此 `mean` 表示的是：

- **每个刷新周期平均导出多少**
- 当前注释说明这个量等价于“当前按秒计的平均值”

也就是说，原版不是瞬时读数，而是 **60 个样本窗口的滑动平均**，用来平滑短期抖动。

### 3. 导出速率会被生产能力钳制

`SectorInfo.update()` 里会对统计进一步约束：

- `production.mean <= rawProduction.mean`
- `export.mean <= rawProduction.mean + max(-production.mean, 0)`

这句很关键：

- 纯导出不能超过原始生产能力
- 如果核心库存正在下降，允许把“从核心里拿出来的量”也算进可导出的上限

所以原版的“评估系统”不是无限相信发射记录，而是会结合生产/库存变化来修正导出能力。

### 4. 目标星区的进口速率由所有来源星区汇总

`SectorInfo.refreshImportRates(planet)` 会遍历整颗星球所有星区：

- 找出 `sector.info.destination == this` 的来源星区
- 把这些来源星区的 `export.mean` 按物品求和

于是目标星区得到：

- `importRateCache[item.id] += source.export.mean`

这代表：

- 某个目标星区对某物品的理论最大进口速率
- 它不是单个着陆台的值，而是 **所有把目的地指向这里的星区总和**

### 5. 着陆台按进口速率和冷却节奏落货

`LandingPad` 的逻辑更像“消费端调度器”：

- 每个着陆台配置一个目标物品 `config`
- `updateTimers()` 读取 `state.rules.sector.info.getImportRates()`
- 对每种物品算出：`framesBetweenArrival = itemCapacity / importedPerFrame`
- 再累计 `importCooldownTimers[item]`

当满足以下条件时，着陆台会排队落货：

- `cooldown <= 0`
- `efficiency > 0`
- 自己目前没有货
- `getImportRate(...) > 0`
- `importCooldownTimers[config] >= 1`

落货完成后：

- 目标着陆台直接得到 `itemCapacity` 数量的该物品
- 记一次 `handleItemImport(config, itemCapacity)`

这说明原版并不是逐个发射舱精确追踪飞行路径，而是：

- 上游统计一个平均出口速率
- 下游按照这个平均速率分批生成到货事件

### 6. 离线战役回合按平均值直接结算

`Universe.runTurn()` 是“真正离线模拟”的关键。

当行星启用 `legacyLaunchPads` 时：

- 先清空各星区 `lastImported`
- 再遍历所有非当前游玩、未被攻击的基地星区
- 如果该星区设置了 `destination`
- 就按 `export.mean * newSecondsPassed` 计算这次回合期间应送出的物品数

然后：

- 加到目标星区库存 `to.addItems(items)`
- 记录到 `to.info.lastImported`

因此离线状态下，发射台系统不是逐台模拟，而是 **按统计均值批量结算**。

## 为什么会感觉它像“评估系统”

因为它确实在做几层估算：

1. 用一段时间的历史出口记录估算长期吞吐
2. 用生产/净产出关系修正这个估算
3. 用所有来源星区对目标星区汇总出理论进口能力
4. 用目标着陆台的冷却与容量，把连续速率离散成一批一批到货
5. 在离线模式里直接按均值乘时间进行结算

所以更准确地说，它是：

- **物流吞吐评估系统**
- **跨星区导入导出模拟系统**
- 而不是传统意义上的“数值评分器”

## 关键限制与特性

- 这是平均值系统，不是逐发精确模拟。
- `destination` 是按星区级别设置的，不是每个发射台独立目的地。
- 导出会受净产出/原始产出约束。
- 导入上限来自所有来源星区的 `export.mean` 汇总。
- `SectorInfo` 源码里还直接写了一个 TODO，说明 **发射后再重定向目的地** 存在可利用问题，意味着该系统本身就是偏近似模型。

## 如果你想复刻原版思路

最核心的不是复刻发射动画，而是复刻这 4 个数据层：

1. `export counter`
2. `WindowedMean` 平滑统计
3. `destination -> importRate 汇总`
4. `mean * elapsedTime` 的离线结算

这样做出来的行为才会接近原版战役发射台网络。

## Related

- [Campaign System](../mindustry/campaign-system.md) — 发射台与战役星区离线更新属于战役系统的一部分
- [Save and Load System](../mindustry/save-load-system.md) — 星区信息与统计会被保存并参与离线结算
- [Mindustry Reference Mod](../entities/mindustry-reference-mod.md) — 本地可用的原版源码与参考资源
