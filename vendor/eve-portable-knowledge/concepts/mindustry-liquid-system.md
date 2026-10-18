# Mindustry 液体系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/world/blocks/liquid/` (6文件) + `BuildingComp.java` 液体方法 + `LiquidModule.java` + `Liquid.java`
> 验证版本：v146 (本地源码)

## 继承链

```
Block (1662行)
  └─ LiquidBlock (97行) — update=true, solid=true, hasLiquids=true, group=BlockGroup.liquids, outputsLiquid=true
       ├─ Conduit (268行) — rotate=true, solid=false, floating=true, canOverdrive=false
       │    └─ ArmoredConduit (48行) — leaks=false
       ├─ LiquidRouter (52行) — solid=true, canOverdrive=false, floating=true
       ├─ LiquidJunction (51行) — floating=true
       └─ LiquidBridge (25行) — 继承 ItemBridge, hasItems=false, hasLiquids=true, outputsLiquid=true, canOverdrive=false
```

## LiquidBlock 基类

```java
update = true;
solid = true;
hasLiquids = true;
group = BlockGroup.liquids;
outputsLiquid = true;
envEnabled |= Env.space | Env.underwater;
```

### 贴图加载
- `@-liquid` — 液体显示贴图
- `@-top` — 顶层贴图
- `@-bottom` — 底层贴图

## Conduit（管道）

### 关键字段
| 字段 | 默认值 | 说明 |
|------|--------|------|
| rotate | true | 有方向 |
| solid | false | 不阻挡子弹 |
| floating | true | 漂浮 |
| padCorners | true | 拐角液体区域裁剪 |
| leaks | true | 无接头时泄漏液体 |
| canOverdrive | false | 不受超速影响 |

### 核心逻辑

```java
// acceptLiquid — 接受液体条件
(liquids.current() == liquid || liquids.currentAmount() < 0.2f)  // 同种液体或未满
    && (source == this || (source.relativeTo(tile) + 2) % 4 != rotation)  // 不接受从前方来的液体

// updateTile — 每tick
if(timer(timerFlow, 1)){
    moveLiquidForward(leaks, liquids.current());  // 向前方推送
}
```

### 替换逻辑
- 正交交叉 → 自动替换为 `LiquidJunction`
- 长距离直线 → 自动替换为 `bridgeReplacement`（默认 `bridgeConduit`）

### 贴图加载
- `@-top-#`（5张动画帧）
- `@-bottom-#`（5张动画帧）
- `@-cap` — 端帽贴图

## ArmoredConduit

```java
leaks = false;  // 不泄漏
```

## LiquidRouter（液体路由器）

```java
// updateTile — 每tick
dumpLiquid(liquids.current());  // 向所有邻接建筑推送

// acceptLiquid
(liquids.current() == liquid || liquids.currentAmount() < 0.2f)  // 同种或未满
```

**关键**：LiquidRouter 是全向推送，不像 Conduit 只向前方推送

## LiquidJunction（液体交叉）

```java
// 无 liquidCapacity 显示，无液体 bar
// 核心：getLiquidDestination 透传
public Building getLiquidDestination(Building source, Liquid liquid){
    int dir = (source.relativeTo(tile) + 4) % 4;  // 来源方向
    Building next = nearby(dir);  // 对面建筑
    return next.getLiquidDestination(this, liquid);  // 递归透传
}
```

**关键**：LiquidJunction 不存储液体，只是让两条管道交叉不混合

## LiquidBridge（液体桥）

```java
// 继承 ItemBridge
hasItems = false;
hasLiquids = true;
outputsLiquid = true;
canOverdrive = false;

// updateTransport — warmup >= 0.25f 后传输
moved |= moveLiquid(other, liquids.current()) > 0.05f;
```

## LiquidModule（液体模块）

### 数据结构
```java
private float[] liquids;  // 所有液体类型的数量数组
private Liquid current;   // 当前主要液体（最后添加的）
```

### 关键方法
| 方法 | 说明 |
|------|------|
| `add(liquid, amount)` | 添加液体，current 更新为最后添加的 |
| `remove(liquid, amount)` | 移除液体，cap 防止负数 |
| `get(liquid)` | 获取某液体数量 |
| `current()` | 返回最后添加的液体类型 |
| `currentAmount()` | 返回 current 液体的数量 |
| `clear()` | 清空所有液体 |

