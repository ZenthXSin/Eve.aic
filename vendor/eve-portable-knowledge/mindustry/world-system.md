# World System

> 源码位置：`mindustry/core/World.java`(664行)、`mindustry/world/Tiles.java`、`mindustry/world/TileGen.java`
> 验证版本：v136.2

## World 类

`World` 是全局世界管理器，通过 `Vars.world` 访问。

### 关键字段
```java
Tiles tiles;           // 瓦片数组（width*height）
int tileChanges = 1;   // 瓦片变化计数（用于懒更新）
int floorChanges = 1;  // 地面变化计数
boolean generating;    // 是否正在生成地图
```

### 坐标转换
```java
world.tile(x, y)         // 瓦片坐标→Tile（可为null）
world.tile(pos)           // 打包坐标→Tile
world.tileWorld(wx, wy)   // 世界坐标→Tile（Math.round/8）
world.build(x, y)         // 瓦片坐标→Building（可为null）
world.buildWorld(wx, wy)  // 世界坐标→Building
world.rawTile(x, y)       // 原始Tile（不检查边界）
World.conv(coord)         // 世界坐标→瓦片坐标（/8）
World.unconv(coord)       // 瓦片坐标→世界坐标（*8）
World.toTile(coord)       // 世界坐标→瓦片坐标（Math.round/8）
```

### 地图加载
```
world.loadMap(map, preview) → 读取地图数据 → tiles填充 → 生成事件
world.beginMapLoad() → generating=true
world.endMapLoad() → generating=false → WorldLoadEvent
```

### Tile 变化追踪
- `tileChanges` 每次 `TileChangeEvent` 递增
- `floorChanges` 每次 `TileFloorChangeEvent` 递增
- 用于方块检测世界状态变化时的懒更新

## Tiles 类

`Tiles` 是瓦片数组的封装。

```java
int width, height;
Tile[] array;  // width*height的一维数组

tiles.get(x, y)    // 返回Tile或null（越界）
tiles.getn(x, y)   // 返回Tile（不检查边界，可能ArrayIndexOutOfBounds）
tiles.getp(pos)    // 打包坐标访问
```

## 硬性规则

1. `world.tile()` 返回可为null，必须检查
2. `world.rawTile()` 不检查边界，可能抛异常
3. `tilesize=8` 是全局常量
4. `conv/unconv` 是静态方法，不依赖World实例
5. `tileChanges` 用于懒更新，不是帧计数器
6. 地图加载期间 `generating=true`，此时不触发事件
7. `WorldLoadEvent` 在 `endMapLoad()` 时触发
8. Tile坐标是short类型，最大32767
