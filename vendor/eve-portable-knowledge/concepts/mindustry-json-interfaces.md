# 饱和火力 JSON Type 与接口总览

> Source: 饱和火力 mod 的 JSON 文件 + Mindustry 源码 + `mindustry-api-parser` 查询结果

## 说明
本页整理饱和火力 mod 中 JSON 的 `type` 及其对应接口，统一按下面格式记录：

- **叫什么**：JSON 里填写的 `type`
- **怎么用**：应放在哪个字段、如何声明、常见配置方式
- **什么效果**：在游戏里会产生什么表现

当遇到 JSON 问题时，优先按以下顺序查询：
1. [Mindustry JSON 技能约定](mindustry-json-skill-rules.md)
2. 饱和火力 mod 的 JSON 文件
3. 源码

## 已整理接口

### DrawCrucibleFlame
- **叫什么**：`DrawCrucibleFlame`
- **怎么用**：作为方块 `draw` 里的 `type` 使用，例如：
  ```json
  {
    "draw": {
      "type": "DrawCrucibleFlame"
    }
  }
  ```
- **什么效果**：给方块添加熔炉/火焰风格的中心火焰与粒子环绘制效果
- **可配置字段**：
  - `flameColor`
  - `midColor`
  - `flameRad`
  - `circleSpace`
  - `flameRadiusScl`
  - `flameRadiusMag`
  - `circleStroke`
  - `x`
  - `y`
  - `alpha`
  - `particles`
  - `particleLife`
  - `particleRad`
  - `particleSize`
  - `fadeMargin`
  - `rotateScl`
  - `particleInterp`

## 相关页面
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md)
- [Mindustry JSON 使用 DrawCrucibleFlame](mindustry-json-drawcrucibleflame.md)
- [Mindustry JSON 技能约定](mindustry-json-skill-rules.md)
