# 旧 Eve 长期记忆：技术节选

> 节选自 eve-portable `memory/MEMORY.md` 的技术小节；去掉了含本机路径、主机、凭据模板的行。
> 这些是旧 Eve 在实际项目中记下的经验，个别结论对应的游戏版本以原文为准。

## Mindustry 关键机制
- headless 无 Core.scene/Core.atlas：仅 Table 布局 + bar/borderImage/reqImage/gridImage 可测；label/button/slider 等需 `scene.getStyle`、`Image()` 无参需 atlas 会 NPE
- 状态效果反应：`affinity` 单向（own `transitions` only），`opposite`/`affinity` 通过 `transitions.put` 覆盖
- Weapon 关键字段：`recoils`（多管独立后座）、`extraVelocity`（velocityScl 公式）、`minShootVelocity`（开火前置条件，flares=2f/horizon=1f/quad=0.01f）、`sublimate` 用 ContinuousFlameBulletType
- UnitType/Weapon 缺 `mass`/`shieldHealth` 等字段；用 ShieldArcAbility 等能力替代
- `Block.init()` 在 `Block.load()` 之前运行（ClientLauncher.java 确认）
- `Drawf.light()` 检查 `renderer == null` 但不检查 `renderer.lights == null` — 潜在 NPE
- Shader mod: Java + GLSL，经 `Vars.mods.getMod(...).root.child("shaders")` 加载；JSON mod 无法触发屏幕震动
- `Vars.maxBlockSize = 16` 硬编码；20 尺寸方块不可行
- `enableDrawStatus` 控制状态指示器；不显示常见原因：字段 false、无消费者、`draw()` 缺 `super.draw()`
- Turret 层级：Turret → ItemTurret/LiquidTurret/PowerTurret/LaserTurret/ContinuousTurret/.../RepairTurret
- `dump()` 是通用方法，大量方块自实现输出逻辑不调用它
- `Seq().get(-1)` 抛 ArrayIndexOutOfBoundsException; `InputHandler.inv` 可反射修改
- `productionValid` 默认 true，与 `shouldConsume()` 不同
- reversed-Z 深度缓冲修复：反转投影**整个第三行** `val[2]/[6]/[10]/[14]`（非仅 M22/M23，因相机有 pitch 旋转）+ `Gl.depthFunc(gl.Greater)` + `clearDepthf(0)`；恢复 LESS+clear-1。知识库 `knowledge/reversed-z-depth-buffer.md`
- 远距离条纹先区分几何走样（aliasing/moiré，子像素采样不足）vs z-fighting——用像素边缘密度/规律性判断，勿一律归因深度碰撞


## MultiCrafter 项目
- v2 修复：getFilter 不安全临时块、注释掉的效率计算、read() 双重读取、craft() 重复输出
- 电力冲突根因：`shouldConsume()` 检查 `power.status < 0.01f`；解法 override `updateConsumption()` + 原生 ConsumePower
- 存档读取用 id 查找而非索引（防列表变动错位）
- 部署：d8 PATH 从 $ANDROID_HOME/build-tools 自动扫描；产物 IE.jar 2.6M (MD5 085d84a7...)
- 构建坑：deploy 的 doLast 删 jar 后任务 UP-TO-DATE 不重建 → d8 NoSuchFileException；解法 `rm -rf build/libs && ./gradlew deploy --rerun-tasks`
- Consume 的 display 保留 raw Table（Mindustry StatValues 叶子强制要求）


## RingWorld 修复（NewHorizon）
- 完成了环世界远端 z-fighting 修复（reversed-Z）；恒星渲染在环后问题通过环 mesh 内用同一反转域+GREATER 重画父恒星解决
- 远端"条纹"实为几何走样（48 traceRib + 9 traceBand 亚像素摩尔纹），非深度问题；修复方向：远距离 LOD 淡出/剔除细节、减少 rib、MSAA
- Key lesson：不要把远距离条纹一律归为 z-fighting


