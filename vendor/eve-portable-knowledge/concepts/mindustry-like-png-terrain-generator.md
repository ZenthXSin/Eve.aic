# Mindustry-like PNG 地形生成器复刻记录

> Source: conversation 2026-07-06；源码 `Mindustry/core/src/mindustry/maps/generators/BasicGenerator.java`、`mindustry/maps/filters/*`、`mindustry/io/MapIO.java`；技能 `skills/mindustry-map-image-generator/scripts/mindustry_terrain_generator.py`

## 结论

已新增 `skills/mindustry-map-image-generator/scripts/mindustry_terrain_generator.py`，目标是复刻 Mindustry 原版地形生成器/滤镜的主要语义，并输出可被 Editor Import Image 读取的 PNG。

由于源码 `MapIO.readImage(Pixmap, Tiles)` 明确 `if(block.hasBuilding()) continue;`，PNG 方案的边界仍是环境 tile：floor、overlay、部分无建筑环境 block。建筑、队伍、旋转、config 不属于 PNG 生成器能力。

## 已复刻的源码语义

参考 `BasicGenerator.java`：

- `terrain()`：噪声 + 中心距离生成墙/块状结构
- `ores()` / `ore()`：基于多层噪声生成矿物 overlay
- `tech()`：按 section 边界生成科技地板/金属结构
- `distort()`：基于噪声偏移采样旧 tile
- `scatter()`：概率散布
- `cells()`：蜂窝自动机式墙体平滑
- `cliffs()`：按邻接 air 计算 cliff 掩码，PNG 中以可导入环境色近似
- `decoration()`：邻接为空时概率装饰
- `blend()`：邻接指定块时替换边缘地板

参考 `GenerateFilter` 系列：

- `TerrainFilter`
- `NoiseFilter`
- `OreFilter`
- `ScatterFilter`
- `RiverNoiseFilter`
- `DistortFilter`
- `MedianFilter`
- `BlendFilter`

## 脚本能力

命令：

```bash
python3 skills/mindustry-map-image-generator/scripts/mindustry_terrain_generator.py config.json output.png
python3 skills/mindustry-map-image-generator/scripts/mindustry_terrain_generator.py config.json output.png --sidecar output.state.json
```

支持 preset：

- `serpulo` / `island`
- `erekir` / `red`
- `volcano`
- `tech`

支持操作：

- `terrainFilter`
- `noiseFilter`
- `oreFilter`
- `scatterFilter`
- `riverNoiseFilter`
- `distortFilter`
- `medianFilter`
- `blendFilter`
- `ores`
- `cells`
- `tech`
- `cliffs`
- `decoration`
- `circle`
- `line`
- `rect`
- `fill`

## 验证

已执行：

```bash
python3 -m py_compile skills/mindustry-map-image-generator/scripts/mindustry_terrain_generator.py
python3 skills/mindustry-map-image-generator/scripts/mindustry_terrain_generator.py /tmp/fullgen_test.json /tmp/fullgen_test.png --sidecar /tmp/fullgen_test.sidecar.json
python3 skills/mindustry-map-image-generator/scripts/mindustry_terrain_generator.py /tmp/fullgen_volcano.json /tmp/fullgen_volcano.png --sidecar /tmp/fullgen_volcano.sidecar.json
```

结果：

- `/tmp/fullgen_test.png`：192×192，10 种颜色
- `/tmp/fullgen_volcano.png`：300×300，10 种颜色

## 重要限制

当前使用 Python 自写 value-noise 近似 Arc `Simplex/Ridged`，结构语义按源码复刻，但不是逐像素一致。若要字节级一致，应改为 JVM 桥接直接调用 Mindustry/Arc 的 `Simplex`、`Ridged` 与 `Blocks` 内容表。

## Related

- [全面复刻 Mindustry 地图编辑器方案](../analysis/mindustry-map-editor-full-clone-plan.md) — 地图编辑器和 `.msav` 方向
- [Mindustry 地图导入图片颜色映射](mindustry-map-image-import-color-mapping.md) — PNG 导入颜色映射链路
