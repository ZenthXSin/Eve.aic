# Mindustry 子弹燃烧效果实现

> Source: Mindustry 源码与参考 mod `FlameBulletType`

## 结论
在 Mindustry 里，“子弹燃烧”通常不是给子弹本身加一个独立的“燃烧中”状态，而是通过下面几件事组合实现：

1. **命中后施加燃烧状态**：`status = StatusEffects.burning`
2. **命中时生成火焰/燃烧特效**：`hitEffect`、`shootEffect`、`smokeEffect`
3. **必要时生成地面火焰**：例如在命中逻辑里调用 `Fires.create(...)`
4. **如果是弹体视觉燃烧**：使用火焰色系的绘制粒子、光照和烟雾效果

## 参考实现
参考 mod 中的 `FlameBulletType`：

- 构造里设置：
  - `pierce = true`
  - `status = StatusEffects.burning`
  - `statusDuration = 60f * 4f`
  - `keepVelocity = false`
  - `hittable = false`
- `init()` 中：
  - 用 `shootEffect` 画一团火焰粒子
  - 用 `smokeEffect` 画烟雾粒子
  - 用 `hitEffect` 画命中喷溅线段

## Kotlin / Java 思路
如果你只是想让子弹“打中后着火”，最小写法就是：

```kotlin
val bullet = object : BulletType(6f, 25f) {
    init {
        status = StatusEffects.burning
        statusDuration = 60f * 4f
        hitEffect = Fx.fireHit
        despawnEffect = Fx.none
    }
}
```

如果你还想让它看起来像“火焰弹”，再补：
- `shootEffect`
- `smokeEffect`
- `hitColor`
- `frontColor` / `backColor`
- 自定义 `Effect`

## 注意
- `status = burning` 只代表**命中后给目标上燃烧状态**，不等于子弹外观会自动变火。
- 想让地面也着火，要在命中或触发逻辑里额外调用 `Fires.create(...)`。
- 如果是 JSON 方案，通常需要看具体子弹类型支持哪些字段；复杂燃烧视觉往往更适合 Java/Kotlin 实现。

## 相关页面
- [Mindustry JSON 技能约定](mindustry-json-skill-rules.md)
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md)
- [Mindustry 钍反防炸逻辑](mindustry-thorium-reactor-safety-logic.md)
