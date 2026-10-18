# Mindustry 星球环绕位置计算

> Source: conversation + `源码和示例/Mindustry/core/src/mindustry/type/Planet.java` + `源码和示例/Mindustry/core/src/mindustry/game/Universe.java`

Mindustry 里星球“环绕位置”的计算核心在 `Planet.addParentOffset(Vec3)`，而实际每帧更新到 `planet.position` 的入口在 `Universe.updatePlanet(Planet)`。

## Key Points

- 轨道角度由 `Planet.getOrbitAngle()` 计算。
- 单层父子轨道偏移由 `Planet.addParentOffset(Vec3)` 计算。
- 绝对世界坐标可用 `Planet.getWorldPosition(Vec3)` 递归/迭代累加所有父级轨道偏移得到。
- 实际运行时更新 `planet.position` 的地方是 `Universe.updatePlanet(Planet)`。
- 原版轨道位置和轨道线渲染都按“圆轨道”实现；若要椭圆轨道，至少要同时改位置计算和轨道线绘制。

## 关键方法

### 1. `Planet.getOrbitAngle()`
根据宇宙时间计算当前公转角：

```java
public float getOrbitAngle(){
    return (orbitOffset + universe.secondsf() / (orbitTime / 360f)) % 360f;
}
```

### 2. `Planet.addParentOffset(Vec3)`
这是“相对父星的环绕位置”真正的坐标计算方法：

```java
public Vec3 addParentOffset(Vec3 in){
    if(parent == null || Mathf.zero(orbitRadius)){
        return in;
    }

    float angle = getOrbitAngle();
    return in.add(Angles.trnsx(angle, orbitRadius), 0, Angles.trnsy(angle, orbitRadius));
}
```

它本质上是：
- 取当前轨道角 `angle`
- 用 `orbitRadius` 作为半径
- 在 XZ 平面上算出圆周位置偏移

### 3. `Planet.getWorldPosition(Vec3)`
如果要拿“考虑所有父级后的最终世界位置”，看这个：

```java
public Vec3 getWorldPosition(Vec3 in){
    in.setZero();
    for(Planet current = this; current != null; current = current.parent){
        current.addParentOffset(in);
    }
    return in;
}
```

### 4. `Universe.updatePlanet(Planet)`
运行时真正把结果写进 `planet.position`：

```java
private void updatePlanet(Planet planet){
    planet.position.setZero();
    planet.addParentOffset(planet.position);
    if(planet.parent != null){
        planet.position.add(planet.parent.position);
    }
    for(Planet child : planet.children){
        updatePlanet(child);
    }
}
```

这里说明：
- 先算自己相对父星的偏移
- 再加上父星的绝对位置
- 子星递归继续算

## 结论

如果你问“控制星球环绕位置计算的方法是哪个”：

- **核心计算方法**：`Planet.addParentOffset(Vec3)`
- **轨道角来源**：`Planet.getOrbitAngle()`
- **最终世界坐标接口**：`Planet.getWorldPosition(Vec3)`
- **每帧更新 `planet.position` 的实际调用点**：`Universe.updatePlanet(Planet)`

## Related

- [Mindustry JSON 星球生成器模板](../concepts/mindustry-json-planet-generator-template.md) — 星球定义与生成器配置
- [Mindustry JS 恒星生成模板](../concepts/mindustry-js-stellar-planet-generator.md) — 恒星/星球外观生成相关示例
