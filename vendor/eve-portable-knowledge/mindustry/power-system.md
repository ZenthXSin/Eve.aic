# Mindustry 电力系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/world/blocks/power/` (17个文件, 3014行)
> 验证版本：v146 (本地源码)

## 继承链

```
Block (1662行)
  └─ PowerBlock (15行)          — update=true, solid=true, hasPower=true, group=BlockGroup.power
       ├─ PowerDistributor (10行) — consumesPower=false, outputsPower=true
       │    ├─ PowerGenerator (181行) — powerProduction, drawer, explosion
       │    │    ├─ ConsumeGenerator (174行) — itemDuration=120f, warmupSpeed=0.05f
       │    │    │    ├─ NuclearReactor (196行)
       │    │    │    └─ ImpactReactor (136行)
       │    │    ├─ ThermalGenerator (115行)
       │    │    ├─ HeaterGenerator (77行)
       │    │    └─ SolarGenerator (36行)
       │    └─ Battery (105行)    — consumesPower=true, update=false
       └─ PowerNode (519行)       — consumesPower=false, outputsPower=false, 中继节点
            └─ LongPowerNode (48行)
  └─ BeamNode (295行)            — 激光电力传输
```

## PowerBlock (基类)

```java
// PowerBlock 构造函数设置：
update = true;
solid = true;
hasPower = true;           // 所有电力方块默认有电力系统
group = BlockGroup.power;
```

**关键**：PowerBlock 构造时直接设置 `hasPower=true`，不依赖 ConsumePower.apply()

## PowerDistributor (发电机/电池基类)

```java
consumesPower = false;     // 发电机不消耗电力
outputsPower = true;       // 发电机输出电力
```

**继承链**：PowerGenerator → PowerDistributor → PowerBlock → Block
- 所有发电机自动有 `outputsPower=true, consumesPower=false`
- JSON 中不需要手动设置这两个字段

## PowerGenerator (发电机基类)

### 关键字段
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| powerProduction | float | 0 | 每tick产电量（满效时） |
| drawer | DrawBlock | DrawDefault | 渲染器 |
| explosionRadius | int | 12 | 爆炸半径 |
| explosionDamage | int | 0 | 爆炸伤害 |
| explodeEffect | Effect | Fx.none | 爆炸特效 |
| explosionMinWarmup | float | 0 | 爆炸最小预热 |

### GeneratorBuild 内部类
| 字段 | 说明 |
|------|------|
| generateTime | 生成时间计数器 |
| productionEfficiency | 生产效率 (0.0~1.0) |

```java
// 电力产出计算
public float getPowerProduction(){
    return enabled ? powerProduction * productionEfficiency : 0f;
}

// warmup 等于 productionEfficiency
public float warmup(){
    return enabled ? productionEfficiency : 0f;
}
```

## Battery (电池)

```java
// Battery 构造函数：
outputsPower = true;
consumesPower = true;       // 电池同时消耗和输出
canOverdrive = false;       // 不受超速影响
update = false;             // 电池不需要 update
flags = EnumSet.of(BlockFlag.battery);
```

**关键**：Battery 继承 PowerDistributor（outputsPower=true），但额外设置 `consumesPower=true`
- PowerGraph.add() 中：`outputsPower && consumesPower && buffered` → 加入 batteries 列表
- Battery 不 update，只在 PowerGraph.update() 中被动充放电

## PowerNode (电力中继)

### 关键字段
| 字段 | 默认值 | 说明 |
|------|--------|------|
| laserRange | 6 | 连接范围（格） |
| maxNodes | 3 | 最大连接数 |

```java
consumesPower = false;     // 中继不消耗
outputsPower = false;      // 中继不产出
```

## ConsumePower (电力消费者)

### 字段
| 字段 | 类型 | 说明 |
|------|------|------|
| usage | float | 每tick用电量 |
| capacity | float | 缓冲容量 |
| buffered | boolean | 是否缓冲模式 |

### apply() — 自动设置
```java
public void apply(Block block){
    block.hasPower = true;
    block.consPower = this;
}
```

### requestedPower() — 需求计算
```java
// 缓冲模式：按缺额请求
buffered ? (1f - entity.power.status) * capacity :
// 非缓冲模式：按 usage 请求
usage * (entity.shouldConsume() ? 1f : 0f);
```

