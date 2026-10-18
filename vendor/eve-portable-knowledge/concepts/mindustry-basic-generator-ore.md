# BasicGenerator ore() 方法用法

> Source: `mindustry/maps/generators/BasicGenerator.java`

## 核心机制

`BasicGenerator` 有三个**字段**（不是方法参数）：
```java
protected Block floor, block, ore;
```

`pass()` 遍历每个格子时自动设值，你修改后自动写回：

```java
pass((x, y) -> {
    // floor = 当前格地板, block = 当前格建筑, ore = 当前格矿层
    ore = Blocks.copperore;  // 改完自动 setOverlay()
});
```

## 三个重载方法

### ores(Seq<Block> ores)
批量放矿。从稀到密遍历 ores 列表，用噪声密度判断是否放置：
```java
ores(Seq.with(Blocks.copperore, Blocks.leadore, Blocks.titaniumore));
```
第一个 ore 放置密度最低（最稀），最后一个密度最高（最密）。

### ore(Block dest, Block src, float i, float thresh)
在指定地板类型上放矿。`thresh` 控制密度阈值（0-1，越小越密）：
```java
ore(Blocks.copperore, Blocks.sand, 1, 1);  // 在沙地上放铜矿
```

### oreAround(Block ore, Block wall, int radius, float scl, float thresh)
在墙周围放矿。检查半径内是否有墙，有则放置 overlay：
```java
oreAround(Blocks.copperore, Blocks.stoneWall, 3, 1, 0.5);
```

## wallOre
在墙表面放矿（overlay 或 block），条件：墙旁边有空气格：
```java
wallOre(Blocks.stoneWall, Blocks.copperore, 1, 0.5f);
```

## overlay
类似 ore()，但用 noise + rand.chance 组合判断，在指定地板上随机放 overlay：
```java
overlay(Blocks.sand, Blocks.scrap, 0.3f, 2, 0.8f, 30f, 0.7f);
```
