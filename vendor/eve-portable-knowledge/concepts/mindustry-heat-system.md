# Mindustry 热系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/world/blocks/heat/` (4文件) + `BuildingComp.java` calculateHeat + `HeatCrafter.java`
> 验证版本：v146 (本地源码)

## 接口与类

```
HeatBlock (interface) — heat(), heatFrac()
HeatConsumer (interface) — sideHeat(), heatRequirement()

Block
  └─ GenericCrafter
       └─ HeatProducer — heatOutput=10f, warmupRate=0.15f, rotate=true, canOverdrive=false
  └─ HeatConductor — visualMaxHeat=15f, splitHeat=false, rotate=true, size=3
  └─ GenericCrafter
       └─ HeatCrafter — heatRequirement=10f, maxEfficiency=1.5f, overheatScale=0.5f

Building implements HeatBlock, HeatConsumer
```

## HeatBlock 接口

```java
public interface HeatBlock{
    float heat();           // 当前热量值
    float heatFrac();       // 热量/最大热量 (0~1)
}
```

## HeatConsumer 接口

```java
public interface HeatConsumer{
    float[] sideHeat();         // 四方向热量数组
    float heatRequirement();    // 需要的热量值
}
```

## HeatProducer（热发生器）

### 关键字段
| 字段 | 默认值 | 说明 |
|------|--------|------|
| heatOutput | 10f | 最大热量输出 |
| warmupRate | 0.15f | 预热速率 |
| rotate | true | 有方向 |
| canOverdrive | false | 不受超速影响 |
| drawer | DrawMulti(DrawDefault, DrawHeatOutput) | 渲染器 |

### 核心逻辑
```java
// HeatProducerBuild.updateTile()
super.updateTile();  // GenericCrafter 逻辑
heat = Mathf.approachDelta(heat, heatOutput * efficiency, warmupRate * delta());

// heat() → 返回 heat
// heatFrac() → 返回 heat / heatOutput
```

**关键**：HeatProducer 继承 GenericCrafter，但 `flags = EnumSet.of()`（不设任何标志），不计为标准 crafter

## HeatConductor（热传导器）

### 关键字段
| 字段 | 默认值 | 说明 |
|------|--------|------|
| visualMaxHeat | 15f | 显示用最大热量 |
| splitHeat | false | 是否分割热量 |
| rotate | true | 有方向 |
| size | 3 | 默认3x3 |
| drawer | DrawDefault | 渲染器 |

### 核心逻辑
```java
// HeatConductorBuild 同时实现 HeatBlock + HeatConsumer
implements HeatBlock, HeatConsumer

// updateTile → updateHeat()
heat = calculateHeat(sideHeat, cameFrom);

// heatFrac() → (heat / visualMaxHeat) / (splitHeat ? 3f : 1)
```

**关键**：splitHeat=true 时热量分3份，用于将热量均匀分配到3个出口

## HeatCrafter（热依赖工厂）

### 关键字段
| 字段 | 默认值 | 说明 |
|------|--------|------|
| heatRequirement | 10f | 需要的热量 |
| maxEfficiency | 1.5f | 最大效率（超热加成） |
| overheatScale | 0.5f | 超热效率缩放 |

### 核心逻辑
```java
// HeatCrafterBuild implements HeatConsumer
heat = calculateHeat(sideHeat);

// shouldConsume — 热量 > 0 才消耗
return (heatRequirement <= 0f || heat > 0) && super.shouldConsume();

// efficiency — 效率 = heat/heatRequirement + 超热加成
float over = Math.max(heat - heatRequirement, 0f);
return Math.min(heat/heatRequirement + over/heatRequirement * overheatScale, maxEfficiency);
```

**关键**：HeatCrafter 效率可以超过 1.0（最高 maxEfficiency=1.5），超热部分按 overheatScale 加成

## calculateHeat（热量计算核心）

### 位置
`BuildingComp.java` 第 406-456 行