### 流速计算
- 使用 `WindowedMean` 窗口均值（windowSize=3）
- `getFlowRate()` 返回 u/s（已乘60）
- `pollScl = 20f` — 每20tick采样一次

## Liquid（液体类型）

### 关键字段
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| gas | boolean | false | 是否为气体 |
| flammability | float | 0 | 可燃性 |
| temperature | float | 0.5f | 温度 |
| heatCapacity | float | 0.5f | 热容量 |
| viscosity | float | 0.5f | 粘度 |
| explosiveness | float | 0 | 爆炸性 |
| blockReactive | boolean | true | 是否参与液体反应 |
| coolant | boolean | true | 是否可用作冷却剂 |
| moveThroughBlocks | boolean | false | 是否穿过方块 |
| incinerable | boolean | true | 是否可焚烧 |
| boilPoint | float | 2f | 沸点 |
| capPuddles | boolean | true | 是否限制水坑大小 |

## BuildingComp 液体传输方法

### acceptLiquid（接受液体）
```java
return block.hasLiquids && block.consumesLiquid(liquid);
```

### dumpLiquid（向邻接推送）
```java
// 遍历 proximity，按 cdump 轮转
for(int i = 0; i < proximity.size; i++){
    Building other = proximity.get((i + dump) % proximity.size);
    other = other.getLiquidDestination(self(), liquid);
    // 按比例推送：fract - ofract
    if(ofract < fract) transferLiquid(other, (fract - ofract) * capacity / scaling, liquid);
}
```

### moveLiquid（向指定建筑推送）
```java
// 压力差驱动
float fract = liquids.get(liquid) / block.liquidCapacity * block.liquidPressure;
float ofract = next.liquids.get(liquid) / next.block.liquidCapacity;
float flow = clamp((fract - ofract)) * block.liquidCapacity;

// 液体反应（blockReactive && blockReactive）
// 可燃+高温 → 着火
// 高温+低温 → 蒸汽（消耗液体）
```

### moveLiquidForward（向前方推送）
```java
Tile next = tile.nearby(rotation);
// 有建筑 → moveLiquid
// 无建筑 && leaks && 非固体 && 非液体方块 → 泄漏为 Puddle
```

### 液体反应规则
| 条件 | 效果 |
|------|------|
| flammability > 0.3 && temperature > 0.7 | 着火 (damageContinuous) |
| temperature > 0.7 && other.temperature < 0.55 | 蒸汽 (消耗液体) |

## 关键规则

| 规则 | 说明 |
|------|------|
| LiquidBlock 默认 outputsLiquid=true | 构造函数直接设置 |
| Conduit 默认 solid=false | 不阻挡子弹，floating=true |
| Conduit 默认 leaks=true | ArmoredConduit 设为 false |
| Conduit 默认 canOverdrive=false | 不受超速影响 |
| Conduit acceptLiquid 不接受前方来液 | (source.relativeTo + 2) % 4 != rotation |
| Conduit timerFlow=1 每tick推送 | 不是每秒 |
| LiquidRouter 全向推送 | dumpLiquid 遍历所有 proximity |
| LiquidJunction 不存储液体 | 透传 getLiquidDestination |
| LiquidBridge 继承 ItemBridge | hasItems=false, hasLiquids=true |
| LiquidModule current = 最后添加的液体 | 不是最多的液体 |
| 液体传输由压力差驱动 | fract * liquidPressure |
| liquidPressure 默认 1f | Block.java |
| liquidCapacity 默认 -1 自动计算 | Block.init() 中按 consumeAmount 计算 |
| 液体反应条件：blockReactive=true | Liquid 字段，默认 true |

## JSON 常见误区

| 误区 | 正确 |
|------|------|
| Conduit 设 solid=true | Conduit 默认 solid=false |
| LiquidRouter 设 outputsLiquid | 继承 LiquidBlock 自动设置 |
| 给 Conduit 设 canOverdrive=true | 默认 false，管道不受超速 |
| 认为 LiquidJunction 存储液体 | 不存储，只透传 |
| 认为 current 是最多的液体 | current 是最后添加的液体 |
| 忘记 leaks 属性 | Conduit 默认泄漏，ArmoredConduit 不泄漏 |

## 关联

- [Block and Building System](../mindustry/block-building-system.md) — Block.init() liquidCapacity 自动计算
- [Consume System](../concepts/mindustry-consume-system.md) — ConsumeLiquid/ConsumeLiquidFilter
- [Power System](../mindustry/power-system.md) — 对比电力/液体分配机制
