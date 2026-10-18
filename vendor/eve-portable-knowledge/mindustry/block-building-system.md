# Block and Building System

> 源码位置：`mindustry/world/Block.java`(1662行)、`mindustry/world/Tile.java`(868行)、`mindustry/entities/comp/BuildingComp.java`(2274行)、`mindustry/world/Build.java`(318行)
> 验证版本：v136.2

## 核心架构

Mindustry 采用 **Block + Building 分离** 架构：
- **Block**（方块定义）：继承 `UnlockableContent`，是静态数据定义，描述方块属性、渲染、消耗、行为
- **Building**（方块实体）：由 `BuildingComp` 通过注解处理器生成，是运行时实体，存储动态状态
- **Tile**（瓦片）：世界网格中的一个位置，持有 `block` 和 `build`

## Block 关键字段

### 模块标志
```java
hasItems = false; hasLiquids = false; hasPower = false;
consumesPower = true;  // ⚠️ 默认true！即使hasPower=false
outputsPower = false; connectedPower = true; conductivePower = false;
```

### 物品/液体
```java
itemCapacity = 10;
liquidCapacity = -1f;  // -1=自动计算
liquidPressure = 1f;
```

### 物理
```java
solid = false; size = 1; health = -1; scaledHealth = -1; armor = 0f;
// health=-1 → scaledHealth*size*size, scaledHealth=-1 → 40
offset = ((size+1)%2) * tilesize/2f; // 奇数=0, 偶数=4
```

### 状态
```java
update = false; destructible = false; rotate = false;
breakable = true; configurable = false; unloadable = true;
```

### 环境
```java
envRequired = 0; envEnabled = Env.terrestrial; envDisabled = 0;
floating = false; placeableLiquid = false;
```

## Building (BuildingComp) 关键

### 模块（可为null）
```java
@Nullable PowerModule power;
@Nullable ItemModule items;
@Nullable LiquidModule liquids;
```

### 效率
```java
float efficiency;         // 所有消费者最小值
float optionalEfficiency; // 可选消费者效率
float potentialEfficiency; // shouldConsume()=true时的效率
```

## 生命周期

### 创建
```
Block.newBuilding() → buildType.get()
Building.init(tile, team, shouldAdd, rotation)
  → create(block, team) → set(x,y) → add() → created()
```

### 更新
```
Building.update() → enabled检查 → timeScale更新 → updateTile() → 效率/消耗
```

### 渲染
```
Building.draw() → drawTeamTop() → drawLight() → drawCracks() → drawStatus()
```

### 销毁
```
Building.kill() → Call.buildDestroyed() → onDestroyed() → remove()
```

## Tile 关键

```java
short x, y;
Block block; Floor floor; Floor overlay;
@Nullable Building build;
byte data, floorData, overlayData; int extraData;
// tile.team() = build==null ? Team.derelict : build.team
```

## 硬性规则

1. `consumesPower` 默认 **true**
2. `liquidCapacity=-1f` 自动计算，不是没有
3. `health=-1` → `scaledHealth*size*size`，`scaledHealth=-1` → 40
4. `category` 默认 `distribution`
5. `isStatic()` = `cacheLayer==CacheLayer.walls`
6. `synthetic()` = `update || destructible`
7. Building是生成类，不是独立Java文件
8. 模块可为null，使用前必须检查
9. `efficiency` 是最小值不是平均值
10. `updateTile()` 是子类重写点，`update()` 是框架方法
11. `isValid()` = `tile.build==self() && !dead()`
12. 多方块非中心tile.build指向中心Building
13. tile.build可为null（Air/环境方块）
