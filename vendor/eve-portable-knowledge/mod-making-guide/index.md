# Mod 制作指南

> 基于 GitHub Stars Top 15 Mindustry Mod 的浅层分析
> 结合 LMM (Learn Mindustry Mod) 教程体系逐层深入
> 记录时间: 2026-05-18
> 状态: 浅层记录（待深层解析）

## 分析样本

基于 GitHub 搜索 `topic:mindustry` + `topic:mindustry-mod` 按 stars 排序的前 15 个有影响力的 mod
（不含官方自动列表、已归档且活跃度低的项目）

### 本地已有
- [NewHorizonMod](../entities/newhorizon.md) — 575★
- [BetaMindy](../entities/betamindy.md) — 359★
- [Extra-Utilities](../entities/extra-utilities.md) — 250★

### 本次新拉取（2026-05-18）
- [ExoGenesis](./exogenesis-analysis.md) — 440★
- [Factoriodustry](./factoriodustry-analysis.md) — 254★
- [Asthosus](./asthosus-analysis.md) — 232★
- [Scheme-Size](./scheme-size-analysis.md) — 230★
- [Allure](./allure-analysis.md) — 227★
- [MindustryToolMod](./mindustry-tool-mod-analysis.md) — 215★
- [Commandblocks](./commandblocks-analysis.md) — 156★
- [Sapphirium](./sapphirium-analysis.md) — 133★
- [Fading-Revelations](./fading-revelations-analysis.md) — 125★
- [Heavy-Armaments-Industries](./hai-analysis.md) — 104★

## Star 排名与流行因素总结

| 排名 | Mod | ★ | 定位 | 语言 |
|------|-----|---|------|------|
| 1 | NewHorizonMod | 575 | 科幻主题全栈扩展 | Java |
| 2 | ExoGenesis | 440 | 5种族全战役内容包 | Java/JSON |
| 3 | BetaMindy | 359 | 实验性混乱趣味 | Java |
| 4 | Factoriodustry | 254 | 像素风重制贴图包 | Aseprite/JS |
| 5 | Extra-Utilities | 250 | 综合扩展（炮塔/DrawBlock/子弹） | Java |
| 6 | Asthosus | 232 | 独立行星战役 | Java/JSON |
| 7 | Scheme-Size | 230 | 沙盒工具类 | Java |
| 8 | Allure | 227 | HJSON RTS 风格 mod | HJSON |
| 9 | MindustryToolMod | 215 | 社区内容浏览器+自动化 | Java |
| 10 | Commandblocks | 156 | 命令方块+逻辑扩展 | JavaScript |

## 学习路径 — LMM 教程 + 示例 Mod 对照

LMM（Learn Mindustry Mod）是目前最系统的 Mindustry mod 开发教程，按章节递进。
每个章节都可以在 Top mods 中找到对应的源码中找到实际案例。以下是路径映射：

### 第一章：环境搭建 → 看最小实现
**LMM**：Gradle 环境、Anuke 模板、文件结构、Android 构建

**示例 Mod**：
- **Scheme-Size**（★230）— 极简 Java mod，`build.gradle` + `mod.json` + 1 个源文件，适合作为模板起点
- **Heavy-Armaments-Industries**（★104）— JS mod 的最小实战结构

<details>
<summary>详细对照</summary>

| LMM 小节 | 示例 Mod 对应源码 |
|-----------|-----------------|
| 1.1 Gradle Java/Kotlin | Scheme-Size 的 `build.gradle` |
| 1.2 Anuke 模板 | 所有 Java mod 的根目录结构 |
| 1.3 文件结构 | 对比 Extra-Utilities 的 `src/` 和 Sapphirium 的根目录 |
| 1.4 Android 构建 | Mod-Tools 的 Android 兼容层 |
| 1.5 JS/TS 环境 | Commandblocks 的纯 JS 结构 |
</details>

