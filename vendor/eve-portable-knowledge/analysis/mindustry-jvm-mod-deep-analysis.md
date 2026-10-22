# Mindustry JVM Mod 深度解析

> 对本地 `源码和示例/mods/` 目录下所有可读取源码的 JVM（Java/Kotlin）mod 的系统化技术解析。
> 覆盖 8 个有效 JVM mod：ProjectUnity、NewHorizon、MultiCrafterLib、voidshield、BetaMindy、Extra-Utilities、mod-tools、Helium。
> 饱和火力（DEX 编译）无源码可分析，CurseOfFlesh 为空目录，不纳入。

**最后更新**: 2026-05-01

---

## 一、精确规模统计

| # | Mod | 语言 | .java 文件 | .kt 文件 | Java 行数 | Kotlin 行数 | 总行数 |
|---|-----|------|-----------|---------|-----------|-------------|--------|
| 1 | **ProjectUnity** | Java | 576 | 0 | 148,916 | 0 | 148,916 |
| 2 | **NewHorizon** | Java | 381 | 0 | 59,430 | 0 | 59,430 |
| 3 | **mod-tools** | Java | 406 | 0 | 66,302 | 0 | 66,302 |
| 4 | **BetaMindy** | Java | 201 | 0 | 33,273 | 0 | 33,273 |
| 5 | **Extra-Utilities** | Java | 179 | 0 | 33,065 | 0 | 33,065 |
| 6 | **Helium** | Kotlin | 2 | 67 | 455 | 12,071 | 12,526 |
| 7 | **voidshield** | Kotlin | 0 | 32 | 0 | 5,463 | 5,463 |
| 8 | **MultiCrafterLib** | Java+Kotlin | 22 | 1 | 3,610 | 47 | 3,657 |

**总计**: 1,767 个源文件，362,632 行 JVM 代码。

---

## 二、Mod 生命周期模式对比

所有 JVM mod 继承 `mindustry.mod.Mod`，通过三个核心钩子组织代码：

### 2.1 Constructor → loadContent() → init() 三阶段

| Mod | Constructor | loadContent() | init() |
|-----|------------|---------------|--------|
| **ProjectUnity** | 注册字体加载器（FreeTypeFontGenerator）、3D 模型加载器（ModelLoader）、初始化经验系统（ExpOrbs） | Items→StatusEffects→Weathers→Liquids→Bullets→WeaponTemplates→UnitTypes→Blocks→Planets→SectorPresets→TechTree | 绑定 postProcessor（TimeStop）、注册 Triggers、AntiCheat、添加 Credits 按钮 |
| **NewHorizon** | 注册输入绑定、事件监听（ContentInitEvent/PlayerConnect/ClientLoadEvent）、设置客户端包处理器 | 内容定义分散在 content/ 各子包 | NHUI 初始化、更新服务器列表、获取新版本 |
| **BetaMindy** | 初始化工具类（pushUtil/musics/MindySounds）、设置默认配置、注册 ClientLoadEvent/WorldLoadEvent/FileTreeInitEvent/DisposeEvent | Items→Liquids→Blocks→Bullets→UnitTypes→Planets→SectorPresets→TechTree | ScoreLib 计算物品/液体/单位评分、Hint 系统、商店系统 |
| **Extra-Utilities** | 注册 ClientLoadEvent、初始化 EUI/设置、加载自定义光标 | Sounds→Items→StatusEffects→Attribute→UnitTypes→Boss→Blocks→Planet→SectorPresets→TechTree | AtLoad、MainRenderer、Boss 免疫列表、设置界面 |
| **mod-tools** | 单实例校验（loaded 标志）、`Core.app.post(this::load0)` 延迟加载 | 无游戏内容 | loadCore()：IntUI/TopGroup/Frag、HopeInput、extending、HotSwap |
| **voidshield** | 无构造器逻辑 | VSBlocks.load() | uiDrawEnd→TestShader、VsVars、逻辑语句创建、分类更新 |
| **Helium** | 无构造器逻辑 | 无 loadContent（纯 UI mod） | He.init() |
| **MultiCrafter** | 无构造器逻辑 | MultiCrafter 内容注册 | — |

### 2.2 关键生命周期模式

**延迟加载**（mod-tools 特有）：
```java
public ModTools() {
    if (loaded) throw new IllegalStateException("already loaded.");
    if (ui != null && ui.hudGroup != null) isImportFromGame = true;
    Core.app.post(this::load0); // 延迟到主线程
}
```

