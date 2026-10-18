# Mindustry Weather JSON 示例

> 依据 `ParticleWeather` 源码与饱和火力 mod 的天气 JSON 例子整理。

## 适用类型

天气 JSON 常用 `type`：`ParticleWeather`。

## 最小示例

```json
{
  "name": "示例天气",
  "type": "ParticleWeather",
  "particleRegion": "circle-shadow",
  "color": "a0b46e",
  "useWindVector": false,
  "minAlpha": 0.8,
  "maxAlpha": 0.15,
  "sizeMin": 3,
  "sizeMax": 10,
  "density": 3000,
  "opacityMultiplier": 0.4,
  "baseSpeed": 2.1,
  "yspeed": -0.8,
  "xspeed": -0.02,
  "force": 0,
  "duration": 7,
  "statusDuration": 10,
  "status": "corroded"
}
```

## 常见字段含义

- `name`：天气名称
- `type`：天气类型，常见是 `ParticleWeather`
- `particleRegion`：粒子贴图区域名
- `color`：粒子颜色
- `useWindVector`：是否使用天气风向向量
- `minAlpha` / `maxAlpha`：粒子透明度范围
- `sizeMin` / `sizeMax`：粒子尺寸范围
- `density`：粒子密度
- `opacityMultiplier`：整体透明度倍率
- `baseSpeed`：基础速度
- `yspeed` / `xspeed`：非风向模式下的移动速度
- `force`：对单位施加的推力
- `duration`：持续时间
- `statusDuration`：状态持续时间
- `status`：附加状态效果

## 源码对应关系

`ParticleWeather` 源码里可直接看到这些字段：

- `particleRegion`
- `color`
- `yspeed`
- `xspeed`
- `sizeMin`
- `sizeMax`
- `density`
- `minAlpha`
- `maxAlpha`
- `force`
- `baseSpeed`
- `useWindVector`

加载时会通过 `Core.atlas.find(particleRegion)` 查找粒子贴图。

## 注意点

1. `particleRegion` 必须能在 atlas 里找到对应贴图名。
2. 颜色一般写十六进制字符串，不带 `#`。
3. `useWindVector` 设为 `false` 时，天气按 `xspeed/yspeed` 走。
4. 想让单位被风吹动，要把 `force` 设成大于 0 的值。
5. `status` 必须是游戏里存在的状态效果名。

## 例子来源

饱和火力 mod 中的天气 JSON：
- `weathers/腐蚀风.json`

## 相关源码

- `core/src/mindustry/type/weather/ParticleWeather.java`

## 相关页面

- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md)
- [Mindustry JSON 技能约定](mindustry-json-skill-rules.md)