### 第二章：创建内容 → 看 Top mod 的 content 目录
**LMM**：物品/流体 → 工厂 → 钻头/泵 → 电力 → 状态效果 → 科技树 → 炮塔/子弹 → 单位 → 行星/区块

**示例 Mod**：
- **Extra-Utilities**（★250）— 14 个 content 类，覆盖 9 种方块类型 + 25 种子弹，是"内容组织"的最佳参考
- **NewHorizonMod**（★575）— 22 阶段严格加载管线，大型 mod 的内容管理范本

<details>
<summary>详细对照</summary>

| LMM 小节 | 示例 Mod 对应源码 |
|-----------|-----------------|
| 2.1 物品/流体 | Extra-Utilities 的 `EUItems.java`、NewHorizon 的 `NHItems.java` |
| 2.2 GenericCrafter | Extra-Utilities 的 `production/` 包（15+ 种工厂变体）|
| 2.3 钻头泵 | Extra-Utilities 的 6 种自定义钻头 + Asthosus 的混合采集 |
| 2.4 电力方块 | BetaMindy 的 BlackholeGenerator + ArtificialSun |
| 2.5 状态效果 | BetaMindy 的 16 种自定义子弹效果 |
| 2.6 科技树 | NewHorizon 的 `NHTechTree.load()`（22 步中第 16 步）|
| 2.7 炮塔/子弹 | Extra-Utilities 的 20+ 炮塔 + 25+ 子弹（最全参考）|
| 2.8 单位 | Sapphirium 的 63 种单位，NewHorizon 的 5K 行 `NHUnitTypes.java` |
| 2.9 行星/区块 | Asthosus 的完整沙漠星球战役 |
</details>

### 第三章：程序逻辑 → 看实装 Building
**LMM**：Block/Building 生命周期 → Crafter 逻辑 → UnitType → 物流/热量/电力 → Payload → BulletType → 事件 → 星球生成器

**示例 Mod**：
- **NewHorizonMod**（★575）— 自定义 `NHBuildEntity` + 7 个能力实现
- **BetaMindy**（★359）— `payduction/` 完整载荷生产链
- **ProjectUnity**（本地）— 图系统 + 地面流体模拟

<details>
<summary>详细对照</summary>

| LMM 小节 | 示例 Mod 对应源码 |
|-----------|-----------------|
| 3.1 Block/Building | VoidShield 的 `HeatCatheter.kt` — 自定义 Building 最简模版 |
| 3.2 Crafter 逻辑 | BetaMindy 的 `payduction/` 包（CRecipe 配方系统）|
| 3.3 UnitType/Unit | Extra-Utilities 的 Boss 单位系统 |
| 3.4 物流/热量/电力 | ProjectUnity 的 HeatGraph + VoidShield 的 HeatCatheter |
| 5 Payload | MultiCrafterLib 的 PayloadBlock 继承实现 |
| 6 BulletType | Extra-Utilities 的 25+ 子弹最全库 |
| 7 事件 | NewHorizonMod 的过场逻辑系统 |
| 8 星球生成器 | Asthosus 的自定义星球 + BetaMindy 的 SharMoonGenerator |
</details>

### 第四章：调试 → 看专业工具
**LMM**：日志、断点、模拟器、VisualVM

**示例 Mod**：
- **Mod-Tools**（本地，406 源文件）— 专业调试 mod：HotSwap 热替换、火焰图采样器、反射 JS 接口
- **Helium**（本地，★—）— 实体信息显示、范围指示器、屏幕采样

### 第五章：绘图与动画 → 看 DrawBlock 武器库
**LMM**：绘制基础、动画、FX 特效、粒子系统

**示例 Mod**：
- **Extra-Utilities**（★250）— **25 种自定义 DrawBlock**，Mindustry 社区最多的 drawer 集合
- **NewHorizonMod**（★575）— 17 种 DrawBlock，包含 RegionPart 动态渲染

<details>
<summary>详细对照</summary>