**事件驱动构造**（BetaMindy/NewHorizon 特有）：构造器中大量 `Events.on()` 注册回调，访问 `Vars.ui` 等对象。

**单实例校验**（mod-tools 特有）：`loaded` 标志防止重复加载。

---

## 三、核心系统深度对比

### 3.1 方块系统（Block/Building）

| Mod | 自定义 Block 总数 | 关键包 | 代表类型 |
|-----|-------------------|--------|---------|
| **ProjectUnity** | 38+ | `unity/world/blocks/`（14 文件 17,427 行）：defense/distribution/effect/environment/light/power/production/sandbox/units 9 子包 | GraphBlockBase（图系统方块）、模块化单位装配器 |
| **NewHorizon** | 26+ | `expand/block/`：ancient/commandable/consumer/defence/distribution/drawer/environment/flood/inner/liquid/power/production/special/stream/turrets | Ancient 方块、Commandable 方块、Flood 方块 |
| **BetaMindy** | 58+ | `world/blocks/`：campaign/defense/distribution/environment/logic/payloads/power/production/storage/units | Shop/DailyStore（经济）、PayloadFactory（载荷线）、GOLBlock（生命游戏）、Pen（绘画） |
| **Extra-Utilities** | 26+ | `worlds/blocks/`：distribution/effect/heat/liquid/logic/power/production/turret/unit | HeatDriver、BatteryNode、SpaceGenerator、CrystalTower（塔防） |
| **MultiCrafter** | 1 | `lib/src/multicraft/` | MultiCrafter（PayloadBlock + HeatBlock + HeatConsumer） |
| **voidshield** | 3 | `world/blocks/` | HeatCatheter（远程导热管）、VelumSolvent、MicroVoid |

### 3.2 炮塔系统（Turret）

| Mod | 炮塔数 | 代表类型 |
|-----|--------|---------|
| **ProjectUnity** | 17 | `defense/turrets/`（17 文件）— Monolith 系列巨型炮塔、AntiCheat 炮塔 |
| **NewHorizon** | 5 | `expand/block/turrets/` |
| **BetaMindy** | 7 | CoinTurret（货币弹炮塔）、DrillTurret（钻头炮塔）、RayTurret、UnitTurret（召唤炮塔）、PayloadTurret、HijackTurret、RepairTurret |
| **Extra-Utilities** | 24 | `turret/`（18）+ `turret/wall/`（3）+ `turret/TowerDefence/`（2）— Prism（棱镜折射）、antiMissile（反导）、CrystalTower（塔防）、Domain（领域墙）、MultiShootTurret（齐射） |

### 3.3 子弹系统（BulletType）

| Mod | 子弹数 | 代表类型 |
|-----|--------|---------|
| **ProjectUnity** | 25+ | `entities/bullet/` 按类别分 6 子包：anticheat（16）、energy（22）、exp（8）、kami（5）、laser（11）、misc/monolith/physical |
| **NewHorizon** | 13+ | `expand/bullets/` + `adapt/` + `raid/` — ChainBulletType、AccelBulletType、DelayedPointBulletType、ShieldBreakerType、MissileSpawner |
| **BetaMindy** | 16 | `entities/bullet/` — CoinBulletType、FallingStar、RainbowLaser、SequenceBulletType、SoundwaveBulletType、HomingPayloadBullet、ItemTrailBulletType |
| **Extra-Utilities** | 25+ | `worlds/entity/bullet/`（22）— BlackHoleBullet、ChainLightning、CtrlMissile（可控导弹）、DeathLaser、ElectricStorm、PrismLaser（棱镜折射）、PercentDamage（百分比伤害） |

### 3.4 DrawBlock 系统

| Mod | DrawBlock 数 | 代表类型 |
|-----|-------------|---------|
| **ProjectUnity** | 5 | `unity/world/draw/` — DrawExp、DrawLightBlock；`unity/v8/` — DrawSmelter、DrawLiquid、DrawGlow |
| **NewHorizon** | 17 | `expand/block/drawer/`（17 文件）— DrawRotator、DrawScanLine、DrawRegionFlip、DrawRegionRotatedDiagonal、DrawParticleFlow、DrawArrowSequence、FlipRegionPart |
| **BetaMindy** | 2 | `world/draw/` — DrawCondenser、DrawBlockShadow |
| **Extra-Utilities** | 25 | `worlds/drawer/`（25 文件）— DrawRainbow（彩虹渐变）、DrawElectric（电弧）、DrawTrail（拖尾）、DrawBow（弓形）、RunningLight（流动灯光）、DrawHeatDriver |
| **MultiCrafter** | 2 | `lib/src/multicraft/` — DrawRecipe、DrawHeatRegion |
| **voidshield** | 1 | `other/drawers/DrawHeat.kt` |

