# NewHorizonMod

- **仓库**: [Yuria-Shikibe/NewHorizonMod](https://github.com/Yuria-Shikibe/NewHorizonMod)
- **版本**: 2.0-TEST
- **作者**: Yuria (Yuria-Shikibe) & Lao (LaoHuaJi)
- **语言**: Java（381 源文件，59,430 行）
- **类型**: Java mod（`newhorizon.NewHorizon`）
- **minGameVersion**: v154
- **本地路径**: `源码和示例/mods/NewHorizonMod/NewHorizonMod-main`

## 定位

科幻主题大型 Mindustry mod，以其完整的过场动画/逻辑系统、丰富的 DrawBlock 和自定义子弹、RegionPart 动态渲染著称。是过场动画系统最完整的 mod 之一。

## 包结构

| 包 | 内容 |
|---|------|
| `newhorizon/` | 主入口（NewHorizon.java）+ NHInputListener、NHSetting、NHUI、NHPostProcess |
| `newhorizon/content/` | 内容定义：NHBlocks(2050行)、NHUnitTypes(多文件5081行)、NHBullets、NHShaders 等 |
| `newhorizon/content/blocks/` | 方块分类（15 文件） |
| `newhorizon/content/bullets/` | 子弹分类 |
| `newhorizon/content/register/` | RecipeRegister、UnitRecipeRegister（配方注册） |
| `newhorizon/content/units/` | 单位分类 |
| `newhorizon/expand/block/` | 方块实现（26+ 种）：ancient/commandable/consumer/defence/distribution/drawer/environment/flood/inner/liquid/power/production/special/stream/turrets |
| `newhorizon/expand/block/drawer/` | **17 种 DrawBlock**：DrawRotator、DrawScanLine、DrawRegionFlip、DrawRegionRotatedDiagonal、DrawParticleFlow、DrawArrowSequence、FlipRegionPart 等 |
| `newhorizon/expand/bullets/` | 子弹实现：ChainBulletType、AccelBulletType、DelayedPointBulletType、ShieldBreakerType、MissileSpawner |
| `newhorizon/expand/bullets/adapt/` | AdaptBulletType、AdaptLaserBulletType（自适应子弹） |
| `newhorizon/expand/bullets/raid/` | BasicRaidBulletType（突袭子弹） |
| `newhorizon/expand/ability/active/` | 主动能力 |
| `newhorizon/expand/ability/passive/` | 被动能力（Active/Passive 分离设计） |
| `newhorizon/expand/logic/` | **完整过场逻辑系统**（90+ 文件） |
| `newhorizon/expand/logic/components/` | Action/ActionBus/CutsceneControl/CutsceneUI + 20+ 具体动作 |
| `newhorizon/expand/logic/cutscene/` | 过场层（15 个逻辑语句 + actionBus + types） |
| `newhorizon/expand/logic/statements/cutscene/` | 语句层（BeginCutscene/AddMainActionBus + 15 个 action） |
| `newhorizon/expand/logic/wip/` | 实验性（GravityWell/RaidControl/RandomSpawn/SpawnBullet/TeamThreat） |
| `newhorizon/expand/entities/` | EntityRegister（实体注册） |
| `newhorizon/expand/units/` | 自定义单位：ai/、unitEntity/、unitType/、ablility/ |
| `newhorizon/expand/graphics/` | 自定义渲染 |
| `newhorizon/expand/net/packet/` | 7 个网络同步包 |
| `newhorizon/expand/game/` | MapMarker、MapObjectives |
| `newhorizon/expand/rules/` | 自定义规则 |
| `newhorizon/expand/type/` | 自定义类型 |
| `newhorizon/expand/draw/` | DrawLiquidSide4、DrawPistonsOffset、DrawRegionOffset、DrawLiquidAnimatedOffset、DrawLiquidSmelt |
| `newhorizon/util/` | 工具库：annotation/、feature/、func/、game/、generator/、graphic/、struct/、ui/（dialog/display/frag） |

## 核心特色

### 1. 完整过场逻辑系统（最完整）
- **components/ 层**: Action + ActionBus + CutsceneControl + CutsceneUI
- **20+ 具体动作**: CameraControl/CameraZoom/CurtainDraw/CurtainFade/InfoText/InputLock/SignalCut/Wait/WarningIcon 等
- **cutscene/ 层**: 15 个逻辑处理器语句 + actionBus（GetActions/InitActions/RunMainBus/RunSubBus/SaveActions）
- **types/**: HudIcon、RaidControllerType、RaidPreset、WarningSound
- **statements/ 层**: BeginCutscene + AddMainActionBus/AddSubActionBus + 15 个 action 逻辑语句

### 2. 17 种 DrawBlock
- DrawRotator（旋转器）、DrawScanLine（扫描线）
- DrawRegionFlip（镜像翻转）、DrawRegionRotatedDiagonal（对角旋转）
- DrawParticleFlow（粒子流）、DrawArrowSequence（箭头序列）
- FlipRegionPart（翻转区域部件）
- DrawArcSmeltRotated、DrawCrucibleFlameRotated、DrawFlameRotated
- DrawLiquidRegionRotated、DrawPrinter、DrawBaseRegion
- RadialEffectRotated、ArcCharge

### 3. 自适应子弹系统
- AdaptBulletType / AdaptLaserBulletType：根据目标自适应伤害类型
- ChainBulletType（链式传导）
- AccelBulletType（加速弹）
- DelayedPointBulletType（延迟点射）
- ShieldBreakerType（破盾弹）
- MissileSpawner（导弹产生器）
- HelixTrailBulletType（螺旋尾迹）

### 4. RegionPart 动态渲染
- 炮塔随单位部件旋转
- FlipRegionPart（翻转区域部件）

### 5. Recipe 配方系统
- RecipeRegister + UnitRecipeRegister
- 单位制造配方

### 6. EntityMapping 注册
- 通过 EntityMapping.nameMap 注册自定义实体类
- 存档正确序列化自定义实体

### 7. 设置同步
- NHSetting + NHPostProcess
- 客户端/服务器设置同步
- override_check 包处理器

### 8. 22 阶段内容加载
```
items → liquids → bullets → units → blocks → tech tree
```
严格排序加载管线，确保依赖关系正确。

## 值得学习的模式
- 完整过场逻辑系统（Action + ActionBus + CutsceneControl）
- 17 种 DrawBlock 的视觉效果变体
- Active/Passive 能力分离设计
- RegionPart 动态炮塔渲染
- EntityMapping 实体注册
- 客户端/服务器设置同步
- Recipe 配方制造系统

## 参考
- [NewHorizonMod GitHub](https://github.com/Yuria-Shikibe/NewHorizonMod)
- [MODS_INDEX](../../源码和示例/mods/MODS_INDEX.md)
