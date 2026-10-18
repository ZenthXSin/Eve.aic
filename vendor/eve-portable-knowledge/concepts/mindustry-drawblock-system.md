# Mindustry DrawBlock 渲染系统源码验证

> Source: `core/src/mindustry/world/draw/` 35个类

## DrawBlock 基类

```java
public abstract class DrawBlock {
    public @Nullable String[] iconOverride = null;
    
    public void draw(Building build) {}
    public void drawLight(Building build) {}
    public void drawPlan(Block block, BuildPlan plan, Eachable<BuildPlan> list) {}
    public void load(Block block) {}
    public TextureRegion[] icons(Block block) {}
}
```

## 继承链（35个子类）

```
DrawBlock
├── DrawDefault           # 默认渲染（block.region）
├── DrawMulti             # 组合多个 DrawBlock
│   └── DrawMultiWeave    # 组合+编织效果
├── DrawRegion            # 绘制指定区域
│   └── DrawSideRegion    # 绘制侧面区域
├── DrawFlame             # 火焰效果（flameColor, flameRadius）
│   └── DrawPlasma        # 等离子体效果
├── DrawGlowRegion        # 发光区域（glowColor, glowIntensity）
├── DrawHeatRegion        # 热量覆盖层
├── DrawHeatInput         # 热量输入可视化
├── DrawHeatOutput        # 热量输出可视化
├── DrawLiquidTile        # 液体瓦片渲染
├── DrawLiquidRegion      # 液体区域渲染
├── DrawLiquidOutputs     # 液体输出渲染
├── DrawPumpLiquid        # 泵液体渲染
├── DrawCells             # 细胞效果（cellsColor, cellSize）
├── DrawCircles           # 圆环效果
├── DrawSpikes            # 尖刺效果
├── DrawPistons           # 活塞效果
├── DrawBlurSpin          # 模糊旋转
├── DrawBubbles           # 气泡效果
├── DrawCultivator        # 培养器效果
├── DrawFade              # 渐变效果
├── DrawFrames            # 帧动画
├── DrawParticles         # 粒子效果
├── DrawPower             # 电力可视化
├── DrawPulseShape        # 脉冲形状
├── DrawShape             # 形状渲染
├── DrawSoftParticles     # 软粒子
├── DrawTurret            # 炮塔渲染（默认）
├── DrawWarmupRegion      # 预热区域
├── DrawWeave             # 编织效果
├── DrawArcSmelt          # 电弧冶炼
├── DrawBlockParts        # 方块部件
├── DrawCrucibleFlame     # 坩埚火焰
└── DrawMultiWeave        # 组合编织
```

## JSON 可用的 drawer 完整清单（ClassMap 注册，34 个）

源码：`core/src/mindustry/mod/ClassMap.java:480-514`；基类 `DrawBlock` 本身也注册（可作 type，实际用子类）。