### 3.5 能力系统（Ability）

| Mod | 能力数 | 代表类型 |
|-----|--------|---------|
| **ProjectUnity** | 9+ | `entities/abilities/` — BulletReflectPulseAbility、DirectionShieldAbility、LightningBurstAbility、SlashAbility、TeleportAbility、TimeStopAbility |
| **NewHorizon** | 7+ | Active/Passive 分离（`expand/ability/active/` + `passive/`） |
| **Extra-Utilities** | 10 | `worlds/entity/ability/` — BatteryAbility、BoostAbility、PcShieldArcAbility、TerritoryFieldAbility、bossUnitAbi、preventCheatingAbility |
| **voidshield** | 1 | `entities/abilities/TeleportAbility.kt`（251 行） |

### 3.6 AI 系统

| Mod | AI 数 | 代表 |
|-----|-------|------|
| **ProjectUnity** | 12 | `unity/ai/`（12 文件）— KamiAI（含 KamiBulletDatas/KamiPattern/KamiRegions）、WormAI、HealerAI、LinkedAI、MonolithSoulAI |
| **NewHorizon** | 3 | `expand/units/ai/` |
| **Extra-Utilities** | 3 | `ai/` — DefenderHealAI、Firefighter、MinerPointAI |

### 3.7 网络同步系统

| Mod | 网络包数 | 代表 |
|-----|---------|------|
| **ProjectUnity** | 2 | `sync/UnityCall` + `sync/packets/BasePacket` |
| **NewHorizon** | 7 | `expand/net/packet/` + `netClient.addPacketHandler("override_check")` |
| **Extra-Utilities** | 7 | `net/` — CtrlMissileSyncPacket、DomainSyncPacket、ExplodeWallPacket、MinerPointConfigCallPacket、ReleaseShieldWallBuildSyncPacket、SetFloorOnlyPacket + EUCall |
| **mod-tools** | 4 | `net/packet/` — HopeCall + SetBlockPacket、UnitSpawnPacket |

### 3.8 着色器系统

| Mod | 着色器数 | 代表 |
|-----|---------|------|
| **ProjectUnity** | 8 | `assets/shaders/` + `graphics/UnityShaders`（含 FragmentationShader、VapourizeShader、StencilShader、PlanetObjectShader、MegalithRingShader） |
| **BetaMindy** | 6 | `assets/shaders/` — bittrium.frag、coffee.frag、dreamjelly.frag、spacefire.frag、starwater.frag、blockregion.vert |
| **Extra-Utilities** | 1 | `graphics/MainShader.java` |
| **voidshield** | 2 | `world/shaders/TestShader.kt`（367 行） + shader 管线（ScreenSampler→ShaderManage→VSShaderLoader） |
| **mod-tools** | 8 | `assets/shaders/` — bloom_blur、dist_base、frontOnly、gaussian_blur、mix、毛玻璃、高斯模糊 |
| **Helium** | 6 | `assets/shaders/` — blur_base、dist_base、entity_range、entity_range_low、gauss_blur（.frag + .vert） |

### 3.9 逻辑系统扩展

| Mod | 逻辑系统 | 规模 |
|-----|---------|------|
| **ProjectUnity** | `unity/logic/` — ExpContentList、ExpSenseI、ExpSensorStatement | 小 |
| **NewHorizon** | `expand/logic/` — **最完整**：cutscene/（CameraControl/CurtainDraw/InfoText/InputLock/Wait 等 15+ 动作）、statements/（BeginCutscene/AddMainActionBus 等）、components/（ActionBus/CutsceneUI/MarkBox）、wip/（GravityWell/RaidControl/RandomSpawn） | 90+ 文件，完整过场逻辑系统 |
| **voidshield** | `other/extends/logicExtend/` — LTeleport（传送逻辑）、VSSensor/VSControl（Void Shield 传感器/控制）、SensorMode/ControlMode（模式枚举）、velumSolventLogic（VelumSolvent 传感器） | 中等 |