### 算法
```java
public float calculateHeat(float[] sideHeat, @Nullable IntSet cameFrom){
    Arrays.fill(sideHeat, 0f);
    float heat = 0f;

    for(var build : proximity){
        if(build != null && build.team == team && build instanceof HeatBlock heater){
            // 方向检查：非路由器必须面向我们，路由器必须背向我们
            boolean split = build.block instanceof HeatConductor cond && cond.splitHeat;
            if(!build.block.rotate || 
               (!split && (relativeTo(build) + 2) % 4 == build.rotation) || 
               (split && relativeTo(build) != build.rotation)){
                
                // 环路检测：避免重复计算
                if(!(build instanceof HeatConductorBuild hc && hc.cameFrom.contains(id()))){
                    // 接触点数 = min(两个方块的半径和 - 距离, min(两个方块的size))
                    float diff = min(abs(build.x - x), abs(build.y - y)) / tilesize;
                    int contactPoints = min((int)(block.size/2f + build.block.size/2f - diff), min(build.block.size, block.size));
                    
                    // 热量 = heat / size * contactPoints
                    float add = heater.heat() / build.block.size * contactPoints;
                    if(split) add /= 3f;  // 路由器分3份
                    
                    sideHeat[mod(relativeTo(build), 4)] += add;
                    heat += add;
                }
            }
        }
    }
    return heat;
}
```

### 热量传输规则
| 规则 | 说明 |
|------|------|
| 必须同队 | build.team == team |
| 必须实现 HeatBlock | instanceof HeatBlock |
| 非路由器：必须面向接收方 | (relativeTo(build) + 2) % 4 == build.rotation |
| 路由器（splitHeat）：必须背向接收方 | relativeTo(build) != build.rotation |
| 环路检测 | cameFrom 集合避免重复计算 |
| 接触点影响传输量 | 大方块传输更多热量 |
| 路由器分割热量 | splitHeat=true 时热量 / 3 |

### 接触点计算
```
contactPoints = min(
    (int)(block.size/2f + build.block.size/2f - distance/tilesize),
    min(block.size, build.block.size)
)
```
- 两个方块相邻时，接触点 = min(两个方块的 size)
- 大方块（如3x3）接触小方块（如1x1）= 1 个接触点

## 关键规则

| 规则 | 说明 |
|------|------|
| HeatProducer 继承 GenericCrafter | 但 flags 清空，不计为 crafter |
| HeatProducer 默认 canOverdrive=false | 不受超速影响 |
| HeatProducer 预热 = heatOutput * efficiency | efficiency 来自 GenericCrafter |
| HeatConductor 默认 size=3 | 不是1 |
| HeatConductor splitHeat 默认 false | 不分割 |
| HeatCrafter 效率可超1.0 | maxEfficiency=1.5，默认 |
| HeatCrafter 超热加成 | overheatScale=0.5 |
| 热量按接触点数传输 | 大方块→小方块传输较少 |
| 路由器分3份 | splitHeat=true 时 |
| 环路检测通过 cameFrom | 避免无限循环 |
| 非路由器必须面向接收方 | rotate=true 时检查方向 |

## JSON 常见误区

| 误区 | 正确 |
|------|------|
| HeatProducer 设 canOverdrive=true | 默认 false，热发生器不受超速 |
| 认为热传输不需要面向 | 非路由器必须面向接收方 |
| 认为 splitHeat 默认 true | 默认 false |
| HeatCrafter 效率固定1.0 | 默认 maxEfficiency=1.5，可超热 |
| 忘记 HeatProducer 清空 flags | 不计为标准 crafter |

## 关联

- [Block and Building System](../mindustry/block-building-system.md) — Block 基类
- [Consume System](../concepts/mindustry-consume-system.md) — HeatCrafter 的消费系统
- [Production Blocks](../mindustry/production-blocks.md) — GenericCrafter, HeatCrafter
