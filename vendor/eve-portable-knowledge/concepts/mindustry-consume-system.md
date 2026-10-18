# Mindustry 消费系统源码验证

> Source: `core/src/mindustry/world/consumers/` 24个类

## 继承链

```
Consume (基类)
├── ConsumeItems          # 物品消耗
├── ConsumeLiquids        # 多液体消耗
├── ConsumeLiquidBase     # 液体基类（amount字段）
│   ├── ConsumeLiquid     # 单液体消耗
│   └── ConsumeLiquidFilter  # 液体过滤器
│       └── ConsumeCoolant    # 冷却剂（maxTemp/maxFlammability）
├── ConsumePower          # 电力消耗（usage/capacity/buffered）
│   ├── ConsumePowerCondition  # 条件电力
│   └── ConsumePowerDynamic    # 动态电力
├── ConsumePayloads       # 载荷消耗
├── ConsumePayloadFilter  # 载荷过滤器
├── ConsumePayloadDynamic # 动态载荷
├── ConsumeItemFilter     # 物品过滤器
│   ├── ConsumeItemEfficiency  # 效率物品
│   │   ├── ConsumeItemCharged     # 充能物品
│   │   ├── ConsumeItemExplosive   # 爆炸物品
│   │   ├── ConsumeItemFlammable   # 易燃物品
│   │   └── ConsumeItemRadioactive # 放射性物品
│   └── ConsumeItemList    # 物品列表
├── ConsumeItemDynamic    # 动态物品
├── ConsumeItemExplode    # 爆炸消耗
├── ConsumeLiquidsDynamic # 动态液体
└── ConsumePayloadFilter  # 载荷过滤器
```

## Consume 基类字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `optional` | boolean | false | 是否可选 |
| `booster` | boolean | false | 是否为加速输入 |
| `update` | boolean | true | 是否自动更新 |
| `multiplier` | Floatf<Building> | b -> 1f | 消耗倍率 |

## ConsumePower 字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `usage` | float | 每tick电力消耗 |
| `capacity` | float | 电力容量 |
| `buffered` | boolean | 是否缓冲 |

**关键**：`ConsumePower.apply()` 自动设置 `block.hasPower = true`

## ConsumeLiquidBase 字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `amount` | float | 每帧消耗量 |

**关键**：`ConsumeLiquidBase.apply()` 自动设置 `block.hasLiquids = true`

## ConsumeCoolant 字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `maxTemp` | float | 0.5f | 最高温度 |
| `maxFlammability` | float | 0.1f | 最高可燃性 |
| `allowLiquid` | boolean | true | 允许液体 |
| `allowGas` | boolean | false | 允许气体 |

**过滤条件**：`liquid.coolant && (allowLiquid && !liquid.gas || allowGas && liquid.gas) && liquid.temperature <= maxTemp && liquid.flammability < maxFlammability`

## ConsumeItems 字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `items` | ItemStack[] | 物品需求 |

**关键**：`ConsumeItems.apply()` 自动设置 `block.hasItems = true` 和 `block.acceptsItems = true`

## ConsumeLiquids 字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `liquids` | LiquidStack[] | 液体需求 |

**关键**：`ConsumeLiquids.apply()` 自动设置 `block.hasLiquids = true`

## ConsumePayloads 字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `payloads` | Seq<PayloadStack> | 载荷需求 |

## 效率计算规则

### ConsumeItems
```java
return build.items.has(items, multiplier.get(build)) ? 1f : 0f;
```

### ConsumeLiquid
```java
return Math.min(build.liquids.get(liquid) / (amount * ed * multiplier.get(build)), 1f);
```

### ConsumeLiquids
```java
float min = 1f;
for(var stack : liquids){
    min = Math.min(build.liquids.get(stack.liquid) / (stack.amount * ed * mult), min);
}
return min;
```

### ConsumePower
```java
return build.power.status;
```

## JSON 中的 Consume

### 物品消耗
```json
"consumeItems": [{"item": "silicon", "amount": 2}]
```

### 单液体消耗
```json
"consumeLiquid": {"liquid": "water", "amount": 0.5}
```

### 多液体消耗
```json
"consumeLiquids": [{"liquid": "water", "amount": 0.5}, {"liquid": "cryofluid", "amount": 0.3}]
```

### 电力消耗
```json
"consumePower": 3.0
```

### 冷却剂
```json
"consumeCoolant": {"amount": 0.5, "maxTemp": 0.5, "maxFlammability": 0.1}
```

### 载荷消耗
```json
"consumePayloads": [{"item": "payload", "amount": 1}]
```

## 硬性规则

1. **amount 单位是每帧**（不是每秒），显示时乘以60
2. **ConsumePower.apply() 自动设置 hasPower**，不需要手动设置
3. **ConsumeLiquidBase.apply() 自动设置 hasLiquids**，不需要手动设置
4. **ConsumeItems.apply() 自动设置 hasItems 和 acceptsItems**
5. **ConsumeCoolant 继承 ConsumeLiquidFilter**，不是 ConsumeLiquid
6. **efficiency 是最小值**，不是平均值
7. **optional=true 的 Consume 不影响效率计算**
8. **booster=true 的 Consume 在 UI 中显示为加速输入**
9. **multiplier 用于动态调整消耗量**，默认 1f
10. **ConsumeLiquidFilter 可以匹配多种液体**，通过 filter 函数

## 常见误区

- ❌ `consumePower` 设置 `hasPower` → ✅ `ConsumePower.apply()` 自动设置
- ❌ `consumeLiquid` 设置 `hasLiquids` → ✅ `ConsumeLiquidBase.apply()` 自动设置
- ❌ `consumeItems` 设置 `hasItems` → ✅ `ConsumeItems.apply()` 自动设置
- ❌ `amount` 单位是每秒 → ✅ 单位是每帧（显示时乘以60）
- ❌ `ConsumeCoolant` 继承 `ConsumeLiquid` → ✅ 继承 `ConsumeLiquidFilter`
- ❌ `efficiency` 是平均值 → ✅ 是最小值

## 关联

- [Block Building System](mindustry-block-building-system.md)
- [Power System](mindustry-power-system.md)
- [Production Blocks](mindustry-production-blocks.md)
