# Mindustry JSON 屏幕抖动

> 记录 Mindustry JSON 中触发屏幕抖动的常见写法。

## 写法
在方块、武器或效果相关 JSON 中，屏幕抖动通常通过 `shake` 或带抖动参数的效果字段表达；具体字段名取决于 JSON 所属对象类型。

### 常见思路
- 如果是 **Effect** 类配置：使用效果本身的抖动参数，或在触发时调用带 `shake` 的效果。
- 如果是 **Block / Building**：通常不是单独一个通用 `shake` 顶层字段，而是挂在某个 effect / draw / weapon / bullet 配置里。
- 如果是 **爆炸/命中效果**：优先查看该 type 支持的字段，常见会有 `shake`、`shakeIntensity`、`shakeDuration` 之类的命名。

## 建议
- 先确认你要加抖动的是 **方块**、**子弹**、**武器** 还是 **效果**。
- JSON 里不要盲写不存在的字段；Mindustry 解析通常只认该 type 支持的字段。
- 如果你要的是“屏幕震一下”的视觉效果，通常要找的是对应 effect 的 `shake` 参数，而不是方块通用字段。

## 相关页面
- [Mindustry JSON 技能约定](mindustry-json-skill-rules.md)
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md)
