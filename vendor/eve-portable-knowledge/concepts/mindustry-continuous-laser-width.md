# Mindustry 持续激光伤害宽度实现

> Source: Mindustry 源码与持续激光相关 BulletType 分析

## 核心结论

在 Mindustry 中，**决定持续激光伤害宽度的主要字段是 `width`**，这个字段通常存在于激光类型的 BulletType 中。

## 常见激光类型与宽度字段

### 1. ContinuousLaserBulletType

```java
public class ContinuousLaserBulletType extends BulletType{
    public float width = 12f;
    public float length = 200f;
    public float damage = 10f;
    public float lifetime = 40f;
    
    // 绘制时通常使用这个宽度
    @Override
    public void draw(Bullet b){
        float w = width * Mathf.clamp(b.fin());
        Drawf.laser(b.team.color, b.team.color, b.x, b.y, b.x + Angles.trnsx(b.rotation(), length), b.y + Angles.trnsy(b.rotation(), length), w);
    }
    
    // 命中判定时也基于这个宽度
    @Override
    public void update(Bullet b){
        Damage.collideLine(b, b.team, b.type.hitEffect, b.x, b.y, b.rotation(), length, width);
    }
}
```

### 2. LaserBulletType

```java
public class LaserBulletType extends BulletType{
    public float width = 8f;
    public float length = 160f;
    
    @Override
    public void draw(Bullet b){
        float w = width;
        Drawf.laser(b.team.color, b.team.color, b.x, b.y, b.x + Angles.trnsx(b.rotation(), length), b.y + Angles.trnsy(b.rotation(), length), w);
    }
}
```

## 持续激光伤害宽度来源

### 1. 主要来源：width 字段

```java
bullet.width = 12f;  // 直接决定激光宽度
```

### 2. 次要来源：碰撞判定函数

```java
Damage.collideLine(bullet, bullet.team, effect, bullet.x, bullet.y, bullet.rotation(), length, width);
```

这里的 `width` 参数直接决定了伤害检测的范围。

### 3. 视觉渲染来源

```java
Drawf.laser(team.color, team.color, x, y, endX, endY, width);
```

或者：

```java
Lines.stroke(width);
Lines.line(x, y, endX, endY);
```

## 实际使用示例

### 最小持续激光模板

```java
val continuousLaser = object : ContinuousLaserBulletType(){
    init {
        width = 14f
        length = 180f
        damage = 20f
        lifetime = 60f
        shootEffect = Fx.none
        hitEffect = Fx.laserHit
        despawnEffect = Fx.none
    }
}
```

### 持续激光武器挂载

```java
weapon.bullet = continuousLaser
weapon.shootEffect = Fx.laserShoot
weapon.reload = 30f
weapon.shootLength = 180f
```

## 宽度调整建议

### 1. 视觉宽度调整

```java
bullet.width = 8f;   // 细激光
bullet.width = 16f;  // 中等激光
bullet.width = 24f;  // 粗激光
```

### 2. 命中宽度调整

```java
// 如果伤害判定宽度与视觉宽度不同
// 可以在碰撞检测时传入不同的值
Damage.collideLine(bullet, bullet.team, effect, bullet.x, bullet.y, bullet.rotation(), length, width * 1.2f);
```

## 相关页面

- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md)
- [Mindustry 子弹燃烧效果实现](mindustry-bullet-burning.md)
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md)
- [Mindustry 抛壳特效](mindustry-shell-ejection-effect.md)
