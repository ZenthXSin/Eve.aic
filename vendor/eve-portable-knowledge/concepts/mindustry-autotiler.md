# Mindustry Autotiler 用法

> 来源：本地 mods 示例与 Mindustry 源码相关实现思路

## 结论
`Autotiler` 本质上是让方块根据**邻接关系**自动选择边角、边缘、连接线贴图的机制。常见用途是：
- 运输带/管线/墙体/地板等需要“连线外观”的方块
- 同一方块根据四周是否有同类方块，切换不同贴图

## 在哪里生效
通常不是你手动去“调用 Autotiler”，而是：
- 方块类开启 `autotile` / 相关自动连接能力
- 提供 `-tiled` 资源或自动贴图分片
- 在 `load()` 时把贴图区域装进 `autotileRegions`
- 运行时由引擎在绘制时根据邻接状态选择对应贴图

## 示例线索
本地示例中，`TiledFloor` 会在 `load()` 里检查：
- `autotile`
- `name + "-tiled"`
- `autotileRegions`
- `autotileVariantRegions`

这说明自动贴图的关键不是算法本身，而是**资源命名 + 区域切分 + 邻接判定**。

## 常见做法
### 1. 准备贴图资源
准备一张或多张用于自动连接的贴图，通常命名为：
- `xxx-tiled`
- `xxx-tiled-1`
- `xxx-tiled-2`

### 2. 在方块加载阶段切图
将整张图按固定尺寸切成 `TextureRegion[][]`，例如 32x32 分块。

### 3. 开启自动连接
如果方块支持 autotile，框架会根据四周相邻方块选择合适的 tile 变体。

### 4. 运行时绘制
绘制时按 tile 坐标、邻接情况、随机种子选中对应切片。

## 适合它的场景
- 地板贴图自动拼接
- 连接型装饰块
- 路面、管道、线路、墙面边缘

## 注意点
- 自动贴图依赖**资源尺寸与切分规则一致**
- 需要按框架预期的命名提供资源
- 如果资源尺寸不对，会加载失败或退化为普通绘制
- 复杂表现常常还会配合 `variants`、`autotileVariants`

## 相关页面
- [Mindustry Java Mod 结构](../mindustry/java-mod-structure.md)
- [Mindustry 方块与建筑系统](../mindustry/block-building-system.md)
- [Mindustry 知识缺口总览](../analysis/mindustry-knowledge-gap-overview.md)

## 备注
如果需要更具体到“某个方块类如何写 Autotiler/Autotile”的级别，建议继续结合对应源码类和贴图命名规则细化。
