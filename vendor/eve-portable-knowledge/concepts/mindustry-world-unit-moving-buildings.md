# Mindustry World Unit / Moving Buildings

> Source: 对话纠错 + 源码验证：`Mindustry/core/src/mindustry/entities/comp/PayloadComp.java`, `world/blocks/payloads/BuildPayload.java`, `entities/comp/BlockUnitComp.java`; 示例 Mod：`ProjectUnityPrivate_V8/src/unity/entities/UnitTile.java`, `ProjectUnityPrivate_V8/src/unity/gen/WorldUnit.java`, `ProjectUnityPrivate_V8/src/unity/util/BlockMovement.java`

## 结论

社区里“把一片地板/建筑变成单位但建筑还能工作”的常见实现不是直接把 `Tile` 变成 `Unit`，而是两类方案：

1. **Payload 方案**：单位持有多个 `BuildPayload`，建筑作为 payload 更新/绘制
2. **WorldUnit / 沙盒世界方案**：单位内部维护一个独立 `World + Tiles + UnitTile`，把真实建筑搬进这个小世界，单位更新时临时切换 `Vars.world`，让建筑在局部世界里正常 `update()`

前者适合单个或少量建筑，后者才接近“地板连同上方一片建筑整体变单位并保持工作”

## 原版依据

### `PayloadComp`

`PayloadComp` 内有：

- `Seq<Payload> payloads`
- `pickup(Building)`：`tile.pickedUp(); tile.tile.remove(); tile.afterPickedUp(); addPayload(new BuildPayload(tile));`
- `update()`：遍历 payload，`pay.set(x, y, rotation); pay.update(self(), null);`

这说明原版单位本来就支持携带建筑 payload

### `BuildPayload`

`BuildPayload.update()`：

```java
if(unitHolder != null && (!build.block.updateInUnits || (!state.rules.unitPayloadUpdate && !build.block.alwaysUpdateInUnits))) return;
build.tile = emptyTile;
build.updatePayload(unitHolder, buildingHolder);
```

关键限制：

- 建筑在单位里时 `tile = emptyTile`
- 默认只有 `state.rules.unitPayloadUpdate` 或方块 `alwaysUpdateInUnits = true` 时才完整更新
- `Building.updatePayload()` 默认只是 `update()`
- `BuildPayload.draw()` 调用 `build.payloadDraw()`

所以 payload 可以工作，但依赖真实地图邻接/真实地板/完整网络的建筑会很容易抽象

### `BlockUnitComp`

`BlockUnitComp` 是“建筑伪装成单位”的原版内部组件，但源码明确：

```java
throw new RuntimeException("Do not add BlockUnit entities to the game, they will simply crash. Internal use only.");
```

所以不能把它当成通用方案

## Project Unity 的 WorldUnit 思路

### `UnitTile`

`unity.entities.UnitTile extends Tile`，重写：

- `setFloor(Floor type)` 只改本地 floor
- `setBlockQuiet(Block block)`
- `changeBuild(...)` 创建 building 并绑定到这个 UnitTile
- `fireChanged()/recache()/recacheWall()` 空实现

这就是“单位内部 tile”的基础

### `WorldUnit.setup()`

示例代码里：

- `unitWorld = new World()`
- `unitWorld.tiles = new Tiles(w, h)`
- 每格塞入 `new UnitTile(...)`
- 初始化地板：`unitWorld.tiles.eachTile(tile -> tile.setFloor(Blocks.metalFloor.asFloor()))`
- 扫描单位周围真实建筑，保存电力链接，`building.tile.remove()` 从真实世界移除
- 临时 `Vars.world = unitWorld`
- 在 `unitWorld.tile(tx, ty).setBlock(..., () -> building)` 重新安放原 building
- 恢复电力链接并 `updateProximity()`

### `WorldUnit.update()` 核心

更新时它会：

- 保存真实世界 `World ow = Vars.world`
- `Vars.world = unitWorld`
- 把内部建筑坐标旋转/平移到单位世界坐标附近
- 对每个内部建筑调用 `b.update()`
- 再把建筑坐标还原为单位内部坐标
- `Vars.world = ow`

这套就是“移动建筑群单位”的核心黑魔法

## 实现要点

- 地板：不要真的移动主地图地板；用单位内部 `UnitTile.floor` 保存局部地板，并在单位绘制时按单位位置/旋转绘制出来
- 建筑：从主地图 `tile.remove()`，转移到单位内部 `unitWorld.tile(...).setBlock(..., () -> building)`
- 更新：临时切 `Vars.world`，把建筑坐标映射到单位外部坐标后 `update()`，再还原
- 电力：保存并转换 `PowerNodeBuild.power.links`，进小世界后重新 configure
- 炮塔：玩家控制时可把 `logicShooting/targetPos` 转发给内部炮塔
- 邻接：进小世界后必须 `updateProximity()`
- 绘制：内部地板与建筑需要按单位中心做坐标变换；炮塔旋转要补偿单位旋转
- 存档/同步：必须自己写 `write/read`，保存内部 tiles、floor、block、building state、power links，否则联机/读档必炸

## 相关

- [Mindustry Building 与 Tile 坐标关系](mindustry-building-tile-coordinate-relationship.md) — Building 强绑定 Tile
- [Mindustry Payload System](mindustry-payload-system.md) — BuildPayload / UnitPayload 机制