### 3.10 UI 系统

| Mod | UI 特色 | 规模 |
|-----|---------|------|
| **ProjectUnity** | CinematicDialog（过场对话框）、CreditsDialog、ScriptsEditorDialog（脚本编辑器）、ScriptsDictionaryDialog、ObjectivesDialog、TagsDialog | 14 文件 |
| **NewHorizon** | NHUI + TableFunc + NewFeatureDialog、NHSetting、NHInputListener | 中等 |
| **BetaMindy** | HardmodeFragment、PlacementInvFragment、TouchpadFragment、ManualDialog、MindyUILoader | 中等 |
| **Extra-Utilities** | EUI + RogueLikeStart（Roguelike 开局）、DDItemsList、ItemDisplay | 中等 |
| **mod-tools** | **最丰富**：IntUI + TopGroup + Frag（914 行）、ReviewElement（1885 行）、语法高亮（JSSyntax/JavaSyntax）、代码补全（CompletionPopup）、火焰图（FlameGraphWindow）、ColorPicker、DrawablePicker、WatchWindow、ShowInfoWindow | 40+ UI 文件 |
| **Helium** | HePlacementFrag（1103 行）、EntityInfoFrag（956 行）、HeModsDialog（805 行）、HeModsBrowser（583 行）、ModPackerDialog（862 行）、EntityRangeDisplay | 20+ UI 文件 |

---

## 四、独有技术系统

### 4.1 ProjectUnity — Graph<T> 图系统

```
unity/world/graphs/
├── Graph.java          — 抽象基类（accept/multi/type/module）
├── BaseGraph.java       — 泛型图实现（connected 集合、合并逻辑）
├── GraphHeat.java       — 热力图
├── GraphTorque.java     — 力矩图（含 Consume/Generate/Trans 三种节点）
├── GraphCrucible.java   — 冶金图
├── GraphFlux.java       — 流量图
└── Graphs.java          — 图注册中心

unity/world/graph/       — 图行为实现
├── HeatGraph.java       — 热传导模拟（heat → heatBuffer → lastHeatFlow）
├── TorqueGraph.java     — 力矩物理（force → friction → inertia → velocity）
├── CrucibleGraph.java   — 冶金模拟
└── BaseGraph.java       — 图合并时物理量守恒
```

**核心设计**：
- `GraphModule<T, M, G>` 泛型模块（neighbours 映射、acceptPorts 端口序列化）
- 帧 ID 去重更新机制（`lastRecalc != build.rotation()` 检测变化）
- Gauss-Seidel 扩散算法（热/力矩平衡）
- 图合并时动量守恒：`momentumA = velocityA * inertiaA`，合并后 `velocity = (momentumA + momentumB) / (inertiaA + inertiaB)`

### 4.2 ProjectUnity — 3D 模型渲染

```
unity/assets/type/g3d/
├── Model.java / ModelInstance.java / Models.java — 模型数据
├── ModelLoader.java       — 资源加载器
├── Node.java / NodePart.java / NodeAnimation.java — 场景图节点
├── Renderable.java / RenderableProvider.java / RenderableSorter.java — 渲染管线
├── attribute/              — 材质属性
│   ├── light/             — DirectionalLight、PointLight、SpotLight、ShadowMap
│   └── type/              — BlendingAttribute、ColorAttribute、TextureAttribute
└── model/                 — ModelData、ModelMesh、ModelMaterial、ModelNode
```

这是 Mindustry mod 中**唯一的完整 3D 渲染管线**。

### 4.3 ProjectUnity — 注解处理器代码生成

`unity/gen/` 包含 **105 个自动生成的 Java 文件**（77,003 行），涵盖：
- WormUnit/WormEndUnit（虫形单位）
- MonolithMechUnit/MonolithLegsUnit/MonolithSoul（巨石单位）
- LegsEndUnit/TallTentacleLegsEndUnit（腿部单位）
- InvisibleEndUnit/DecorationWaterMoveEndUnit（装饰单位）
- TimeStopVelEndUnit/BoostEndUnit（特殊能力单位）
- Oppression（压制作战单位）

通过 `@EntityDef`、`@Merge`、`@Dupe` 注解 + JavaPoet 实现。

### 4.4 NewHorizon — 过场逻辑系统（最完整）