| 名称 | 用途 | 典型使用 |
|------|------|----------|
| `DrawDefault` | 只画本体贴图（默认） | 默认 |
| `DrawRegion` | 画指定后缀贴图 | 大量方块 |
| `DrawSideRegion` | 侧边偏移贴图 | — |
| `DrawFrames` | 多帧序列 | — |
| `DrawMulti` | 组合多个 drawer | 几乎所有原版工厂 |
| `DrawTurret` | 炮塔渲染（base/rotator/heat + DrawPart） | 炮塔 |
| `DrawBlockParts` | DrawPart 零件绘制 | — |
| `DrawFlame` | 火焰 | 熔炉 |
| `DrawPlasma` | 等离子体（DrawFlame 子类） | 冲击反应堆 |
| `DrawArcSmelt` | 电弧熔炼 | 硅电弧炉 |
| `DrawCrucibleFlame` | 坩埚火焰 | 碳化坩埚 |
| `DrawCultivator` | 培养器生长特效 | 培养器 |
| `DrawPistons` | 活塞 | 孢子压缩机、裂解发电机 |
| `DrawSpikes` | 旋转尖刺 | 相合成器 |
| `DrawBubbles` | 气泡 | 电解器 |
| `DrawCells` | 细胞粒子 | 培养器 |
| `DrawCircles` | 圆环扩散 | 涌流坩埚 |
| `DrawParticles` | 粒子 | 大气浓缩机、氰合成器 |
| `DrawSoftParticles` | 软粒子 | 通量反应堆 |
| `DrawPulseShape` | 脉冲形状 | — |
| `DrawShape` | 形状轮廓 | — |
| `DrawWeave` | 编织纹理 | 相位织布机 |
| `DrawMultiWeave` | 组合编织 | — |
| `DrawBlurSpin` | 旋转模糊 | 涡轮冷凝器、排气冷凝器 |
| `DrawFade` | warmup 渐显顶层（`-top`） | 塑钢压缩机 |
| `DrawWarmupRegion` | 预热渐显贴图 | 发电机 |
| `DrawGlowRegion` | 自发光叠加（`-glow`） | 再生投影仪 |
| `DrawLiquidRegion` | 液体区域 | 渣离心机 |
| `DrawLiquidTile` | 液体瓦片 | 冷冻液混合器 |
| `DrawLiquidOutputs` | 液体输出 | — |
| `DrawPumpLiquid` | 泵液体 | 泵 |
| `DrawHeatInput` | 热量输入 | 热传导相关 |
| `DrawHeatOutput` | 热量输出 | 电加热器、渣加热器 |
| `DrawHeatRegion` | 热量发光 | — |
| `DrawPower` | 电量显示 | 电池 |

## 常用 DrawBlock 字段（源码默认值）

### DrawFlame（Source: DrawFlame.java:17-23）
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `flameColor` | Color | "ffc999" | 火焰颜色 |
| `lightRadius` | float | 60f | 光照半径 |
| `lightAlpha` | float | 0.65f | 光照透明度 |
| `lightSinScl/lightSinMag` | float | 10f/5 | 光照正弦波动 |
| `flameRadius` | float | 3f | 火焰半径 |
| `flameRadiusIn` | float | 1.9f | 内火焰半径 |
| `flameRadiusScl/Mag` | float | 5f/2f | 半径缩放/幅度 |
| `flameX/flameY` | float | 0 | 火焰偏移 |
| 注意 | — | — | 需要 `-top` 贴图（load() 硬编码），自动设置 emitLight |

### DrawGlowRegion（Source: DrawGlowRegion.java）
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `blending` | Blending | additive | 混合模式 |
| `suffix` | String | "-glow" | 区域后缀 |
| `alpha` | float | 0.9f | 透明度 |
| `glowScale` | float | 10f | 光晕缩放 |
| `glowIntensity` | float | 0.5f | 发光强度 |
| `rotateSpeed` | float | 0f | 旋转速度 |
| `layer` | float | Layer.blockAdditive | 绘制层 |
| `rotate` | boolean | false | 是否旋转 |
| `color` | Color | red | 发光颜色 |

### DrawHeatRegion（Source: DrawHeatRegion.java）
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `color` | Color | (1,0.22,0.22,0.8) | 热量颜色 |
| `pulse/pulseScl` | float | 0.3f/10f | 脉冲 |
| `layer` | float | Layer.blockAdditive | 绘制层 |
| `suffix` | String | "-glow" | 区域后缀（注意不是 -heat） |

### DrawCells（Source: DrawCells.java）
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `color` | Color | white | 细胞颜色 |
| `particleColorFrom/To` | Color | black | 粒子渐变色 |
| `particles` | int | 12 | 粒子数 |
| `range` | float | 4f | 范围 |
| `recurrence` | float | 2f | 频率 |
| `radius` | float | 1.8f | 半径 |
| `lifetime` | float | 180f | 寿命 |

### DrawMulti（Source: DrawMulti.java）
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `drawers` | DrawBlock[] | {} | 子渲染器数组 |

### DrawRegion（Source: DrawRegion.java）
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `suffix` | String | "" | 区域后缀 |
| `spinSprite` | boolean | false | 自旋精灵 |
| `drawPlan` | boolean | true | 蓝图绘制 |
| `buildingRotate` | boolean | false | 建筑旋转 |
| `rotateSpeed/x/y/rotation` | float | 0 | 旋转速度/偏移/角度 |
| `layer` | float | -1 | 绘制层 |

