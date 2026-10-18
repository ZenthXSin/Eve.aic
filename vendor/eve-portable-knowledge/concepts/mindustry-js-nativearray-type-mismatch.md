# JS Rhino NativeArray → Java 数组类型崩溃

> Source: 2026-05-10 对话，Mindustry 社区常见问题

## 问题

JS 中用 `[item1, item2]` 创建的数组，传入期望 Java 数组的方法时触发 `ClassCastException` 或 `IllegalArgumentException`。

## 根因

Mindustry 使用 Rhino JS 引擎，JS 的数组字面量 `[...]` 被包装为 `org.mozilla.javascript.NativeArray`，而 Java 方法（如 `Seq.with(T...)`、注册表的 `add(Content[])`）期望的是原生 Java 数组类型。

当科技树/升级链使用 `Seq.with([unit1, unit2])` 写法时，实际传递的是 `NativeArray` 而非 `UnitType[]`，导致类型不匹配崩溃。

## 修复

手动将 JS 数组转为 Java 数组：

```js
// 错误写法
var units = Seq.with([unit1, unit2]); // NativeArray → crash

// 正确写法  
var units = Seq.with(unit1, unit2);
// 或显式创建 Java 数组
var units = java.lang.reflect.Array.newInstance(java.lang.Class.forType(UnitType), 2);
units[0] = unit1;
units[1] = unit2;
```

**原则**: 在 JS mod 中，调用 `Seq.with()`、`ObjectMap.put` 等 Java 泛型/可变参数方法时，应逐个传参而非传数组字面量。