```
expand/logic/
├── components/        — 组件层
│   ├── Action.java / ActionBus.java — 动作总线
│   ├── CutsceneControl.java / CutsceneUI.java — 过场控制/UI
│   ├── action/       — 20+ 具体动作
│   │   ├── CameraControlAction / CameraResetAction / CameraZoomAction
│   │   ├── CurtainDrawAction / CurtainFadeInAction / CurtainFadeOutAction / CurtainRaiseAction
│   │   ├── InfoFadeInAction / InfoFadeOutAction / InfoTextAction
│   │   ├── InputLockAction / InputUnlockAction
│   │   ├── SignalCutInAction / SignalCutOutAction / SignalTextAction
│   │   ├── UIHideAction / UIShowAction / WaitAction
│   │   └── WarningIconAction / WarningSoundAction
│   └── ui/           — MarkBox / MarkStyle
├── cutscene/         — 过场层
│   ├── action/       — 15 个逻辑处理器语句（CameraControl/CameraZoom/CurtainDraw 等）
│   ├── actionBus/    — GetActions/InitActions/RunMainBus/RunSubBus/SaveActions
│   └── types/        — HudIcon/RaidControllerType/RaidPreset/WarningSound
├── statements/       — 语句层
│   └── cutscene/     — BeginCutscene/AddMainActionBus/AddSubActionBus + 15 个 action 语句
└── wip/              — 实验性
    ├── GravityWell / LineTarget / RaidControl / RandomSpawn / RandomTarget / SpawnBullet / TeamThreat
```

### 4.5 MultiCrafterLib — 多配方解析架构

```
lib/src/multicraft/
├── MultiCrafter.java     — 主方块（PayloadBlock + HeatBlock + HeatConsumer）
├── Recipe.java           — 配方数据结构
├── IOEntry.java          — 输入输出条目
├── MultiCrafterParser.java — 解析器
├── RecipeParserException  — 解析异常
├── RecipeSwitchStyle     — 配方切换样式
├── ContentResolver       — 内容名称→对象解析
├── ConsumeFluidDynamic   — 动态液体消费
├── CustomConsumePayloadDynamic — 动态载荷消费
├── DrawRecipe.java       — 配方切换渲染
└── DrawHeatRegion.java   — 热力渲染

main/src/
├── MultiCrafterMod.java  — 主入口
├── MultiJavaAdapter.java — 为 JS mod 生成字节码适配器
└── TestBlocks.java       — 测试方块

injection/src/
└── MultiCrafterAdapter.java — 注入适配器
```

### 4.6 voidshield — 热力学 + Shader 管线

```
world/HeatBlock.kt (221行)    — 基础温度20°C，比热容控制升降温
world/blocks/HeatCatheter.kt (343行) — 远程导热管，范围5格，tempChanges[]数组双遍热平衡

shader/
├── ScreenSampler.kt (118行)  — 屏幕采样器
├── ShaderManage.kt (95行)    — 着色器管理
├── VSShaderLoader.kt (51行)  — 着色器加载器
└── ../world/shaders/TestShader.kt (367行) — 完整着色器

other/dateTypes/SpaceDate.kt (1012行) — QuadTree 空间索引 O(log n) 查询
other/interfaces/SpaceDateInterface.kt (339行) — 射线投射点面判断
```

### 4.7 mod-tools — HotSwap + 反射 + 火焰图

```
主 src (37,919行)
├── unsupported/
│   ├── HotSwapController.java — 热替换控制器
│   ├── HotSwapManager.java    — 热替换管理器
│   └── JDWP.java              — Java 调试线协议
├── jsfunc/reflect/
│   ├── REFLECT.java           — JS 反射 API
│   ├── UNSAFE.java            — JS Unsafe 操作
│   └── InitMethodHandle.java  — 方法句柄
├── utils/
│   ├── ByteCodeTools.java (540行) — ASM 9.5 字节码操作
│   └── profiler/SamplingProfiler.java — 采样分析器
└── ui/windows/profile/FlameGraphWindow.java — 火焰图可视化

子项目 (21,618行)
├── hotswap-agent/ (26 文件, 4,962行) — HotSwapAgent.java (980行)
├── compiler/ (43 文件, 7,519行) — 编译器
├── jni-agent/ (13 文件, 3,121行) — JNI/JVMTI 代理
├── annotations/ (26 文件, 596行) — 注解处理器
└── utils-agent/ (1 文件, 10行)

依赖库
├── asm-9.5.jar — ASM 字节码框架
├── compiler.jar — 编译器
├── magicClass.jar — 魔法类
└── AndroidField/Android_dalvik/Mindustry_android — Android 兼容
```

