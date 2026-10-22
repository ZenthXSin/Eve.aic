# BetaMindy

- **仓库**: [sk7725/BetaMindy](https://github.com/sk7725/BetaMindy)
- **版本**: 2.0（v8-beta 分支 `4775723`）
- **作者**: sk7725
- **语言**: Java（201 源文件，33,273 行）
- **类型**: Java mod（`betamindy.BetaMindy`）
- **minGameVersion**: 155.4
- **本地路径**: `源码和示例/mods/BetaMindy`

## 定位

"Demo of Chaos Itself"——sk7725 的第一个 Java mod，以混乱、趣味性和实验性为核心，包含大量创意方块、武器、UI 组件和小游戏机制。

## 包结构

| 包 | 内容 |
|---|------|
| `content/` | 16 个内容定义类（Blocks, Items, Liquids, Bullets, Fx, StatusEffects, Weathers, Planets, Sectors, TechTree, Sounds, ShopItems, LorePages 等） |
| `type/` | 自定义类型：ColloidLiquid, ForceLiquid, StarryLiquid, LayerStatusEffect, InflictStatusEffect, SpecialSectorPreset |
| `type/item/` | ForeignItem, RandomAnimatedItem |
| `type/shop/` | 商店系统：ShopItem, ItemItem, LiquidItem, BlockItem, PurchaseCoin, PurchaseDrink 等 |
| `type/weather/` | BlockWeather, IonWind |
| `world/blocks/` | 7 大类方块（见下方） |
| `world/draw/` | DrawBlockShadow, DrawCondenser |
| `entities/bullet/` | 16 种自定义子弹类型 |
| `graphics/` | ColorMesh, Drawm, FireColor, MindyShaders, Pal2 |
| `planets/` | SharMoonGenerator |
| `ui/` | 10 个 UI 组件（HardmodeFragment, ManualDialog, PlacementInvFragment, TouchpadFragment 等） |
| `util/` | 工具类：HardMode, InventoryModule, MusicControl, ScoreLib, ThickLightning, UnitGravity 等 |

## 方块分类（world/blocks/）

| 子包 | 代表方块 |
|------|---------|
| `campaign/` | Altar, Barrier, Terraformer, LoreManual |
| `defense/` | AbsorbWall, Campfire, Crusher, Spike, IgniteWall, JointWall, MissileLauncher |
| `defense/turrets/` | CoinTurret, DrillTurret, HijackTurret, MultiTurret, PayloadTurret, RayTurret, RepairTurret, SentryTurret, UnitTurret |
| `defense/turrets/pattern/` | TurretPattern, ChaosBusterPattern |
| `distribution/` | Piston, PistonArm, Spinner, RailSorter, SlimeBlock, PayloadRail |
| `environment/` | Coin, Crystal, DecorativeFloor, GlowPowder, PresentBox, RuneBlock, ShaderCrystal, FallenUnit |
| `logic/` | GOLBlock, Pen, NotePlayer, MessageSource, ProcessorCooler, LinkPinner, FilterDome |
| `power/` | BlackholeGenerator, ArtificialSun, IsotopeReactor, Capacitor, FloodLight, ImpactCrafter |
| `production/` | BlockCloner, Condenser, SpaceCrafter, DrinkCrafter, LiquidRefiner |
| `production/payduction/` | PayloadFactory, PayloadFurnace, Centrifuge, BittriumFurnace（载荷生产线） |
| `storage/` | Shop, Store, Chest, Box, AnucoinVault, TradingPost, ResearchCenter, DailyStore |
| `units/` | ClearPipe, TeleportPad, TeleportPortal, BoostPad, Bumper, Claw, UnitFan, UnitPad, Yutnori |
| `payloads/` | RBuild |

## 特色系统

- **商店/货币系统**: Anucoin 货币 + Shop/DailyStore/TradingPost 经济链
- **载荷生产**: payduction/ 子包实现 PayloadFactory/PayloadFurnace/Centrifuge 载荷生产线
- **自定义子弹**: 16 种（CoinBullet, FallingStar, RainbowLaser, HomingPayloadBullet, SequenceBulletType 等）
- **自定义着色器**: 7 个 GLSL 着色器（bittrium, coffee, dreamjelly, milkyway, spacefire, starwater, blockregion）
- **UI 扩展**: HardmodeFragment, PlacementInvFragment, TouchpadFragment 等放置与战斗 UI
- **行星生成**: SharMoonGenerator
- **Hard Mode**: HardMode 工具类，提供难度增强机制
- **逻辑扩展**: GOLBlock（生命游戏）、Pen（绘画）、NotePlayer（音符）、MessageSource
- **小游戏/趣味方块**: Yutnori, ClearPipe, Bumper, Spinner

## 值得学习的模式

- 清晰的 content/ 包组织（每种内容类型独立一个定义类）
- 商店/货币经济系统实现
- payduction/ 载荷生产链
- 自定义子弹类型的多样性
- 着色器管线（assets/shaders/）
- UI Fragment 扩展模式

## 参考

- [BetaMindy GitHub](https://github.com/sk7725/BetaMindy)
- [Trello 计划板](https://trello.com/b/AiElGCt1)