| DrawBlock | Extra-Utilities 实现 | NewHorizon 实现 |
|-----------|---------------------|-----------------|
| 基础 region | DrawRegion | DrawBaseRegion |
| 旋转 | DrawRegionRotated | DrawRegionRotated |
| 液体覆盖 | — | DrawLiquidRegionRotated |
| 火焰/粒子 | — | DrawFlameRotated, DrawParticleFlow |
| 电弧 | DrawElectric | — |
| 彩虹 | DrawRainbow | — |
| 拖尾 | DrawTrail | — |
| 弓形 | DrawBow + PartBow | — |
| 瞄准 | AimPart | — |
</details>

### 第六章：UI → 看 Fragment 扩展
**LMM**：GUI 基础、Fragment、Dialog、Table 布局

**示例 Mod**：
- **BetaMindy**（★359）— 10 个 UI 组件，Shop/DailyStore/TradingPost
- **MindustryToolMod**（★215）— 社区内容浏览器、健康条 HUD、全局聊天覆盖层
- **Helium** — 重写 Mods Manager/Browser、增强放置面板

### 第七章：程序架构 → 看大型 mod 设计
**LMM**：源码结构 → 编译时 ECS → Flyweight → Game Start → World/Tile → Vars → IO/Sync/Network → Mod Load → Box2D → Logic Processor → Input → Reflection

**示例 Mod**：
- **NewHorizonMod**（★575）— 22 阶段加载管线，最严谨的内容生命周期管理
- **ProjectUnity**（本地，576 文件）— 注解处理器 + JavaPoet 代码生成，`@EntityDef` 编译时 ECS
- **Mod-Tools**（本地，406 文件）— JDWP HotSwap + ASM 字节码

<details>
<summary>详细对照</summary>

| LMM 小节 | 示例 Mod 对应源码 |
|-----------|-----------------|
| 7.A 源码结构 | NewHorizon 的 22 阶段加载 |
| 7.B 编译时 ECS | ProjectUnity 的注解处理器 |
| 7.C Flyweight | BetaMindy 的实体注册 |
| 7.F Vars | VoidShield 的 VsVars 设计 |
| 7.G IO/Sync/Network | Extra-Utilities 的 8 个网络同步包 |
| 7.H Box2D | — |
| 7.I Logic Processor | VoidShield 的 LTeleport + NewHorizon 的 NHLogic |
| 7.J Input | Helium 的输入增强 |
| 7.K Reflection | Mod-Tools 的 REFLECT + UNSAFE |
</details>

### 第八章：高级图形效果 → 看着色器
**LMM**：OpenGL 概述 → 渲染管线 → Shader → 纹理/Pixmap → 变换/投影 → 混合 → 渲染流程 → FrameBuffer → Bloom → 3D → 星球渲染

**示例 Mod**：
- **BetaMindy**（★359）— 7 个自定义着色器（bittrium/coffee/dreamjelly/spacefire/starwater 等）
- **VoidShield**（本地）— 完整 shader 管线（ScreenSampler → ShaderManage → VSShaderLoader）
- **Helium**（本地）— 6 个后处理着色器
- **Mod-Tools**（本地）— 8 个着色器 + 毛玻璃效果

<details>
<summary>详细对照</summary>

| LMM 小节 | 示例 Mod 对应源码 |
|-----------|-----------------|
| 8.1 OpenGL 概述 | — |
| 8.2 渲染流程 | Mindustry 渲染系统知识页 |
| 8.3 Shader | BetaMindy 的 `assets/shaders/` |
| 8.4 纹理/Pixmap | — |
| 8.5 变换/投影/相机 | — |
| 8.6 Alpha/Blending | Extra-Utilities 的 DrawRainbow additive |
| 8.7 渲染 Pipeline | VoidShield 的 ScreenSampler |
| 8.8 FrameBuffer | VoidShield 的帧缓冲离屏渲染 |
| 8.9 Bloom | — |
| 8.10-14 3D/Planet | ProjectUnity 的 3D 模型渲染 |
</details>

