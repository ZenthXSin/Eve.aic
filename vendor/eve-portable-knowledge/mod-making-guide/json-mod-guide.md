# JSON/Hjson Mod 开发指南

> 基于 LMM JSON 教程 + 本地示例 mod（Allure / 饱和火力）
> 记录时间: 2026-05-18
> 状态: 初版（已覆盖基础到进阶）

## 概述

JSON（严格来说是 Hjson）是 Mindustry mod 开发的**最低门槛入口**。
优势：无需编译工具链，仅需文本编辑器 + 压缩包，手机也能写。
限制：只能使用原版已有的机制，无法自定义程序逻辑。

## 三种开发语言的选择

| 语言 | 门槛 | 上限 | 推荐人群 |
|------|------|------|---------|
| **JSON/Hjson** | ★ | ★★ | 零编程经验、快速原型、小规模 mod |
| JavaScript | ★★ | ★★★ | 无电脑、需要简单逻辑、JS 基础 |
| Java/Kotlin | ★★★ | ★★★★★ | 有编程经验、追求自定义机制、大型 mod |

> **不推荐混合使用**：加载顺序为 Java > JavaScript > JSON，靠前的语言无法引用靠后语言定义的内容。

## 基础结构

### 目录布局

```
my-mod/
├── mod.json              # 必需 — 模组身份证
├── content/              # 必需 — 所有 JSON 内容
│   ├── blocks/           # 方块（工厂、炮塔、钻头、墙壁...）
│   │   ├── production/   # 生产类（推荐按类别分子目录）
│   │   ├── turrets/      # 炮塔类
│   │   └── environment/  # 环境方块（地板、矿物、装饰墙）
│   ├── items/            # 物品
│   ├── liquids/          # 流体
│   ├── units/            # 单位
│   ├── status/           # 状态效果
│   ├── planets/          # 星球
│   ├── sectors/          # 战役区块
│   └── weathers/         # 天气（可选）
├── sprites/              # 自定义贴图
│   └── blocks/environment/  # 环境方块贴图必须放这
├── sprites-override/     # 覆盖原版贴图
├── bundles/              # 语言文件
│   └── bundle_zh_CN.properties
├── sounds/               # 音效（.ogg 格式）
└── scripts/              # JS 脚本（进阶时用）
```

### mod.json

```json
{
  "name": "my-first-mod",
  "displayName": "我的第一个模组",
  "author": "你",
  "description": "跟着教程做的模组",
  "version": "1.0",
  "minGameVersion": "136"
}
```

## 内容类型速查（从易到难）

### 物品（Item）

```hjson
{
  type: Item
  color: 7595D2
  explosiveness: 0
  flammability: 0
  radioactivity: 0
  charge: 0.45
  hardness: 3
  cost: 1.75
  research: plastanium  // 科技树前置
}
```

### 流体（Liquid）

```hjson
{
  type: Liquid
  color: 7CF389FF
  temperature: 0.3
  heatCapacity: 1.45
  viscosity: 0.3
}
```

### 工厂（GenericCrafter）— 最通用

```hjson
{
  type: GenericCrafter
  size: 3
  health: 400
  category: crafting
  requirements: [ copper/100, lead/50, graphite/20 ]
  
  craftTime: 90
  outputItem: my-mod-silicon-steel/2
  
  consumes: {
    items: [ silicon/4, titanium/2 ]
    power: 1.5
  }
  
  // 多层绘制器（v7 新增）
  drawer: {
    type: DrawMulti
    drawers: [
      { type: DrawRegion, suffix: -bottom }
      { type: DrawLiquidTile, drawLiquid: slag }
      { type: DrawCrucibleFlame }
      { type: DrawDefault }
      { type: DrawGlowRegion }
    ]
  }
}
```

### 属性增益工厂（AttributeCrafter）

```hjson
{
  type: AttributeCrafter
  attribute: heat
  baseEfficiency: 0.5
  boostScale: 0.4
  maxBoost: 1.5
  // 其余同 GenericCrafter
}
```

### 热量工厂（HeatCrafter）

```hjson
{
  type: HeatCrafter
  heatRequirement: 20
  overheatScale: 0.5
  maxEfficiency: 4
  // 需要配合 HeatProducer 或 HeatSource 方块供热
}
```

