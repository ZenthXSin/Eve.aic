# Mindustry Environment Block Fields

> Source: `源码和示例/Mindustry/core/src/mindustry/world/blocks/environment/Floor.java`, `OverlayFloor.java`, `OreBlock.java`, `TiledFloor.java`, `TiledWall.java`, `源码和示例/Mindustry/core/src/mindustry/world/Block.java`

## Scope

这里的“environment”指 `mindustry.world.blocks.environment.*` 下的环境类方块。它们大多继承 `mindustry.world.Block`，其中 `Floor` 是核心基类，其余类型在此基础上叠加自己的字段和绘制/放置规则。

## Floor 公开字段

来自 `core/src/mindustry/world/blocks/environment/Floor.java`：

- `edge: String = "stone"`
- `speedMultiplier: float = 1f`
- `dragMultiplier: float = 1f`
- `damageTaken: float = 0f`
- `drownTime: float = 0f`
- `walkEffect: Effect = Fx.none`
- `walkSound: Sound = Sounds.none`
- `walkSoundVolume: float = 0.1f`
- `walkSoundPitchMin: float = 0.8f`
- `walkSoundPitchMax: float = 1.2f`
- `drownUpdateEffect: Effect = Fx.bubble`
- `status: StatusEffect = StatusEffects.none`
- `statusDuration: float = 60f`
- `liquidDrop: Liquid = null`
- `liquidMultiplier: float = 1f`
- `isLiquid: boolean`
- `overlayAlpha: float = 0.65f`
- `supportsOverlay: boolean = false`
- `shallow: boolean = false`
- `blendGroup: Block = this`
- `oreDefault: boolean = false`
- `oreScale: float = 24f`
- `oreThreshold: float = 0.828f`
- `wall: Block = Blocks.air`
- `decoration: Block = Blocks.air`
- `canShadow: boolean = true`
- `forceDrawLight: boolean = false`
- `needsSurface: boolean = true`
- `allowCorePlacement: boolean = false`
- `wallOre: boolean = false`
- `blendId: int = -1`
- `tilingVariants: int = 0`
- `autotile: boolean = false`
- `autotileMidVariants: int = 1`
- `autotileVariants: int = 1`
- `drawEdgeIn: boolean = true`
- `drawEdgeOut: boolean = true`

### Floor 内部缓存/运行时字段

- `tilingRegions: TextureRegion[][][]`
- `autotileRegions: TextureRegion[]`
- `autotileMidRegions: TextureRegion[]`
- `autotileVariantRegions: TextureRegion[][]`
- `tilingSize: int`
- `edges: TextureRegion[][]`
- `blenders: Seq<Floor>`
- `blended: Bits`
- `dirs: int[]`
- `edgeRegion: TextureRegion`

## OverlayFloor

`OverlayFloor` 自身几乎不新增字段，主要变化是：

- 继承 `Floor` 的全部字段
- 构造时强制 `useColor = false`
- `canPlaceOn(...)` 受 `wallOre` 影响
- `drawBase(...)` 使用 `variantRegions` 绘制

## OreBlock

`OreBlock` 继承 `OverlayFloor`，没有新增大量公开字段，核心配置是：

- `localizedName`
- `itemDrop`
- `variants = 3`
- `mapColor`
- `useColor = true`

并且 `init()` 会强制检查 `itemDrop != null`。

## TiledFloor

来自 `core/src/mindustry/world/blocks/environment/TiledFloor.java`：

- `sizedRegions: TextureRegion[][][][]`
- `maxSize: int = 3`

## TiledWall

来自 `core/src/mindustry/world/blocks/environment/TiledWall.java`：

- `sizedRegions: TextureRegion[][][][]`
- `maxSize: int = 3`

## 常一起用的 Block 字段

来自 `core/src/mindustry/world/Block.java`，环境类最常配合这些字段：

- `variants`
- `useColor`
- `itemDrop`
- `solid`
- `floating`
- `placeableLiquid`
- `placeableOn`
- `ignoreBuildDarkness`
- `hasShadow`
- `customShadow`
- `lightColor`
- `emitLight`
- `obstructsLight`
- `lightRadius`
- `buildVisibility`
- `replaceable`
- `alwaysReplace`
- `envRequired`
- `envEnabled`
- `envDisabled`
- `fillsTile`
- `forceDark`

## Cliff（悬崖）

继承 `Block`（不是 Floor），关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `size` | 11f | 悬崖尺寸（像素） |
| `cliffs` | cliffmask#256 | 256 张遮罩贴图 |
| `solid` | true | 固体 |
| `cacheLayer` | CacheLayer.walls | 墙壁缓存层 |
| `fillsTile` | false | 不填满瓦片 |
| `hasShadow` | false | 无阴影 |

**drawBase**：使用 `tile.data & 0xff` 索引 256 张 cliffmask，颜色 = floor.mapColor * 1.6

## Prop（装饰物）

继承 `Block`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `layer` | Layer.blockProp | 绘制层 |
| `breakable` | true | 可破坏 |
| `alwaysReplace` | true | 总是可替换 |
| `instantDeconstruct` | true | 瞬间拆除 |
| `unitMoveBreakable` | true | 单位移动可破坏 |
| `breakEffect` | Fx.breakProp | 破坏特效 |
| `breakSound` | Sounds.rockBreak | 破坏音效 |

**drawBase**：使用 `Mathf.randomSeed(tile.pos())` 选择变体

## TreeBlock（树木）

继承 `Block`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `shadowOffset` | -4f | 阴影偏移 |
| `shadow` | @-shadow | 阴影贴图 |
| `solid` | true | 固体 |
| `clipSize` | 90 | 裁剪尺寸 |
| `customShadow` | true | 自定义阴影 |