### 效率计算
```java
public float efficiency(Building build){
    return build.power.status;  // 电力效率 = power.status
}
```

**关键**：非缓冲消费者的效率直接等于 `power.status`（0~1），由 distributePower() 设置

## PowerGraph (电力网络核心)

### 数据结构
```java
Seq<Building> producers;   // 发电建筑
Seq<Building> consumers;   // 耗电建筑
Seq<Building> batteries;   // 电池建筑
Seq<Building> all;         // 全部建筑
```

### 建筑分类逻辑 (add())
```java
if(outputsPower && consumesPower && !buffered)
    → producers + consumers    // 如：某些双向电力块
else if(outputsPower && consumesPower && buffered)
    → batteries                // 电池
else if(outputsPower)
    → producers                // 发电机
else if(consumesPower)
    → consumers                // 消费者
```

### update() — 每tick电力分配
```
1. 计算 powerNeeded = sum(consumer.requestedPower() * delta())
2. 计算 powerProduced = sum(producer.getPowerProduction() * delta())
3. 如果 powerNeeded > powerProduced:
   → useBatteries(差额) → 补充 powerProduced
4. 如果 powerProduced > powerNeeded:
   → chargeBatteries(差额)
5. distributePower(powerNeeded, powerProduced, charged)
```

### distributePower() — 覆盖率分配
```java
float coverage = produced / needed;  // 0.0~1.0

// 非缓冲消费者：
consumer.power.status = coverage;    // 直接设覆盖率

// 缓冲消费者：
float maximumRate = requestedPower * coverage * delta();
consumer.power.status += maximumRate / capacity;  // 按比例充电
```

**关键**：所有消费者共享同一个 coverage，不存在"先到先得"

### 电池充放电
```java
// 放电：按比例放电
float consumedPowerPercentage = min(1.0, needed / stored);
battery.power.status *= (1f - consumedPowerPercentage);

// 充电：按比例充电
float chargedPercent = min(excess / capacity, 1f);
battery.power.status += (1f - battery.power.status) * chargedPercent;
```

### 图合并/分裂
- `addGraph()` — 合并两个图（小图合并到大图）
- `remove()` — 移除节点后 BFS 重建新图
- `reflow()` — BFS 重新发现所有连接

### 节能模式
```java
// cheating 模式（沙箱）：所有消费者直接 status=1
if(consumers.first().cheating()){
    for(Building tile : consumers) tile.power.status = 1f;
}
```

### 满意度
```java
getSatisfaction() = lastPowerProduced / lastPowerNeeded
// 如果无生产：返回 0
// 如果无需求：返回 1
```

## 关键规则

| 规则 | 说明 |
|------|------|
| PowerBlock 默认 hasPower=true | 构造函数直接设置，不依赖 ConsumePower |
| PowerDistributor 默认输出电力 | consumesPower=false, outputsPower=true |
| Battery 不需要 update | update=false，被动参与 PowerGraph |
| PowerNode 不消耗不输出 | consumesPower=false, outputsPower=false |
| ConsumePower.apply() 设置 hasPower | 和 consPower 引用 |
| 发电机产出 = powerProduction * productionEfficiency | productionEfficiency 由子类维护 |
| 非缓冲消费者效率 = coverage | coverage = produced / needed |
| 电池按比例充放电 | 不是先到先得 |
| 消费者始终先于电池 | 充电用剩余电力，放电补充不足 |
| buildTime=-1 自动计算 | sum(requirements * cost) * buildCostMultiplier |

## JSON 常见误区

| 误区 | 正确 |
|------|------|
| 手动设置 outputsPower | 发电机继承 PowerDistributor 自动设置 |
| 手动设置 consumesPower | ConsumePower.apply() 自动设置 hasPower |
| 给 PowerNode 设置 outputsPower | PowerNode 不输出不消耗 |
| 电池设 update=true | Battery 默认 update=false |
| 认为先到先得 | 所有消费者共享 coverage |
| 认为 powerProduction 是每秒 | 是每 tick，乘 60 才是每秒 |

## 关联

- [Power System](../concepts/mindustry-power-system.md) — 电力系统持久化补充
- [Block and Building System](block-building-system.md) — Block.init() 自动计算
- [Consume System](../concepts/mindustry-consume-system.md) — 消费者系统