### 分离器（Separator）— 随机多产出

```hjson
{
  type: Separator
  size: 2
  craftTime: 30
  results: [ copper/5, lead/3, graphite/2 ]  // 概率比例
  consumes: { items: [ scrap/1 ], power: 0.5 }
}
```

### 钻头（Drill）

```hjson
{
  type: Drill
  size: 2
  tier: 3       // >= 目标矿物的 hardness
  drillTime: 300
  liquidBoostIntensity: 1.5
  consumes: { power: 1.2 }
}
```

### 发电机

**消耗型（ConsumeGenerator）：**
```hjson
{
  type: ConsumeGenerator
  size: 4
  powerProduction: 550
  itemDuration: 19.5
  consumes: { liquid: nano-fluid/0.9, itemRadioactive: {} }
}
```

**热能型（ThermalGenerator）：**
```hjson
{
  type: ThermalGenerator
  size: 3
  powerProduction: 3.6
  attribute: heat
  floating: true
}
```

### 炮塔（ItemTurret / PowerTurret）

```hjson
{
  type: ItemTurret
  size: 3
  health: 1200
  reload: 60
  range: 240
  shootCone: 15
  inaccuracy: 2
  rotateSpeed: 2.5
  shootSound: bang
  
  ammoTypes: {
    copper: {
      type: BasicBulletType, damage: 45, speed: 5, lifetime: 48
    }
    my-mod-silicon-steel: {
      type: BasicBulletType, damage: 120, pierce: true
    }
  }
  
  category: turret
}
```

### 单位（Unit）

```hjson
{
  type: flying
  flying: true
  speed: 3.5
  health: 450
  armor: 2
  hitSize: 12
  
  weapons: [
    {
      name: light-laser
      x: 4, y: 2        // x 非 0 自动生成左右对称
      reload: 30
      rotate: true
      bullet: {
        type: LaserBoltBulletType
        damage: 25
      }
    }
  ]
}
```

## 语言文件（Bundles）

```
# 文件: bundles/bundle_zh_CN.properties
# 格式: [类型].[模组名]-[文件名].[属性] = 值

item.my-first-mod-silicon-steel.name=硅钢
item.my-first-mod-silicon-steel.description=高强度合金材料
block.my-first-mod-heavy-cannon.name=重型火炮
techtree.my-first-mod=演示科技
```

## 科技树

```hjson
// 简写形式（无额外条件）
research: plastanium

// 完整形式
research: {
  parent: plastanium
  objectives: [
    { type: SectorComplete, preset: groundZero }
  ]
}
```

进游戏看：科技树根节点会自动解析，不需要手动声明整棵树。每个内容挂到 parent 下即可。

## Drawer 系统（v7 关键机制）

所有方块（工厂、炮塔、发电机）都可以用 `drawer` 自定义绘制层。

**贴图命名规则：**
- 主体贴图：`blockname.png`
- 后缀贴图：`blockname-suffix.png`
- 发光层：`blockname-glow.png`
- 底部：`blockname-bottom.png`
- 液体层：`blockname-liquid.png`

**常用 Drawer 速查：**

| Drawer | 作用 | 需要贴图 |
|--------|------|---------|
| DrawDefault | 默认主体贴图 | 无后缀 |
| DrawRegion | 指定后缀的静态贴图 | `-suffix` |
| DrawGlowRegion | 随 warmup 变化亮度的发光层 | `-glow` |
| DrawLiquidTile | 纯色液体层 | 无（用颜色填充色） |
| DrawLiquidRegion | 使用液体贴图的层 | `-liquid` |
| DrawFlame | 周期性跳动的火光 | `-top` |
| DrawCrucibleFlame | 向内缩放的粒子火焰 | 无 |
| DrawPistons | 周期收缩的活塞 | `-piston` |
| DrawWeave | 旋转的梭子 | `-weave` |
| DrawMulti | 组合多个 drawer | 取决于子项 |
| DrawTurret | 炮塔绘制的基座 | 取决于 DrawPart |

**warmup（预热值）**：0→1 代表从停工到满负荷，`DrawGlowRegion`、`DrawFlame` 等依赖它。

**律动（Pulse）**：底层用正弦波驱动。`Mag`=振幅（变化大小），`Scl`=频率（变化快慢）。

