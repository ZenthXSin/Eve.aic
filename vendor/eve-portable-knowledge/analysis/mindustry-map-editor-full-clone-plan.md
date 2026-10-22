# 全面复刻 Mindustry 地图编辑器方案

## 来源

- 知识库：`knowledge/concepts/mindustry-map-editor-system.md`
- 源码：`Mindustry/core/src/mindustry/editor/MapEditor.java`
- 源码：`EditorTool.java`、`EditorTile.java`、`DrawOperation.java`、`OperationStack.java`、`EditorRenderer.java`
- 源码：`Mindustry/core/src/mindustry/io/MapIO.java`
- 示例 Mod：`NewHorizonMod/src/newhorizon/NHUI.java` 中通过反射向 `MapEditorDialog` 菜单追加入口
- 现有技能：`skills/mindustry-map-image-generator/SKILL.md`

## 结论

现有地图生成技能只复刻了 `MapIO.readImage()` 的 PNG 导入链路，能力边界是“像素颜色 → floor/overlay/部分非建筑环境块”。要全面复刻 Mindustry 地图编辑器，不能继续以 PNG 为核心，应改成三层架构：

1. 数据模型层：复刻 `Tiles` / `EditorTile` / `TileState`
2. 编辑操作层：复刻 `MapEditor` / `EditorTool` / `DrawOperation` / `OperationStack`
3. 导入导出层：对接 `.msav` 地图写入或生成可由 Mindustry 加载的地图包

## 原版核心结构

```text
EditorTool
  -> MapEditor.drawBlocks / drawCircle / resize / addCliffs
  -> EditorTile.setFloor / setBlock / setOverlay / setTeam
  -> DrawOperation 记录旧值
  -> OperationStack 撤销/重做，maxSize=10
  -> EditorRenderer 增量 recache
```

`MapEditor` 默认状态：

- `brushSizes = {1, 1.5f, 2, 3, 4, 5, 9, 15, 20}`
- `drawBlock = Blocks.stone`
- `drawTeam = Team.sharded`
- `showTerrain/showFloor/showBuildings = true`

## 必须复刻的编辑能力

### 1. Tile 数据

每格至少保存：

- `x/y`
- `floor`
- `overlay`
- `block`
- `team`
- `rotation`
- `data/floorData/overlayData/extraData`
- `config`
- 多格建筑中心与占位关系

否则无法支持建筑、队伍、旋转、撤销、resize 配置迁移。

### 2. 绘制逻辑

需要复刻 `MapEditor.drawBlocks()` 的分支：

- floor：`setFloor()`，不支持 overlay 时清 overlay
- overlay：`setOverlay()`，检查 `needsSurface/supportsOverlay/OreBlock`
- building：`setBlock(drawBlock, drawTeam, rotation)`
- multiblock：clamp 到边界，调用 `hasOverlap()` 禁止重叠
- saveData/saveConfig：记录 data，并在放置后调用配置逻辑

### 3. 工具

原版 `EditorTool`：

- `pick(I)`：建筑优先，其次 overlay，再 floor
- `line(L)`：Bresenham 直线，可 orthogonal
- `pencil(B)`：normal/replace/square/drawteams/underliquid
- `eraser(E)`：清建筑或清矿
- `fill(G)`：连通填充、全图替换、队伍填充、擦除、悬崖填充、液体下填充
- `spray(R)`：随机喷涂，`chance = 0.012`

### 4. 撤销/重做

原版 `OperationStack.maxSize = 10`，`DrawOperation` 记录旧值，undo 逆序，redo 正序。复刻时建议可配置历史长度，但默认应保持 10，避免和原版语义偏差。

操作类型：

- `opFloor`
- `opBlock`
- `opRotation`
- `opTeam`
- `opOverlay`
- `opData`
- `opDataExtra`

### 5. resize

`MapEditor.resize(width,height,shiftX,shiftY)` 不只是裁剪/扩展数组，还会：

- `world.clearBuildings()`
- 重新分配 `Tiles`
- 迁移 tile 坐标
- 更新 Building 世界坐标
- 用 `BuildPlan.pointConfig()` 修正相对坐标配置
- 遵守 `ignoreResizeConfig`

### 6. 悬崖

`addCliffs()` 基于 8 邻接静态方块检测生成 `Blocks.cliff`，并把邻接掩码写入 `tile.data`。

## 与现有技能差距

| 能力 | 现有 PNG 地图技能 | 原版编辑器 |
|---|---|---|
| floor | 支持 | 支持 |
| overlay/ore | 部分支持 | 支持并校验地表 |
| building | 基本不支持，`readImage()` 明确忽略建筑 | 完整支持 |
| team | 不支持 | 支持 synthetic 建筑 |
| rotation | 不支持 | 支持 |
| config | 不支持 | 支持 saveConfig/saveData |
| multiblock | 不可靠 | 有边界 clamp 和重叠检测 |
| undo/redo | 不支持 | 10 步 |
| fill/line/spray | 不支持 | 支持 |
| resize | 不支持 | 支持配置坐标迁移 |
| export `.msav` | 不支持 | 原版保存链路支持 |

## 推荐复刻路线

### Phase 1：离线编辑核心

目标：先做 headless 版，不碰 UI。

- 建立 `TileState` 数据结构
- 建立 `MapState(width,height,tiles,tags)`
- 建立 block registry，从 Mindustry content 导出字段或读取 API 索引
- 实现 `setFloor/setOverlay/setBlock/setTeam`
- 实现 circular/square brush
- 实现 undo/redo

验收：能用 JSON 指令生成一张包含 floor/ore/wall/building/team/rotation 的地图状态。

### Phase 2：工具复刻

- pencil/eraser/pick
- line + Bresenham
- flood fill + replaceall
- spray
- addCliffs
- resize + config shift

验收：相同操作序列下，输出 tile 状态与原版一致。

### Phase 3：导入导出

两条路：

1. 简化路：输出 PNG + JSON sidecar，只适合地形预览
2. 正统路：复用 Mindustry `SaveIO.write()` / `MapIO.writeMap()` 生成 `.msav`

要全面复刻必须走第 2 条，否则建筑、队伍、配置全丢。

### Phase 4：UI

可选实现 Web UI 或 Mindustry 内 Mod UI：

- Web UI：Canvas/WebGL 渲染 tile，适合外部工具
- Mod UI：参考 NewHorizon 用反射扩展 `MapEditorDialog` 菜单，但深度替换编辑器 UI 风险更高

## 关键风险

1. `.msav` 写入复杂，必须对齐 `SaveVersion` 区域格式
2. `Block` 字段多，脱离 JVM 原版 content 容易字段不全
3. 多格建筑和 config 是最大坑
4. `EditorRenderer` 依赖 Arc/Mindustry 渲染系统，外部复刻不应照搬，应重写渲染层
5. PNG 导入不是完整地图格式，继续围绕 PNG 做会走偏

## 最小可行方案

先把现有 `mindustry-map-image-generator` 升级为 `mindustry-map-editor-core`：

- 保留 PNG 生成功能作为 preview/import-image 兼容层
- 新增 JSON 操作脚本：`new/set/fill/line/erase/resize/undo/redo/export`
- 最终通过 JVM 小工具调用 Mindustry 原版 `SaveIO` 写 `.msav`

这比从零手写 `.msav` 稳，也比只做 PNG 强得多。