**drawBase**：带风摆动画（`Mathf.sin` 多频叠加），阴影在 Layer.power-1，本体在 Layer.power+1

## TallBlock（高物体）

继承 `Block`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `shadowOffset` | -3f | 阴影偏移 |
| `layer` | Layer.power+1 | 绘制层 |
| `shadowLayer` | Layer.power-1 | 阴影层 |
| `rotationRand` | 20f | 随机旋转范围 |
| `shadowAlpha` | 0.6f | 阴影透明度 |
| `solid` | true | 固体 |
| `customShadow` | true | 自定义阴影 |

**注释原文**："I don't know what else to call this. It's not a prop, it's not a tree."

## StaticWall（静态墙壁）

继承 `Prop`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `large` | @-large | 大贴图 |
| `autotile` | false | 自动贴图 |
| `autotileMidVariants` | 1 | 中间变体数 |
| `breakable` | false | 不可破坏 |
| `alwaysReplace` | false | 不可替换 |
| `variants` | 2 | 2个变体 |
| `cacheLayer` | CacheLayer.walls | 墙壁缓存 |
| `allowRectanglePlacement` | true | 允许矩形放置 |
| `instantBuild` | true | 瞬间建造 |
| `ignoreBuildDarkness` | true | 忽略建造黑暗 |
| `placeableLiquid` | true | 液体上可放置 |

**drawBase**：autotile 用 8 邻接 bit → 256 种贴图；非 autotile 用 2x2 大贴图拼接

## StaticTree（静态树）

继承 `StaticWall`，`variants = 0`

**drawBase**：根据邻接的 StaticWall 裁剪边缘（crop 避免重叠）

## SteamVent（蒸汽口）

继承 `Floor`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `parent` | Blocks.air | 底层地面 |
| `effect` | Fx.ventSteam | 蒸汽特效 |
| `effectColor` | Pal.vent | 特效颜色 |
| `effectSpacing` | 15f | 特效间隔 |
| `variants` | 2 | 2个变体 |
| `flags` | EnumSet.of(BlockFlag.steamVent) | 蒸汽口标志 |

**drawMain**：先画 parent 地面，再画 vent 贴图（2x2 跨瓦片）

## SeaBush（海灌木）

继承 `Prop`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `botRegion` | @-bot | 底部区域 |
| `lobesMin/Max` | 7 | 叶片数 |
| `obstructsLight` | false | 不遮光 |

**drawBase**：多叶片随机角度绘制，带风摆

## Seaweed（海草）

继承 `Prop`，`obstructsLight = false`

**drawBase**：类似 TreeBlock 的风摆动画，`Draw.rectv` 带顶点偏移

## ShallowLiquid（浅液面）

继承 `Floor`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `liquidBase` | null | 液体地面 |
| `floorBase` | null | 固体地面 |
| `liquidOpacity` | 0.35f | 液体透明度 |

**set(liquid, floor)** 方法设置：
- `isLiquid = true`
- `status = liquidBase.status`
- `liquidDrop = liquidBase.liquidDrop`
- `cacheLayer = liquidBase.cacheLayer`
- `shallow = true`

**createIcons**：在 MultiPacker 中混合 liquidBase 和 floorBase 贴图

## SpawnBlock（出生点）

继承 `OverlayFloor`，`variants = 0`，`needsSurface = false`

**drawBase**：只在 EditorTile 中绘制（游戏内不可见）

## AirBlock（空气方块）

继承 `Floor`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `alwaysReplace` | true | 总是可替换 |
| `hasShadow` | false | 无阴影 |
| `useColor` | false | 不使用颜色 |
| `wall` | this | 墙壁指向自己 |
| `needsSurface` | false | 不需要地面 |
| `canShadow` | false | 无阴影 |
| `generateIcons` | false | 不生成图标 |

**drawBase**：空实现（什么都不画）
**isHidden**：返回 true

## EmptyFloor（空地面）

继承 `Floor`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `variants` | 0 | 无变体 |
| `canShadow` | false | 无阴影 |
| `placeableOn` | false | 不可放置 |
| `solid` | true | 固体 |
| `drawEdgeOut` | false | 不画外边缘 |

**注释**："Empty floor is *not* equivalent to air. Unlike air, it is solid"

**drawBase**：只画边缘和 overlay，不画自己

## ColoredFloor / ColoredWall（可着色地面/墙壁）

- `saveData = true` / `editorConfigurable = true` / `saveConfig = true`
- `defaultColor = Color.white`
- 使用 `tile.extraData` 存储 RGBA 颜色
- ColoredFloor 特殊标记：`flagIgnoreDifferentColor=1`（忽略不同颜色）, `flagSmoothBlend=2`（平滑插值）
- ColoredWall 特殊标记：`flagIgnoreDifferentColor=1`, `flagApplyDarkness=2`（应用黑暗）

## 写环境类时的注意点

- `Floor` 是基础：地面效果、状态、液体、边缘、自动贴图都在这里。
- `OverlayFloor` 适合“叠在地面上的东西”，如矿层、表层装饰。
- `OreBlock` 必须有 `itemDrop`，否则初始化直接报错。
- `autotile = true` 时，`variants` 会被置 0，不能和普通 variants 的思路混用。
- `Tiling` / `TiledFloor` / `TiledWall` 依赖资源尺寸一致，否则会退化或出问题。
- `wallOre` 会影响覆盖在墙上的矿逻辑。
- `supportsOverlay`、`needsSurface`、`allowCorePlacement` 会直接影响可放置性。

## Related

- [BasicGenerator ore() 方法用法](mindustry-basic-generator-ore.md)
- [Mindustry Autotiler 用法](mindustry-autotiler.md)
- [Mindustry Rendering System](mindustry/rendering-system.md)
