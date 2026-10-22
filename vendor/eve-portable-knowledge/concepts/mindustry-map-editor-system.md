# Mindustry 地图编辑器系统源码验证

## MapEditor（编辑器核心）

- 路径：`mindustry.editor.MapEditor`
- 持有 `EditorRenderer`、`OperationStack`（撤销/重做）

### 关键字段

| 字段 | 默认值 | 含义 |
|------|--------|------|
| `brushSize` | 1f | 画笔大小 |
| `drawBlock` | Blocks.stone | 当前绘制方块 |
| `drawTeam` | Team.sharded | 当前绘制队伍 |
| `rotation` | 0 | 当前旋转 |
| `showTerrain` | true | 显示地形层 |
| `showFloor` | true | 显示地板层 |
| `showBuildings` | true | 显示建筑层 |

### 画笔大小预设

```java
{1, 1.5f, 2, 3, 4, 5, 9, 15, 20}
```

### 核心方法

- `beginEdit(int width, int height)` — 从空白创建
- `beginEdit(Map map)` — 从地图加载
- `beginEdit(Pixmap pixmap)` — 从图片导入
- `drawBlocks(x, y)` — 绘制方块（圆形画笔）
- `drawBlocksReplace(x, y)` — 替换模式绘制
- `resize(width, height, shiftX, shiftY)` — 调整地图大小
- `undo()` / `redo()` — 撤销/重做
- `addCliffs()` — 自动生成悬崖

### 编辑模式

- 默认：正常绘制
- 替换：仅替换已有方块
- 方形：方形画笔
- 画队伍：只改变队伍
- 下液体：在液体下方绘制

## EditorTile（编辑器专用 Tile）

- 继承 `Tile`
- 所有操作记录到 `DrawOperation`（用于撤销）
- `isEditorTile()` 返回 true

## EditorTool（编辑工具枚举）

| 工具 | 快捷键 | 模式 |
|------|--------|------|
| zoom | V | 无 |
| pick | I | 无（拾取方块） |
| line | L | replace, orthogonal |
| pencil | B | replace, square, drawteams, underliquid |
| eraser | E | eraseores |
| fill | G | replaceall, fillteams, fillerase, fillcliffs, fillunderliquid |

## OperationStack（撤销/重做栈）

- `maxSize = 10` — 最多保存 10 步操作
- 超出时移除最早的
- `undo()` — 回退一步
- `redo()` — 前进一步

## DrawOperation（操作记录）

- 记录每个 Tile 的变化（位置 + 旧值）
- 支持 opFloor / opBlock / opTeam / opRotation / opData 等操作类型

## Context（WorldContext 实现）

- 内部类，实现 `WorldContext`
- `create(x, y, floorID, overlayID, wallID)` — 创建 EditorTile
- `begin()` / `end()` — 调用 `world.beginMapLoad()` / `world.endMapLoad()`

## 硬性规则

1. 地图编辑器用 `EditorTile`，不是普通 `Tile`
2. 撤销栈最多 10 步，不是无限
3. 画笔大小预设 9 档，最大 20
4. 默认绘制方块是 `Blocks.stone`
5. 默认队伍是 `Team.sharded`
6. `beginEdit(Map)` 会把 Steam ID 写入 tags
7. 悬崖自动生成基于 8 邻接静态方块检测
8. 多方块绘制有重叠检测 `hasOverlap()`
9. `resize()` 会偏移建筑配置坐标
10. `loading` 标志期间不记录操作
11. EditorTile 继承 Tile，override 了 setFloor/setBlock 来记录操作

## 关联

- [Vars UI](../mindustry/vars-ui.md)
- [Block and Building System](../mindustry/block-building-system.md)
- [Save and Load System](../mindustry/save-load-system.md)

## 补充：工具行为源码细节（2026-07-06）


### 总体结构类比

地图编辑器像一张带历史记录的画布：

```text
EditorTool(笔/桶/橡皮)
        ↓ 调用
MapEditor.drawBlocks / drawCircle / resize
        ↓ 修改
EditorTile.setFloor/setBlock/setOverlay/setTeam
        ↓ 记录
DrawOperation(TileOp long编码)
        ↓ 入栈
OperationStack(max 10)
        ↓ 通知
EditorRenderer.updateStatic/updateBlock
```

