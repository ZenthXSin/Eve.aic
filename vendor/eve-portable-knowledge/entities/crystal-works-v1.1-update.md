# crystal-works v1.1.0 Update Record

> 时间: 2026-05-18
> 作者: Eve (AIC-EVE-001)
> 版本: v1.0.0 → v1.1.0

## 更新概述

**晶工 (Crystal Works)** —— 晶能科技模组，新增晶矿及其加工联动与防御/炮塔系统。

## 平衡性修订（v1.1.0 重点）

| 内容 | v1.0 → v1.1 变化 | 原因 |
|------|------------------|------|
| **crystal** | hardness 3→4 | tier 4 钻机无法采集，需 tier 5 |
| **crystal-drill** | tier 4→5, drillTime 320→200, health 360→540 | 对比 industrial-drill (tier 6, 200)，原参数过弱 |
| **crystal-drill** | 新增 hardnessDrillMultiplier: 8, booster:true | BurstDrill 模式，液体增压 |
| **crystal-smelter** | recipe 3:2→4:6, craftTime 75→100 | 原配方太便宜，新比例 2:3 镜像 plastanium/poly |
| **crystal-alloy** | hardness 0→1 | hardness 0 无钻机可采，设为 1 |
| **crystal-fluid** | heatCapacity 1.8→1.5, viscosity 0.15→0.3 | 1.8 太接近 cryofluid(1.9)，定位中阶冷却液 |

## 新增内容

### 生产类
- **crystal-fluid-mixer** — GenericCrafter, size 2, crystal+water→crystal-fluid
- **crystal-borer** — BurstDrill, size 4, tier 6, 高产能钻机

### 炮塔类
- **crystal-turret** — ItemTurret, size 3, 支持 crystal/crystal-alloy 弹药
  - crystal 弹: damage 40, splash 20, 附加 crystal-shock
  - crystal-alloy 弹: damage 70, pierce 0.3, splash 35, 附加闪电链特效

### 防御类
- **crystal-wall** — Wall, size 1, health 120
- **crystal-wall-large** — LargeWall, size 2, health 480

### 状态效果
- **crystal-shock** — 移动降低 0.6, 防御降低 0.7, duration 60-90 ticks

## 科技树路线

```
plastanium → crystal → crystal-alloy → crystal-drill → crystal-smelter
                                              ↓
                            crystal-fluid-mixer → crystal-borer
                                              ↓
                            crystal-turret ← crystal-wall
```

## 文件结构

```
crystal-works/
├── mod.json                    # v1.1.0
├── bundles/
│   ├── bundle.properties       # en
│   └── bundle_zh_CN.properties # zh (新增状态/炮塔翻译)
├── content/
│   ├── items/
│   │   ├── crystal.hjson       # hardness 4
│   │   └── crystal-alloy.hjson # hardness 1
│   ├── liquids/
│   │   └── crystal-fluid.hjson # heatCap 1.5
│   ├── blocks/
│   │   ├── production/
│   │   │   ├── crystal-drill.hjson  # 平衡修订
│   │   │   ├── crystal-smelter.hjson # 平衡修订
│   │   │   ├── crystal-fluid-mixer.hjson
│   │   │   └── crystal-borer.hjson   # BurstDrill
│   │   ├── turrets/
│   │   │   └── crystal-turret.hjson
│   │   └── defense/
│   │   │   ├── crystal-wall.hjson
│   │   │   └── crystal-wall-large.hjson
│   └── status/
│   │   └── crystal-shock.hjson
└── sprites/    # 占位贴图
```

## v1.2 规划

- crystal-battery: 储能方块
- crystal-conduit: 电力/液体传输
- crystal-armor-unit: 装甲单位

## 参考

- [Allure mod 平衡模式](../../knowledge/mod-making-guide/allure-analysis.md)
- [LMM JSON 教程](../../learn-mindustry-mod.github.io/javascript-tutorial-overview.md)
- [crystal-works GitHub](https://github.com/ZenthXSin/crystal-works)