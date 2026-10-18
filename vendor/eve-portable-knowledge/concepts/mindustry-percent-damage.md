# Mindustry 百分比伤害

> Source: Conversation synthesis; verified against Mindustry source and local mod examples.

## 结论

Mindustry 原版没有一个专门叫 `percentDamage` 的通用字段。百分比伤害通常手写为：

```java
h.damage(h.maxHealth() * percent);
```

或穿甲版本：

```java
h.damagePierce(h.maxHealth() * percent);
```

其中 `percent` 建议用 `0.05f` 表示 5%，而不是 `5f`。

## 源码依据

- `mindustry.entities.comp.HealthComp`：`damage(float amount)` 直接扣 `health -= amount`；`damagePierce` 默认转到 `damage`。
- `mindustry.entities.bullet.BulletType`：有 `maxDamageFraction`，但它是单次伤害上限，不是百分比伤害字段。
- `mindustry.type.StatusEffect`：持续伤害调用 `unit.damageContinuousPierce(damage)`，不是按最大生命百分比。
- 原版多处百分比治疗写法是 `healPercent / 100f * maxHealth()`，可作为百分比换算风格参考。

## 示例 Mod 依据

- BetaMindy `InstantHitBulletType`：`result.damage(result.maxHealth() * percentDamage)`。
- NewHorizon `NHStatusEffects`：`unit.damagePierce(unit.maxHealth() * damagePercent)`。
- ProjectUnity `VoidFractureBulletType`：`h.damage(Math.max(b.damage, h.maxHealth() * ratioDamage))`。
- Extra-Utilities JS：对建筑使用 `build.maxHealth * options.percent`。

## 推荐写法

### 单位/通用 Healthc

```java
if(entity instanceof Healthc h){
    h.damage(h.maxHealth() * percent);
}
```

### 忽略护甲倾向

```java
if(entity instanceof Healthc h){
    h.damagePierce(h.maxHealth() * percent);
}
```

### 状态效果持续百分比伤害

```java
unit.damagePierce(unit.maxHealth() * percentPerSecond * Time.delta);
```

## 注意

- `maxDamageFraction` 不是百分比伤害；它限制子弹单次最终伤害不超过目标最大生命的一定比例。
- 建筑也有 `maxHealth`，但 `Building` 和 `Unit` 的访问形式不同；写通用逻辑时优先用 `Healthc.maxHealth()`。
- 如果想受护甲影响，用 `damage`；如果想更接近真实伤害/无视护甲，用 `damagePierce`。

## Related

- [Mindustry 子弹燃烧效果实现](mindustry-bullet-burning.md) — 子弹命中后执行附加效果的模式
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md) — JSON 字段边界与接口真实性检查
