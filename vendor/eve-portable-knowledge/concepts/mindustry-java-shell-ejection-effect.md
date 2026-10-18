# Mindustry Java 抛壳特效

> Source: conversation

在 Mindustry 的 Java/Kotlin Mod 中，抛壳特效通常用 `Effect` 实现：在武器开火时生成一个短生命周期的视觉对象，通过位移、旋转和渐隐模拟弹壳飞出。

## 核心思路

- 用 `Effect` 表示一次抛壳动画
- 在回调里根据 `e.fin()` 计算位置、缩放和透明度
- 使用 `Angles.trnsx/trnsy` 或 `Vec2` 为壳体提供初速度方向
- 用 `Fill.rect`、`Draw.rect` 或贴图绘制壳体
- 通过 `e.rotation` 表示枪口朝向，必要时加少量随机角度

## 典型写法

```java
public static final Effect shellFx = new Effect(20f, e -> {
    float f = e.fin();
    float ox = Angles.trnsx(e.rotation, f * 4f);
    float oy = Angles.trnsy(e.rotation, f * 4f);

    Draw.color(Color.lightGray, Color.gray, f);
    Fill.rect(e.x + ox, e.y + oy, 4f, 2f, e.rotation + f * 360f);
});
```

## 作为武器效果使用

可以把它挂到武器的 `shootEffect`、`smokeEffect` 或自定义开火逻辑里：

```java
weapon.shootEffect = shellFx;
```

如果需要更像“抛出去”的感觉，可以把壳体拆成两段：

- 前半段做加速位移
- 后半段做旋转和下坠

## 进阶做法

- 用贴图代替纯几何图形，视觉更像真实壳体
- 增加随机偏移，让每次抛壳略有差异
- 结合重力和旋转速度，做成落地前的抛物线感
- 如果要更真实，可以把壳体做成临时实体，而不是单纯 `Effect`

## 相关

- [Mindustry 抛壳特效](mindustry-shell-ejection-effect.md)
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md)
- [Mindustry JSON 使用 DrawCrucibleFlame](mindustry-json-drawcrucibleflame.md)