### DrawTurret（Source: DrawTurret.java）
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `parts` | Seq\<DrawPart\> | {} | 零件 |
| `ammoParts` | ObjectMap | {} | 弹药零件 |
| `basePrefix` | String | "" | 底座前缀 |
| `turretLayer/shadowLayer/heatLayer` | float | Layer.turret 等 | 各层 |

## JSON 三种写法（ContentParser 解析逻辑，Source: ContentParser.java:195-209）

1. **字符串**（按 ClassMap 名实例化）：`"drawer": "DrawRegion"`
2. **数组**（自动变成 DrawMulti）：`"drawer": ["DrawDefault", "DrawRegion"]`
3. **对象**（type 指定类 + 参数）：`"drawer": {"type": "DrawLiquidTile", "alpha": 0.5}`

解析逻辑：`data.isString()` → `make(resolve(...))`；`data.isArray()` → `new DrawMulti(parser.readValue(DrawBlock[].class, data))`；对象 → `resolve(data.getString("type",""), DrawDefault.class)` 后 `readFields`。

## JSON 中的 DrawBlock

### 默认渲染
```json
"drawer": {"type": "DrawDefault"}
```

### 多渲染器组合
```json
"drawer": {
    "type": "DrawMulti",
    "drawers": [
        {"type": "DrawDefault"},
        {"type": "DrawFlame", "flameColor": "ff0000"},
        {"type": "DrawHeatRegion", "color": "ff5500"}
    ]
}
```

### 火焰效果
```json
"drawer": {
    "type": "DrawFlame",
    "flameColor": "ffc999",
    "flameRadius": 3,
    "lightRadius": 60
}
```

### 发光区域
```json
"drawer": {
    "type": "DrawGlowRegion",
    "color": "ff0000",
    "glowIntensity": 0.5,
    "suffix": "-glow"
}
```

## 硬性规则

1. **字段名是 `drawer`**，不是 `draw` 或 `drawBlock`
2. **DrawMulti 是组合器**，不是独立渲染器；JSON 数组直接简写为 DrawMulti
3. **DrawFlame 自动设置 emitLight**，不需要手动设置
4. **DrawFlame 硬编码需要 `-top` 贴图**（`Core.atlas.find(block.name + "-top")`，无 fallback）
5. **DrawGlowRegion/DrawHeatRegion 默认找 `-glow` 后缀贴图**，颜色字段是 `color` 不是 `glowColor`/`heatColor`
6. **DrawCells 需要 `-middle` 贴图**（`Core.atlas.find(block.name + "-middle")`）
7. **DrawDefault 只绘制 block.region**
8. **DrawRegion 可以指定偏移、旋转和自旋**（`x/y/rotateSpeed/spinSprite`）
9. **DrawTurret 是炮塔默认渲染器**（含 base/rotator/heat 三层与 DrawPart）
10. **iconOverride 可以覆盖图标**

## 常见误区

- ❌ `draw` → ✅ `drawer`
- ❌ `DrawFlame` 需要手动设置 `emitLight` → ✅ 自动设置
- ❌ `DrawMulti` 是独立渲染器 → ✅ 是组合器
- ❌ `DrawDefault` 绘制所有区域 → ✅ 只绘制 `block.region`
- ❌ `DrawGlowRegion` 字段是 `glowColor` → ✅ 是 `color`
- ❌ `DrawHeatRegion` 需要 `-heat` 后缀 → ✅ 默认 `-glow` 后缀
- ❌ `DrawHeatRegion` 字段是 `heatColor` → ✅ 是 `color`
- ❌ `DrawCells` 需要 `-cells` 后缀 → ✅ 需要 `-middle` 贴图

## 关联

- [Block Building System](mindustry-block-building-system.md)
- [Turret System](mindustry-turret-system.md)
- [Production Blocks](mindustry-production-blocks.md)