## 子弹类型

| 类型 | 特点 | 例子 |
|------|------|------|
| BasicBulletType | 基础实体弹 | 双管的铜弹 |
| FlakBulletType | 近炸引信 | 分裂高射炮 |
| MissileBulletType | 导弹+尾迹 | 蜂群 |
| ArtilleryBulletType | 抛物线炮弹 | 冰雹 |
| LaserBulletType | 激光束 | 蓝瑟 |
| ContinuousLaserBulletType | 持续激光 | 熔毁 |
| RailBulletType | 瞬间命中 | 厄兆 |
| LightningBulletType | 闪电链 | 电弧 |
| LiquidBulletType | 液体弹 | 波浪 |
| ShrapnelBulletType | 散射尖刺 | 雷光 |
| SapBulletType | 抑制效果（紫色吸血流） | 血蛭 |
| PointBulletType | 点防御激光 | 裂解 |

## 状态效果

```hjson
{
  color: a2a2a2
  armorMultiplier: 0.5
  speedMultiplier: 0.8
  damage: 2.0
  effectChance: 0.05
  effect: formSmoke
  permanent: false
}
```

## 特效（内联定义）

```hjson
hitEffect: {
  type: MultiEffect
  effects: [
    {
      type: ParticleEffect
      particles: 12
      sizeFrom: 8
      sizeTo: 0
      lifetime: 40
      colorFrom: E9665B
      colorTo: 737373
    }
    {
      type: WaveEffect
      sizeFrom: 0
      sizeTo: 40
      lifetime: 15
      colorFrom: ffffff
      colorTo: E9665B
    }
  ]
}
```

## JSON mod 的极限

**能做：**
- 任意种类、数量、数值的内容（方块、单位、效果、星球）
- v7 多层绘制器组合（DrawMulti + 各种 drawer）
- 自定义子弹效果 + 内联特效
- 完整的科技树 + 战役系统
- Hjson 注释、语法糖、字段省略

**不能做（需要 JS/Java）：**
- 自定义 Building 逻辑（自定义 update()、config()）
- 自定义程序生成的地形（PlanetGenerator）
- 自定义绘制模式（超出原版 drawer 范围）
- 网络同步自定义数据
- 系统级覆盖（如蓝图大小限制、UI 扩展）

**什么时候该转 JS/Java：**
- 需要自定义 `updateTile()` 逻辑
- 需要自定义 Behavior/AI
- 需要绘制原版 drawer 无法实现的视觉效果
- 需要跨模组兼容或网络同步

## 示例 Mod 参考

### Allure（★227）— 纯 HJSON mod 标杆
- 位于 `源码和示例/mods/Allure/`
- 完整使用 Hjson 定义所有内容：物品、流体、方块、单位、星球
- 证明了纯 Hjson mod 可达的复杂度上限
- 被 LMM 官方 wiki 引用为主要示例

### 饱和火力 3.3.0（★—）— JSON + DEX 混合
- 475 个 JSON 文件，覆盖所有内容类型
- 带 Java DEX 编译（`classes.dex`），过渡到 Java 力量
- 展示了 JSON 在大型模组中的组织模式

## 常见陷阱

1. **标点符号**：必须英文半角。中文冒号、引号、逗号是报错第一原因。
2. **贴图路径**：环境方块贴图在 `sprites/blocks/environment/`，其余在 `sprites/`。
3. **文件前缀**：同一模组内引用自己的内容，可用内部名，引用原版用原版名字。
4. **ticks vs 秒**：`craftTime`、`reload`、`lifetime` 等单位都是 tick（1 tick = 1/60 秒）。
5. **电力消耗也是 tick 单位**：`power: 1.5` 表示每 tick 1.5，每秒 90。
6. **压缩包不要多套一层文件夹**：直接选 mod.json 和文件夹压缩，第一层要是 mod.json。
7. **`.properties` 文件编码**：必须是 UTF-8 without BOM，记事本会加 BOM 头。

> **相关知识页**：[Mindustry JSON 可用接口概览](../concepts/mindustry-json-interfaces.md)、[JSON 常见问题](../concepts/json-common-issues.md)、[JSON 问题解决流程](../concepts/json-problem-solving-flow.md)