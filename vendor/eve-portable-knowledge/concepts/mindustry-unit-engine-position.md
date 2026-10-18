# Mindustry 单位引擎/推进器位置自定义

> Source: UnitType.java:135-138, 336-337, 1018-1019, 1457-1466, 2005-2012

## 结论

单位推进器位置通过 `engineOffset`/`engineSize` 控制单引擎默认位置，`engines` 数组自定义多引擎位置，`setEnginesMirror()` 实现镜像对称排列。

## 关键字段（UnitType.java）

```java
// UnitType.java:135-138
public float engineOffset = 5f;      // 引擎偏移（世界单位）
public float engineSize = 2.5f;      // 引擎半径
// engineColor, engineColorInner 控制颜色

// UnitType.java:336-337
public Seq<UnitEngine> engines = new Seq<>();  // 多引擎列表
```

## 自动添加默认引擎（UnitType.java:1018-1019）

```java
if(engineSize > 0){
    engines.add(new UnitEngine(0f, -engineOffset, engineSize, -90f));
}
```

## UnitEngine 内部类（UnitType.java:2005-2012）

```java
public static class UnitEngine implements Cloneable {
    public float x, y, radius, rotation;
    // x/y = 偏移位置, radius = 引擎半径, rotation = 绘制旋转角度
}
```

## setEnginesMirror 镜像排列（UnitType.java:1457-1466）

```java
public void setEnginesMirror(UnitEngine... array){
    for(var base : array){
        engines.add(base);           // 原始引擎
        var engine = base.copy();
        engine.x *= -1;              // x 取反
        engine.rotation = 180f - engine.rotation;  // 旋转角镜像
        if(engine.rotation < 0) engine.rotation += 360f;
        engines.add(engine);         // 镜像引擎
    }
}
```

## 用法示例

### 单引擎（默认）
```json
{ "engineOffset": 5, "engineSize": 2.5 }
```

### 多引擎（JSON 不直接支持，需 JS/Kotlin）
```javascript
// JS mod 示例
unitType.engines.clear();
unitType.setEnginesMirror(
    new UnitType.UnitEngine(4, -6, 2, -90),
    new UnitType.UnitEngine(8, -4, 1.5, -90)
);
```

## 硬性规则

1. `engineOffset` 单位是世界单位（1格=8世界单位），不是瓦片
2. `engineSize` 是引擎绘制半径，不是偏移
3. 默认引擎在 `init()` 时自动添加到 `engines` 列表
4. 设置 `engines` 后会覆盖默认引擎（`init()` 中先清空再添加）
5. `setEnginesMirror` 只做 x 轴镜像，不做 y 轴
6. `useEngineElevation` 控制引擎是否随单位海拔缩放
7. JSON 中只能用 `engineOffset`/`engineSize` 简写，多引擎需 JS/Kotlin

## 关联

- [UnitType 完整字段](mindustry-unit-system.md)
- [Block and Building System](../mindustry/block-building-system.md)