### 4.8 Helium — UI 优化 + 实体信息

```
src/main/kotlin/helium/
├── Helium.kt / He.kt (413行) — 主入口与核心初始化
├── HeConfig.kt (310行) — 配置系统
├── graphics/
│   ├── DrawUtils.kt (532行) — 绘制工具
│   ├── NinePatchClipDrawable.kt (511行) — 可裁剪 Drawable
│   └── g2d/ — 2D 图形扩展
├── ui/
│   ├── fragments/
│   │   ├── placement/HePlacementFrag.kt (1103行) — 增强建造面板
│   │   └── entityinfo/
│   │       ├── EntityInfoFrag.kt (956行) — 实体信息主 Fragment
│   │       ├── EntityInfoDisplay.kt (193行) — 信息显示基类
│   │       └── displays/
│   │           ├── HealthDisplay.kt (365行) — 血量/护盾显示
│   │           └── EntityRangeDisplay.kt (247行) — 攻击范围指示
│   ├── dialogs/
│   │   ├── mods/HeModsDialog.kt (805行) + HeModsBrowser.kt (583行) — Mod 管理器
│   │   ├── modpacker/ModPackerDialog.kt (862行) — Mod 打包器
│   │   └── database/ — 数据库浏览
│   └── HeStyles.kt (195行) — 样式定义
└── util/
    ├── Downloader.kt (207行) — 下载器
    └── binds/ — 输入绑定
```

---

## 五、构建系统对比

| Mod | 构建工具 | Java 版本 | 特殊依赖 |
|-----|---------|----------|----------|
| **ProjectUnity** | Gradle | source 9 / target 8，使用 Jabel | FreeType、3D 模型（Wavefront OBJ） |
| **NewHorizon** | Gradle | 未确认 | — |
| **BetaMindy** | Gradle | Java | alpha-bleed（贴图工具） |
| **Extra-Utilities** | Gradle | source 17 / target 8 | Jabel、Zelaux repo |
| **mod-tools** | Gradle + Kotlin plugin | Java + Kotlin | ASM 9.5、magicClass、Android SDK、3 个 Agent 子项目 |
| **voidshield** | Gradle | Kotlin | — |
| **Helium** | Gradle + Kotlin plugin | Kotlin | — |
| **MultiCrafter** | Gradle | Java | PayloadBlock + HeatBlock |

**关键构建技巧**：
- **Jabel**：ProjectUnity 和 Extra-Utilities 使用 Jabel 让开发者书写 Java 9+ 语法（switch 表达式、var），编译输出 Java 8 字节码
- **Agent 子项目**：mod-tools 的 hotswap-agent/jni-agent/utils-agent 独立构建，通过 `--add-opens` 访问 JDK 内部 API
- **Multi-source 模块**：mod-tools 在一个 Gradle 项目中混合 Java 和 Kotlin 源码

---

## 六、关键设计模式总结

### 6.1 内容加载管线

```
所有游戏内容 mod 遵循相同顺序：
Items → StatusEffects → Liquids → Bullets → Weapons → UnitTypes → Blocks → Planets → SectorPresets → TechTree

Exceptions:
- ProjectUnity 额外加载 Weathers、WeaponTemplates、Parts、Overwriter、FactionMeta
- Extra-Utilities 额外加载 Sounds、Attribute、Boss
- voidshield 只加载 Blocks（最小化）
```

### 6.2 自定义 DrawBlock 模式

```
DrawBlock (基类)
  ├── DrawMulti → 多层渲染组合
  │     ├── DrawRegion → 贴图区域
  │     ├── DrawGlow → 发光
  │     └── DrawPistons → 活塞动画
  ├── DrawFlame → 火焰效果
  └── 完全自定义 → 重写 draw/buildSimple 方法

Extra-Utilities 的 25 种 DrawBlock 是最佳参考。
NewHorizon 的 17 种 DrawBlock 展示了旋转/镜像/粒子流等变体。
```

### 6.3 网络同步模式

```
1. 定义 Packet（实现 Packet 接口）
2. 在 Call 类中封装发送方法（EUCall/HopeCall/UnityCall）
3. init() 中注册 PacketHandler
4. serverPacketHandler 中权限验证
```

