# Mindustry Tile 中 floor / overlay / block 的渲染位置

> Source: 对话总结；源码：`mindustry.graphics.FloorRenderer`、`mindustry.graphics.BlockRenderer`、`mindustry.world.blocks.environment.Floor`、`mindustry.world.Block`、`mindustry.graphics.Layer`、`mindustry.core.Renderer`。

## 结论

一个 `Tile` 里有三类内容：

- `floor`：地板，类型是 `Floor`
- `overlay`：地板覆盖层，也属于 `Floor`，常见是矿物/浅层覆盖物
- `block`：墙、建筑、空气等，类型是 `Block`

渲染上不是简单三个独立 Layer：

- `floor` 主要由 `Renderer.draw()` 注册到 `Layer.floor` 的 `blocks.floor::drawFloor` 渲染。
- `overlay` 通常在 `Floor.drawBase(tile)` 内由 `drawOverlay(tile)` 递归绘制，跟随 floor 的 floor mesh/cache layer 一起进入环境网格缓存。
- `block` 若是普通建筑/方块，主要由 `BlockRenderer.drawBlocks()` 设置 `Draw.z(Layer.block)` 后调用 `block.drawBase(tile)` / `Building.draw()` 渲染。
- 墙类等 `cacheLayer != CacheLayer.normal` 的静态环境 block，会在 `FloorRenderer.cacheChunkLayer()` 中作为环境缓存层渲染；`Renderer.draw()` 中 `CacheLayer.walls` 被安排在 `Layer.block - 0.09f` 附近绘制。

## 关键源码链路

### floor / overlay

`mindustry.core.Renderer.draw()`：

```java
Draw.draw(Layer.floor, blocks.floor::drawFloor);
```

`mindustry.graphics.FloorRenderer.cacheChunkLayer()`：

```java
}else if(floor.cacheLayer == layer && (ignoreWalls || world.isAccessible(tile.x, tile.y) || tile.block().cacheLayer != CacheLayer.walls || !tile.block().fillsTile)){
    floor.drawBase(tile);
}else if(floor.cacheLayer != layer && layer != CacheLayer.walls){
    floor.drawNonLayer(tile, layer);
}
```

`mindustry.world.blocks.environment.Floor.drawBase(Tile tile)`：

```java
@Override
public void drawBase(Tile tile){
    drawMain(tile);

    if(drawEdgeIn){
        drawEdges(tile);
    }
    drawOverlay(tile);

    if(tile.overlay() != Blocks.air && tile.floor() == this && isLiquid){
        Draw.alpha(1f - overlayAlpha);
        drawMain(tile);
        Draw.color();
    }
}
```

`Floor.drawOverlay(Tile tile)`：

```java
public void drawOverlay(Tile tile){
    Floor floor = tile.overlay();
    if(floor != Blocks.air && floor != this){
        floor.drawBase(tile);
    }
}
```

### block

`mindustry.graphics.BlockRenderer.drawBlocks()`：

```java
Draw.z(Layer.block);

boolean visible = (build == null || !build.inFogTo(pteam));

if(block != Blocks.air && (visible || build.wasVisible)){
    block.drawBase(tile);
    Draw.reset();
    Draw.z(Layer.block);
```

`mindustry.graphics.Layer`：

```java
//floor tiles
floor = 0,

//base block layer - most blocks go here
block = 30,
```

## 注意

`overlay` 不是 `Layer.blockOver`。`Layer.blockOver` 是建筑上层附加绘制常用层；Tile 的 `overlay` 是 `Floor` 覆盖层，通常随 floor 环境缓存绘制。


## FloorRenderer 如何按 chunk 缓存成 mesh

`mindustry.graphics.FloorRenderer` 的源码注释给出整体设计：

```java
/**
 * general implementation:
 *
 * caching:
 * 1. create fixed-size float array for rendering into
 * 2. for each chunk, cache each layer into buffer; record layer boundary indices (alternatively, create mesh per layer for fast recache)
 * 3. create mesh for this chunk based on buffer size, copy buffer into mesh
 *
 * rendering:
 * 1. iterate through visible chunks
 * 2. activate the shader vertex attributes beforehand
 * 3. bind each mesh individually, draw it
 *
 * */
```

实际实现不是一个 chunk 一个总 mesh，而是：

```text
cache[cx][cy][cacheLayer.id] = ChunkMesh
```

关键流程：

1. `reload()` 按 `chunksize = 30` 将地图切成 chunk，创建 `cache` 与 `dirty` 数组；非动态模式下立即预缓存全部 chunk。
2. `cacheChunk(cx, cy, ignoreWalls)` 扫描该 chunk 外扩 1 格范围，收集实际用到的 `CacheLayer`，例如 `normal`、水体 shader 层、`walls`。
3. 对每个用到的 `CacheLayer` 调用 `cacheChunkLayer()`。
4. `cacheChunkLayer()` 临时把 `Core.batch` 替换成 `FloorRenderBatch`，让 `Floor.drawBase(tile)` / `Block.drawBase(tile)` 产生的 `Draw.rect()` 不直接进 GPU，而是写入 `vertices` 浮点数组。
5. `FloorRenderBatch.draw()` 把每个 sprite 转换成 4 个顶点，每个顶点 3 个 float：打包位置、颜色、打包 UV。
6. 构造 `ChunkMesh`，调用 `mesh.setVertices(vertices, 0, vidx)`，并复用全局 `indexData` 作为四边形三角索引。
7. 渲染时 `drawFloor()` 只找摄像机可见 chunk，按 `CacheLayer.id` 排序后 `drawLayer()`，最终对每个可见 `ChunkMesh` 调 `mesh.render(shader, Gl.triangles, ...)`。

`recacheTile(x, y)` 不会立刻重建 mesh，只是把对应 chunk 标记为 dirty：

```java
dirty[cx][cy] = true;
```

下一次 `drawFloor()` 或 `drawLayer(..., checkChanges)` 时才重建该 chunk。这样避免每个 tile 改动都立刻进行 GPU mesh 重建。

## Related

- [Mindustry Rendering System](../mindustry/rendering-system.md) — 总体渲染管线
- [Mindustry Content Loading Chain](mindustry-content-loading-chain.md) — Tile / Block / Floor / Building 的内容关系
