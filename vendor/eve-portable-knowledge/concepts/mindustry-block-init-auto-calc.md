# Mindustry Block.init() 自动计算逻辑

> Source: `源码和示例/Mindustry/core/src/mindustry/world/Block.java` 第1318~1450行
> 验证版本：v146

## 自动计算顺序

Block.init() 中的自动计算按以下顺序执行：

### 1. 音效默认值
```java
// destroySound / placeSound / breakSound 根据 size 自动选择
if(destroySound == Sounds.unset){
    destroySound = size >= 3 ? Sounds.blockExplode3 :
                   size >= 2 ? RandomSound(blockExplode2, blockExplode2Alt) :
                   RandomSound(blockExplode1, blockExplode1Alt);
}
```

### 2. health 自动计算（当 health == -1 时）
```java
if(health == -1){
    boolean round = false;
    if(scaledHealth < 0){
        scaledHealth = 40;  // 默认缩放血量
        float scaling = 1f;
        for(var stack : requirements){
            scaling += stack.item.healthScaling;  // 物品血量加成
        }
        scaledHealth *= scaling;
        round = true;
    }
    health = round ?
        Mathf.round(size * size * scaledHealth, 5) :  // 四舍五入到5
        (int)(size * size * scaledHealth);
}
```

**计算公式**：`health = size² × scaledHealth`

**scaledHealth 来源**：
- 如果 JSON 未设置 scaledHealth（< 0）：默认 40，乘以 `1 + sum(item.healthScaling)`
- 如果 JSON 设置了 scaledHealth（>= 0）：直接使用

**关键**：
- `health = -1` 且 `scaledHealth = -1`：自动从 requirements 计算
- `health = -1` 且 `scaledHealth >= 0`：用 scaledHealth × size²
- `health >= 0`：不自动计算，直接使用

### 3. liquidCapacity 自动计算（当 liquidCapacity < 0 时）
```java
if(liquidCapacity < 0){
    float consumeAmount = 1f;
    for(var cons : consumeBuilder){
        if(cons instanceof ConsumeLiquidBase liq){
            consumeAmount = Math.max(consumeAmount, liq.amount * 60f);
        }else if(cons instanceof ConsumeLiquids liq){
            for(var stack : liq.liquids){
                consumeAmount = Math.max(consumeAmount, stack.amount * 60f);
            }
        }
    }
    liquidCapacity = Mathf.round(10f * consumeAmount);
}
```

**计算公式**：`liquidCapacity = round(10 × maxConsumerAmount × 60)`

**关键**：
- 扫描所有 ConsumeLiquidBase 和 ConsumeLiquids 消费者
- 取最大消耗量 × 60（转为每秒）
- 乘以 10（留 10 秒缓冲）
- 如果没有液体消费者，consumeAmount=1，liquidCapacity=60

### 4. buildTime 自动计算（当 buildTime < 0 时）
```java
if(requirements.length > 0 && buildTime < 0){
    buildTime = 0f;
    for(ItemStack stack : requirements){
        buildTime += stack.amount * stack.item.cost;
    }
}
if(buildTime < 0){
    buildTime = 20f;  // 无需求物品时的默认值
}
buildTime *= buildCostMultiplier;
```

**计算公式**：`buildTime = sum(itemAmount × item.cost) × buildCostMultiplier`

**关键**：
- 有 requirements：sum(amount × cost)
- 无 requirements 且 buildTime=-1：默认 20f
- 最终乘以 buildCostMultiplier（默认 1f）

### 5. 偏移量计算
```java
offset = ((size + 1) % 2) * tilesize / 2f;   // tilesize=8
sizeOffset = -((size - 1) / 2);
```

**规则**：
- 偶数 size：offset = 1 × 4 = 4f（偏移半个格子）
- 奇数 size：offset = 0（居中）

### 6. 其他自动设置
```java
// 目标优先级
if(underBullets) priority = TargetPriority.under;

// 雾半径标志
if(fogRadius > 0) flags = flags.with(BlockFlag.hasFogRadius);

// 同步标志
if(sync) flags = flags.with(BlockFlag.synced);

// 发光（有液体且绘制液态光）
if(hasLiquids && drawLiquidLight){
    emitLight = true;
}

// 运输方块自动接受物品
if(group == BlockGroup.transportation || category == Category.distribution){
    acceptsItems = true;
}
```

### 7. 消费者初始化
```java
consumers = consumeBuilder.toArray();
optionalConsumers = select(optional && !ignore);
nonOptionalConsumers = select(!optional && !ignore);
updateConsumers = select(update && !ignore);

// 关键：遍历所有消费者，调用 apply()
for(Consume cons : consumers){
    cons.apply(this);
}
// ConsumePower.apply() → block.hasPower = true, block.consPower = this
// ConsumeLiquidBase.apply() → block.hasLiquids = true
// ConsumeItems.apply() → block.hasItems = true, block.acceptsItems = true
```

## 自动计算汇总表

| 字段 | 默认值 | 自动计算条件 | 计算公式 |
|------|--------|-------------|----------|
| health | -1 | health == -1 | size² × scaledHealth |
| scaledHealth | -1 | health == -1 且 scaledHealth < 0 | 40 × (1 + sum(healthScaling)) |
| liquidCapacity | -1f | liquidCapacity < 0 | round(10 × maxLiquidConsume × 60) |
| buildTime | -1f | buildTime < 0 且有 requirements | sum(amount × cost) × buildCostMultiplier |
| buildTime | -1f | buildTime < 0 且无 requirements | 20f |
| offset | 0 | 自动 | ((size+1)%2) × 4f |
| hasPower | false | ConsumePower.apply() | true |
| hasLiquids | false | ConsumeLiquidBase.apply() | true |
| hasItems | false | ConsumeItems.apply() | true |
| acceptsItems | false | ConsumeItems.apply() 或 category=distribution | true |

## JSON 常见误区

| 误区 | 正确 |
|------|------|
| 手动设置 health=640 | 可以，但不如设 scaledHealth+size 自动算 |
| 手动设置 liquidCapacity | 通常不需要，自动从消费者计算 |
| 手动设置 buildTime | 通常不需要，自动从 requirements 计算 |
| 手动设置 hasPower | ConsumePower.apply() 自动设置 |
| 手动设置 hasLiquids | ConsumeLiquidBase.apply() 自动设置 |
| 奇数 size 需要设 offset | 不需要，自动计算 |
| 偶数 size 需要设 offset | 不需要，自动计算 |