### 6.4 屏幕采样管线（3 家独立实现）

```
FrameBuffer.bind() → 渲染场景 → unbind()
      ↓
ScreenSampler 捕获当前帧纹理
      ↓
Shader.setUniformInput() → 绑定采样纹理
      ↓
Draw.rect() 回贴到屏幕

voidshield: ScreenSampler.kt → TestShader.kt
Helium: 6 个着色器（模糊/毛玻璃/实体范围）
mod-tools: ScreenSampler + BufferCapturer + EBBlur
```

### 6.5 图系统物理模拟

```
ProjectUnity 的 Graph<T> 系统：
GraphModule<Graph, Module, Network> 泛型模块
    ↓
neighbours: OrderedMap<Module, Integer> 邻接映射
    ↓
BaseGraph<Module, Graph> 图网络
    ↓
merge(): 合并两个图，物理量守恒
    ↓
updateGraph(): 每帧更新（Gauss-Seidel 迭代）
    ↓
TorqueGraph: velocity += (netForce / inertia) * Time.delta
HeatGraph: heat += heatBuffer → lastHeatFlow
CrucibleGraph: 冶金模拟
```

---

## 七、学习路径建议

| 学习目标 | 首选参考 | 次选参考 | 文件/包 |
|---------|---------|---------|---------|
| **Mod 生命周期** | BetaMindy（清晰） | Extra-Utilities（完整） | `BetaMindy.java` / `ExtraUtilitiesMod.java` |
| **图系统** | ProjectUnity | — | `unity/world/graphs/` + `unity/world/graph/` |
| **自定义方块** | Extra-Utilities（种类多） | BetaMindy（58+种） | `worlds/blocks/` / `world/blocks/` |
| **自定义炮塔** | Extra-Utilities（24种） | ProjectUnity（17种） | `worlds/blocks/turret/` |
| **自定义子弹** | ProjectUnity（25+种，分6子包） | Extra-Utilities（25+种） | `entities/bullet/` |
| **自定义 DrawBlock** | Extra-Utilities（25种） | NewHorizon（17种） | `worlds/drawer/` / `expand/block/drawer/` |
| **自定义能力** | Extra-Utilities（10种） | ProjectUnity（9种） | `worlds/entity/ability/` |
| **自定义 AI** | ProjectUnity（12种） | Extra-Utilities（3种） | `unity/ai/` |
| **网络同步** | Extra-Utilities（7包） | NewHorizon（7包） | `net/` / `expand/net/` |
| **逻辑系统扩展** | NewHorizon（完整 cutscene） | voidshield（最小化） | `expand/logic/` |
| **过场动画** | NewHorizon | ProjectUnity | `expand/logic/cutscene/` + `statements/` |
| **多配方工厂** | MultiCrafterLib | — | `lib/src/multicraft/MultiCrafter.java` |
| **热管理** | voidshield | ProjectUnity | `HeatCatheter.kt` / `HeatGraph.java` |
| **3D 模型渲染** | ProjectUnity | — | `unity/assets/type/g3d/` |
| **注解代码生成** | ProjectUnity | — | `unity/gen/`（105 文件） |
| **着色器管线** | voidshield（完整管线） | BetaMindy（6种 shader） | `shader/` / `assets/shaders/` |
| **UI 系统** | mod-tools（最丰富） | Helium（最实用） | `ui/` / `ui/fragments/` |
| **反射/字节码** | mod-tools | — | `jsfunc/reflect/` + `utils/ByteCodeTools.java` |
| **HotSwap** | mod-tools | — | `unsupported/HotSwapController.java` |
| **火焰图** | mod-tools | — | `utils/profiler/SamplingProfiler.java` |
| **Kotlin mod** | Helium（实用） | voidshield（简洁） | `helium/` / `voidshield/` |

---

## 八、交叉引用

详情参见各 mod 独立知识页面：
- [ProjectUnityPublic](../entities/project-unity.md)
- [BetaMindy](../entities/betamindy.md)
- [Extra-Utilities](../entities/extra-utilities.md)
- [mod-tools](../entities/mod-tools.md)
- [Helium](../entities/helium.md)
- [VoidShield](../entities/voidshield.md)
- [Mindustry Mod Collection](../entities/mindustry-mod-collection.md)
