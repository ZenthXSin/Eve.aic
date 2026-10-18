# Mindustry 抛壳特效

抛壳特效通常指武器开火时，从炮口或枪口附近抛出弹壳/空壳的小型视觉效果。实现上一般分成两层：

1. **视觉绘制**：在 `Effect` 里画一个小矩形、细长四边形或带旋转的壳体
2. **运动轨迹**：给壳体加上初速度、重力、旋转速度，让它看起来被“抛出”

## 常见实现思路

- 在开火时触发 `Effect.at(x, y, rotation, data)`
- 使用 `Angles.trnsx/trnsy` 或 `Mathf` 算出抛出的初速度方向
- 在 `Effect` 的回调中根据 `e.fin()` 计算位移和缩放
- 用 `Draw` + `Fill` 或 `Lines` 绘制简单壳体形状
- 如果要更精细，可以用贴图精灵而不是几何图形

## 典型用途

- `shootEffect`
- `hitEffect`
- 自定义武器 `shoot` 回调
- `update()` 中按时间间隔触发

## 关键点

- **特效只是视觉**，真正的抛壳通常不需要参与物理碰撞
- 若要做“真实壳体飞出”，建议把壳体做成独立的临时实体或粒子
- 若只是装饰，`Effect` 已经足够

## 与电弧特效的区别

- 电弧特效偏向“线条、闪光、分叉线段”
- 抛壳特效偏向“短小实体、弹出、旋转、落地消失”

## 简化模板

```kotlin
val shellFx = Effect(20f) { e ->
    val f = e.fin()
    val x = e.x + Angles.trnsx(e.rotation, f * 4f)
    val y = e.y + Angles.trnsy(e.rotation, f * 4f)

    Draw.color(Color.lightGray)
    Fill.rect(x, y, 4f, 2f, e.rotation + f * 360f)
}
```

## 适用建议

- 追求性能：优先用 `Effect`
- 追求表现：用贴图 + 旋转 + 位移 + 少量随机偏移
- 追求多样性：结合武器射击方向、弹种和枪口偏移做多个版本

## 参考

- [Mindustry 电弧特效](mindustry-electric-arc-effect.md)
- [Mindustry JSON 使用 DrawCrucibleFlame](mindustry-json-drawcrucibleflame.md)