## MWU (Modular-Weapon-Unit)
- Android 崩溃根因：`WeaponEditorDialog` 全静态字段 + `hide()` 置空 → NPE；修复不置空静态、传 SlotCanvas 实例
- 武器预设：`recoilTime = reload`；EMPTY = 10f；电力系统（机枪15/霰弹22/榴弹40/导弹28/激光55/闪电30/火焰35/EMP45）；powerCost 字段上限 200、被动回充 8/s、失能阈值 -100、DATA_VERSION 2
- 物品尺寸系统：`WeaponPreset.size` (1=S,2=M,3=L,0=any)；槽位限制 slotMaxSize={3,2,2}；UI 过滤尺寸不匹配的预设
- 空武器黑块根因：EMP 预设名 "empty" 与 EmptyFloor 撞名 → 图集命中纯黑 empty.png；修复 `WeaponPreset.EMPTY_WEAPON_NAME="mwu-empty-weapon"` + byName 映射旧档 "empty"
- 槽位配置独立于单位武器：`SlotDef` 无 name，编辑器用 `MWMod.layoutOf(unit)` 而非 `mounts[].weapon`；`WeaponSize.label()` 按等级键 `mwu.sizeLevel.{level}`，setStats 显示完整标签
- **扩展性架构**：WeaponPreset 动态注册表（ObjectMap registered，register/unregister/all/ensureScanned）；MWMod 公开 registerWeaponPreset/unregisterWeaponPreset/getWeaponPresets 供 mod 间调用；MWUnitType `SlotDef[] layout` 支持多单位独立布局（null=回退全局 SLOTS）
- 全 i18n：bundle.properties 80 key；预设名英文 id（machine-gun/...）；所有 UI 用 `Core.bundle.get/format("mwu.xxx")`
- 重构教训：文件级重构用完整唯一块匹配，禁止宽 regex 替换多行（曾吞掉类头部、丢失 EMPTY lambda）
- 当前 jar: build/libs/modular-weapon-unit.jar (~88KB, minGameVersion 159, contains classes.dex)


## IE UI DSL
- headless 验证通过：`[UI-DSL-TEST][scene-safe] OK children=6 cells=10`；5 项 scene 控件正确 SKIP
- 沉淀为 skill `mindustry-headless-ui-test`（模板 + 脚本 assets/run-headless-ui-test.sh）
- build.gradle 打包 mod.hjson（非 mod.json）；assets 缺 mod.json 曾致 jar 无 meta


## Helium (he) Mod — v159.7
- 7 个旧文件选择器调用迁移到新 FileChooser builder API；Kotlin 需 `import mindustry.ui.FileChooser` 非 `.*`
- Keybinding: `entityInfoHotKey` 移至 HeKeyBindings；7 bundles 增加
- 移除：快捷物品栏 + 实体信息显示；保留折叠按钮 (Q)、悬浮信息窗格、方块选择面板
- 构建：`ANDROID_HOME=/opt/android-sdk ./gradlew deploy`；Helium.jar 11.06MB 分 2 卷发 QQ
- 构建产物：`server-release.jar` 是 158.1，IE 用它验证**从未真正加载**（minGameVersion 159 被静默跳过）— 已换用官方 159.7


## Teleport-Unlock
- 功能：teleport_cursor 键全模式+联机可用；架构 TeleportPacket（服务器权威 set + Call.setPosition）
- 关键坑：①jar 须 .class+classes.dex 双存在（单 dex 桌面 ClassNotFound）；②mod.hjson 须 `java:true`；③d8 勿加 --classpath 含 Mindustry jar
- 产物：build/libs/teleport-unlock.jar (MD5 01de08d4...)


## 通用技术笔记
- `buildSelectRecipeMenu`: `requestKeyboard()` 在 clicked 不可靠；推荐 FocusListener
- `BlockInventoryFragment.takeItem`: canWithdraw → 计算数量 → Call.requestItem → 重置 → WithdrawEvent
- arc 无泛型 Pair；替代：IntIntMap/IntPoint/Structs pairs/自定义内联类
- `ConsumePowerDynamic` + `updateConsumption()` override 兼容
- Mindustry 像素画：精灵图可行（分割/缩放/着色）；Mindustry 风格 API 难 — 建议后处理
- IT 项目：单模块、Groovy DSL、Kotlin 2.2.0；规划 KSP 处理器模块 `@IEContent` → 生成 object IEBlocks


## FluidShader 测试模组 (2026-08-14)
- `fluidFrames` = 构建期用 Generators.java:32-72 的 Simplex.rawTiled 逐个像素生成 50 帧 fluid-liquid-N/gas-N;运行时 Renderer.java:132-140 加载
- headless 验证 OK: noiseliquid meanAbs=0.1385 max=0.16; noisegas meanAbs=0.2262; headless shaderCreated=false 防 NPE
- **arc Shader 无 fetch() 返回 Uniform**,用 setUniformf/setUniformi(String,int,float);顶点 a_texCoord0+u_projTrans
- **jar mod 读 shader 必须逐段 child()**: root.child("shaders").child("fluid.vert"); 单段含/解析不到(ZipFi.child 按直接子名匹配);ZipFi.length() 返回 0,用 readString().length()
- Draw.shader 前必须 Draw.flush();不能 override newEntity()(Building 子类反射发现)