### MapEditor 关键链路

- `beginEdit(int width, int height)`：`reset()` 后 `createTiles()`，创建全 stone 地板的 `EditorTile`，再 `renderer.resize()`。
- `beginEdit(Map map)`：复制 `map.tags`，Steam 工坊地图会补 `steamid`，然后 `MapIO.loadMap(map, context)`。
- `beginEdit(Pixmap pixmap)`：先建同尺寸 tile，再 `MapIO.readImage(pixmap, tiles())` 按图片导入。
- `updateRenderer()`：把现有 `Tile` 转成 `EditorTile`，保留中心建筑，再重设多格建筑中心。
- `load(Runnable)`：临时打开 `loading`，避免加载/撤销过程被记录成新操作。

### 绘制逻辑

- 多格建筑：`drawBlocks()` 会 clamp 到地图边界，并用 `hasOverlap()` 禁止与现有多格建筑重叠；同尺寸中心替换例外。
- 非多格建筑：按 `drawBlock.isFloor()` 分地板/覆盖层/建筑处理。
- `forceOverlay` 用于液体下覆盖层模式，调用 `tile.setOverlay(drawBlock.asFloor())`。
- `saveData` 或 tile 当前有数据时，先记录 `opData` 和 `opDataExtra`；若实际没变化，会 `removeLastOps(2)` 撤销冗余记录。
- `saveConfig` 建筑绘制后调用 `drawBlock.placeEnded(tile, null, editor.rotation, drawBlock.lastConfig)`。

### EditorTile 记录机制

`EditorTile` override 了 `setFloor`、`setBlock`、`setTeam`、`setOverlay`。只要不处于 `state.isGame()`、`editor.isLoading()`、`world.isGenerating()`，就先调用 `editor.addTileOp(...)` 记录旧值，再执行真实修改。

- `setFloor(Floor)`：覆盖层类型会转走 `setOverlay()`；普通地板变更记录 `opFloor`。
- `setBlock(Block, Team, rotation, Prov)`：记录 `opBlock`，建筑存在时额外记录 `opRotation`、`opTeam`；多格非中心会先回到中心记录。
- `setTeam(Team)`：只对 `synthetic()` 建筑有效。
- `setOverlay(Block)`：检查 `needsSurface` / `supportsOverlay` / `OreBlock` 后记录 `opOverlay`。
- 编辑器状态下 `changeBuild()` 只构建本地 Building 和模块，不触发正常游戏事件链。

### 撤销/重做

- `OperationStack.maxSize = 10`，超过删除最早操作。
- `DrawOperation.undo()` 逆序遍历操作，`redo()` 正序遍历。
- `updateTile()` 会把当前 tile 值反写进数组，再把旧值设置回 tile，所以同一份操作记录可以来回切换。
- `DrawOperation` 类型：`opFloor/opBlock/opRotation/opTeam/opOverlay/opData/opDataExtra`。

### EditorTool 工具行为

- `pick(I)`：优先拾取建筑，其次覆盖层，再地板；会调用 `editorPicked(tile)`。
- `line(L)`：用 `Bresenham2.line()`；`orthogonal` 模式强制横/竖。
- `pencil(B)`：normal、replace、square、drawteams、underliquid 五种模式。
- `eraser(E)`：默认清建筑，`eraseores` 清 overlay。
- `fill(G)`：支持连通区域 flood fill、全图 replaceall、队伍填充、擦除、悬崖填充、液体下填充。
- `spray(R)`：随机喷涂，概率常量 `chance = 0.012`。

### 易错点

- 不是所有修改都会进撤销栈：`loading`、生成中、游戏状态会跳过记录。
- 撤销栈只有 10 步，别当无限历史。
- fill 的 replaceall 是全图扫描，不是连通填充。
- 多格建筑填充会退化成 pencil，源码注释写了 “don't fill multiblocks”。
- `setTeam()` 对非 synthetic 方块无意义。
- 地图 resize 会修正建筑配置里的相对坐标，除非 `ignoreResizeConfig`。