---

## 玩家为什么喜欢这些 mod？（浅层总结）

### 1. 量大管饱（第1-3名特征）
- NewHorizonMod、ExoGenesis、Asthosus 的共同点：
  - 新增完整行星+战役
  - 独立于原版的科技树
  - 自定义UI、音效、贴图
  - **玩家本质上在玩一个新游戏**

### 2. 视觉新颖（Factoriodustry + BetaMindy）
- Factoriodustry 仅靠贴图重制就拿到 254★
- BetaMindy 有 7 个自定义着色器
- **视觉风格是 mod 的第一印象，决定了玩家愿不愿意点进来看**

### 3. 功能性增强（MindustryToolMod + Scheme-Size）
- 工具类 mod 不增加内容，但解决玩家的痛点
  - Scheme-Size: 突破原版蓝图大小限制
  - MindustryToolMod: 社区图/蓝图浏览器、自动玩家
- **解决痛点比堆内容更吸粉**

### 4. 创造自由（Commandblocks + Allure）
- Commandblocks 给玩家在游戏内写逻辑的能力
- Allure 提供了 RTS 风格的自定义玩法
- **允许玩家自我表达的工具类 mod 有独特粘性**

### 5. 多阵营/种族（ExoGenesis + Sapphirium）
- 多个独立科技线延长了游戏时间
- 每个种族有独立的视觉和玩法风格
- **玩家愿意为每个种族重新开始**

### 6. 长期维护 + 社区参与
- Asthosus 有 6 种语言翻译
- MindustryToolMod 有活跃的 Discord
- Heavy-Armaments-Industries 接入了 Steam Workshop
- **mod 的可持续性决定了 star 增长上限**

## 开发者可复用的关键模式

### 技术层面
| 模式 | 代表性 mod | 建议参考 |
|------|-----------|---------|
| 严格加载管线 | NewHorizonMod | 内容依赖链管理 |
| 自定义 DrawBlock | Extra-Utilities | 25种 drawer 复用 |
| 逻辑系统扩展 | Commandblocks + NewHorizon | 自定义逻辑处理器 |
| 着色器管线 | BetaMindy + VoidShield | 屏幕采样+后处理 |
| 外部服务集成 | MindustryToolMod | HTTP 通信+社区平台 |
| 自定义 UI Fragment | BetaMindy | Fragment 扩展模式 |
| 贴图风格统一 | Factoriodustry | 像素风统一调色板 |
| 战役系统 | Asthosus | 完整自洽的独立战役 |
| 多个种族分支 | ExoGenesis/Sapphirium | 多科技线并行 |

### 成功要素
1. **明确的目标定位** — 一个 mod 不能解决所有问题
2. **清晰的视觉风格** — 贴图/着色器是最直接的吸引点
3. **可扩展的架构** — 为后续内容留空间
4. **社区渠道** — Discord/GitHub Issues 是 star 增长的加速器
5. **工具思维** — 给玩家工具比直接给内容更有价值

## 参考资源

- **LMM 教程**：`knowledge/sources/learn-mindustry-mod-github-io/` — 9 章系统性教程，推荐作为主线学习路径
- **JSON 教程**：[JSON/Hjson Mod 开发指南](./json-mod-guide.md) — 基于 LMM JSON 教程 + 本地示例的全覆盖指南
- **LMM 附录**：方块速查表、子弹字段表、炮塔字段表、单位类型表、事件触发器表 — 开发时的快速参考
- **LMM 绪论**：详细说明了 Java/Kotlin/JS/JSON 四种语言的选择依据和混合使用的注意事项

> **待深层解析**：
> - 每个 mod 的代码架构细节
> - 按 LMM 章节逐个解析对应 mod 实现
> - 性能优化策略
> - 资源管理（贴图、音效、着色器）