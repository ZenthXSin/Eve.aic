# Mindustry DrawPart 部件系统源码验证

> Source: `core/src/mindustry/entities/part/` 6个类

## DrawPart 基类

```java
public abstract class DrawPart {
    public boolean turretShading;
    public boolean under = false;
    public int weaponIndex = 0;
    public int recoilIndex = -1;
    
    public abstract void draw(PartParams params);
    public void load(String name) {}
    public void getOutlines(Seq<TextureRegion> out) {}
}
```

## PartParams 字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `warmup` | float | 预热值 |
| `reload` | float | 装填进度 |
| `smoothReload` | float | 平滑装填 |
| `heat` | float | 热量 |
| `recoil` | float | 后坐力 |
| `life` | float | 生命值 |
| `charge` | float | 充能进度 |
| `x` | float | X位置 |
| `y` | float | Y位置 |
| `rotation` | float | 旋转角度 |
| `sideOverride` | int | 侧边覆盖 |
| `sideMultiplier` | int | 侧边倍率 |

## 继承链（6个子类）

```
DrawPart
├── RegionPart      # 区域部件（最常用）
├── ShapePart       # 形状部件
├── FlarePart       # 耀斑部件
├── HaloPart        # 光环部件
├── HoverPart       # 悬浮部件
└── EffectSpawnerPart # 特效生成器
```

## RegionPart 字段（最常用）

### 基础
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `suffix` | String | "" | 区域后缀 |
| `name` | String | null | 区域名称（覆盖suffix） |
| `mirror` | boolean | false | 是否镜像 |
| `outline` | boolean | true | 是否绘制轮廓 |
| `drawRegion` | boolean | true | 是否绘制区域 |
| `under` | boolean | false | 是否在底层 |

### 变换
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `x` | float | 0 | X偏移 |
| `y` | float | 0 | Y偏移 |
| `xScl` | float | 1f | X缩放 |
| `yScl` | float | 1f | Y缩放 |
| `rotation` | float | 0 | 旋转 |
| `moveX` | float | 0 | X移动 |
| `moveY` | float | 0 | Y移动 |
| `moveRot` | float | 0 | 旋转移动 |
| `growX` | float | 0 | X增长 |
| `growY` | float | 0 | Y增长 |

### 进度
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `progress` | PartProgress | PartProgress.warmup | 进度函数 |
| `growProgress` | PartProgress | PartProgress.warmup | 增长进度 |
| `heatProgress` | PartProgress | PartProgress.heat | 热量进度 |
| `clampProgress` | boolean | true | 是否钳制进度 |

### 颜色
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `color` | Color | null | 颜色覆盖 |
| `colorTo` | Color | null | 渐变颜色 |
| `mixColor` | Color | null | 混合颜色 |
| `mixColorTo` | Color | null | 混合渐变 |
| `heatColor` | Color | Pal.turretHeat | 热量颜色 |

### 层级
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `layer` | float | -1 | 渲染层 |
| `layerOffset` | float | 0 | 层偏移 |
| `heatLayerOffset` | float | 1 | 热量层偏移 |
| `outlineLayerOffset` | float | -0.001 | 轮廓层偏移 |

### 子部件
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `children` | Seq<DrawPart> | [] | 子部件列表 |
| `moves` | Seq<PartMove> | [] | 移动序列 |

## ShapePart 字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `circle` | boolean | false | 是否圆形 |
| `hollow` | boolean | false | 是否空心 |
| `stroke` | float | 1.5f | 描边宽度 |
| `radius` | float | 3f | 半径 |
| `triLength` | float | 3f | 三角长度 |
| `tri` | boolean | false | 是否三角 |

## FlarePart 字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `radius` | float | 10f | 半径 |
| `radiusTo` | float | 15f | 目标半径 |
| `rotateSpeed` | float | 1f | 旋转速度 |
| `stroke` | float | 1.5f | 描边宽度 |

## HaloPart 字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `radius` | float | 10f | 半径 |
| `triLength` | float | 4f | 三角长度 |
| `tri` | boolean | false | 是否三角 |
| `stroke` | float | 1.5f | 描边宽度 |

## HoverPart 字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `radius` | float | 10f | 半径 |
| `phase` | boolean | false | 是否相位 |
| `scl` | float | 3f | 缩放 |

## JSON 中的 DrawPart

### 区域部件
```json
"parts": [
    {
        "type": "RegionPart",
        "suffix": "-barrel",
        "mirror": true,
        "under": true,
        "moveX": 0,
        "moveY": -2,
        "progress": "recoil"
    }
]
```

### 形状部件
```json
"parts": [
    {
        "type": "ShapePart",
        "circle": true,
        "radius": 5,
        "color": "ff0000"
    }
]
```

### 耀斑部件
```json
"parts": [
    {
        "type": "FlarePart",
        "radius": 10,
        "radiusTo": 15,
        "rotateSpeed": 1
    }
]
```

## PartProgress 类型

| 类型 | 说明 |
|------|------|
| `warmup` | 预热值 |
| `reload` | 装填进度 |
| `smoothReload` | 平滑装填 |
| `heat` | 热量 |
| `recoil` | 后坐力 |
| `life` | 生命值 |
| `charge` | 充能进度 |
| `constant` | 常量 |

## 硬性规则

1. **RegionPart.mirror=true 时自动创建镜像副本**
2. **RegionPart.under=true 时绘制在底层**
3. **PartProgress 控制动画进度**
4. **children 是子部件，不是子渲染器**
5. **moves 是移动序列，不是动画**
6. **outline 默认 true，自动绘制轮廓**
7. **drawRegion 默认 true，绘制区域**
8. **clampProgress 默认 true，钳制进度到 0-1**
9. **layer -1 使用默认层级**
10. **heatColor 默认 Pal.turretHeat**

## 常见误区

- ❌ `RegionPart.mirror` 手动创建两个部件 → ✅ 自动创建镜像
- ❌ `RegionPart.under` 是绘制在顶层 → ✅ 是绘制在底层
- ❌ `PartProgress` 是固定值 → ✅ 是动态函数
- ❌ `children` 是子渲染器 → ✅ 是子部件
- ❌ `moves` 是动画 → ✅ 是移动序列
- ❌ `outline` 默认 false → ✅ 默认 true
- ❌ `drawRegion` 默认 false → ✅ 默认 true

## 关联

- [Turret System](mindustry-turret-system.md)
- [Unit System](mindustry-unit-system.md)
- [DrawBlock System](mindustry-drawblock-system.md)
