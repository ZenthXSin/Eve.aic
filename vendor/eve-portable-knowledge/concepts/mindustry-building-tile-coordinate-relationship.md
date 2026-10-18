# Mindustry Building 与 Tile 坐标关系

## 结论

在 Mindustry 中，`Building` 的格子坐标与 `Tile` 的坐标是**同一套网格坐标**。`Building` 不单独维护一套独立的 tile 坐标，它是绑定在某个 `Tile` 上的建筑实体。

## 关键关系

- `Building.tileX()` 等于 `tile.x`
- `Building.tileY()` 等于 `tile.y`
- `Building.init(Tile tile, ...)` 会把 `this.tile = tile`
- 初始化时会调用 `set(tile.drawx(), tile.drawy())`，把建筑的世界坐标设置到该 Tile 的绘制中心

## 源码证据

### `BuildingComp.init`

```java
public Building init(Tile tile, Team team, boolean shouldAdd, int rotation){
    ...
    this.tile = tile;
    set(tile.drawx(), tile.drawy());
    ...
}
```

这说明：

1. 建筑先绑定到某个 Tile
2. 建筑的世界位置由 Tile 的绘制坐标决定
3. 建筑的格子位置来自 Tile 本身

### `BuildingComp.tileX / tileY`

```java
@Override
public int tileX(){
    return tile.x;
}

@Override
public int tileY(){
    return tile.y;
}
```

这说明 `Building` 只是直接转发 `Tile` 的坐标。

### `World.tileBuilding`

```java
public Tile tileBuilding(int x, int y){
    Tile tile = tiles.get(x, y);
    if(tile == null) return null;
    if(tile.build != null){
        return tile.build.tile;
    }
    return tile;
}
```

这进一步说明建筑与格子是强绑定关系：有建筑时，返回的仍然是该建筑绑定的 `Tile`。

## 理解方式

可以把它理解成三层坐标：

- **Tile 坐标**：地图网格坐标
- **Building 坐标**：建筑所占据的 Tile 坐标
- **World 坐标**：用于绘制和实体摆放的实际坐标，来自 `tile.drawx()` / `tile.drawy()`

## 实际开发中的用法

如果你要判断建筑所在格子：

```java
int x = building.tileX();
int y = building.tileY();
```

如果你要拿到对应 Tile：

```java
Tile tile = building.tile;
```

如果你要拿建筑的世界位置：

```java
float wx = building.x;
float wy = building.y;
```

## 一句话总结

`Building` 的 tile 坐标就是它绑定的 `Tile` 的坐标；`Building` 只是建筑实体，`Tile` 才是网格位置本体。