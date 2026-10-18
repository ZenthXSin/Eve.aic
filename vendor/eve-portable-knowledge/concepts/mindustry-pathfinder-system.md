# Mindustry 寻路系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/ai/Pathfinder.java` 直接验证，2026-06-11

## 核心常量

```java
private static final long maxUpdate = Time.millisToNanos(8);  // 最大更新时间 8ms
private static final int updateFPS = 60;                       // 更新频率 60 FPS
private static final int updateInterval = 1000 / updateFPS;    // 更新间隔 ~16ms

public static final int fieldCore = 0;     // 核心流场 ID
public static final int maxFields = 10;    // 最大流场数

// 代价类型
public static final int
    costGround = 0,     // 地面
    costLegs = 1,       // 腿部
    costNaval = 2,      // 海军
    costNeoplasm = 3,   // 肿瘤
    costNone = 4,       // 无
    costHover = 5,      // 悬浮
    maxCosts = 8;       // 最大代价数
```

## 流场系统

Pathfinder 使用**流场**（Flowfield）寻路，不是 A*。

- 每个流场从目标向外扩散，计算每个瓦片的代价值
- 单位查询所在瓦片的代价，选择最低代价方向移动
- 支持多种代价类型（地面/海军/腿部/悬浮等）

## 地面代价计算

```java
// 地面单位代价
impassable if: allDeep(tile) || (team==tile.team && !teamPassable && solid)
cost = 1 + health*5 + nearSolid?2:0 + nearLiquid?6:0 + deep?6000:0 + damages?30:0

// 腿部单位代价
impassable if: legSolid(tile)
cost = 1 + deep?6000:0 + solid?5:0

// 海军代价
cost = (!liquid || (solid && (team==tile.team || team==0))) ? 7000 : 1
     + health*5 + (nearGround||nearSolid)?14:0 + deep?0:1 + damages?35:0
```

## 常见误区

- **Pathfinder 使用流场，不是 A***：所有单位共享流场
- **updateFPS = 60**：不是实时更新，是 60Hz 更新
- **maxUpdate = 8ms**：每帧最多更新 8ms，避免卡顿
- **maxFields = 10**：最多 10 个流场（不同目标）
- **maxCosts = 8**：最多 8 种代价类型
- **deep 水域代价 6000**：几乎不可通行，但不是 impassable
- **海军在陆地代价 7000**：几乎不可通行
- **impassable = -1**：不可通行标记

## Related

- [Mindustry AI System](../mindustry/ai-system.md)
- [Mindustry Unit System](mindustry-unit-system.md)
