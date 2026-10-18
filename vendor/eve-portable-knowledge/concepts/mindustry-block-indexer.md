# Mindustry 方块索引系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/ai/BlockIndexer.java` 直接验证，2026-06-11

## 核心常量

```java
private static final int quadrantSize = 20;  // 象限大小 20×20 瓦片
```

## 核心字段

```java
// 矿石索引（按物品 ID → 象限坐标 → 瓦片列表）
private IntSeq[][][] ores, wallOres;

// 受损建筑（按队伍）
private Seq<Building>[] damagedTiles = new Seq[Team.all.length];

// 所有存在的矿石
private Seq<Item> allPresentOres = new Seq<>();
private ObjectIntMap<Item> allOres = new ObjectIntMap<>(), allWallOres = new ObjectIntMap<>();

// 活跃队伍
private Seq<Team> activeTeams = new Seq<>(Team.class);

// 标志映射（队伍 → 标志 → 建筑列表）
private Seq<Building>[][] flagMap = new Seq[Team.all.length][BlockFlag.all.length];

// 方块存在标记
private boolean[] blocksPresent;

// 地板标志映射
private Seq<Tile>[] floorMap;
```

## 事件监听

```java
// 瓦片变化前：移除索引
Events.on(TilePreChangeEvent.class, event -> removeIndex(event.tile));

// 瓦片变化后：添加索引
Events.on(TileChangeEvent.class, event -> addIndex(event.tile));

// 地板变化：更新地板索引
Events.on(TileFloorChangeEvent.class, event -> {
    removeFloorIndex(event.tile, event.previous);
    addFloorIndex(event.tile, event.floor);
});

// 世界加载：重建所有索引
Events.on(WorldLoadEvent.class, event -> {
    // 重新初始化所有数组
    // 扫描所有瓦片建立索引
});
```

## 索引机制

### 矿石索引

- 按物品 ID 分类
- 使用象限（20×20 瓦片）分块
- 支持地板矿石和墙壁矿石

### 标志索引

- `BlockFlag` 枚举：core、storage、production、turret 等
- 按队伍和标志双重索引
- 用于 AI 快速查找目标

### 受损建筑索引

- 按队伍索引受损建筑
- 用于修复 AI 快速定位需要修复的建筑

## 常见误区

- **quadrantSize = 20**：不是 16 或 32
- **索引是实时更新的**：通过事件监听
- **BlockFlag 是方块的战术标志**：不是功能分类
- **damagedTiles 按队伍分开**：每个队伍独立
- **allPresentOres 包含所有矿石**：包括墙壁矿石
- **floorMap 是地板标志映射**：不是地板类型

## Related

- [Mindustry AI System](../mindustry/ai-system.md)
- [Mindustry Block System](mindustry-block-building-system.md)
