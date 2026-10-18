# Mindustry 电弧特效

在 Mindustry 里，电弧特效通常分两种：
1. 效果粒子：比如闪电、弧光、命中特效
2. 绘制特效：比如在 `draw()` 里自己画一条抖动电弧

如果你想要“真正的电弧闪电”，最常见做法是直接用 `Fx.lightning`，或者自己创建 `Effect`。

```kotlin
val arcFx = Effect(30f) { e ->
    val x = e.x
    val y = e.y
    Draw.color(Color.cyan)
    Lines.stroke(2f)
    Lines.lineAngle(x, y, e.rotation, 24f)
}
```

如果要做“带分叉感”的电弧，通常不是一条直线，而是用多个随机折点：

```kotlin
val points = Seq<Vec2>()
repeat(6) { i ->
    points.add(Vec2(startX + i * step, startY + Mathf.range(6f)))
}
Lines.beginLine()
points.each { p -> Lines.linePoint(p.x, p.y) }
Lines.endLine()
```

如果你要的是“单位攻击时的电弧”，常用在：
- `shootEffect`
- `hitEffect`
- `update()` 里定时触发 `Effect.at()`
- `draw()` 里按温度/充能值画弧

一句话总结：
想要简单电弧，优先用 `Effect` + `Lines`；
想要更像闪电，就加随机偏移、分叉、短寿命和发光颜色。
