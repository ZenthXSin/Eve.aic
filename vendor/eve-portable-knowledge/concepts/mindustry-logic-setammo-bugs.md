# SetAmmoStatement (LAmmo) 双 bug

> Source: `源码和示例/Mindustry/core/src/mindustry/logic/LAmmo.java`（本地副本 `~/cow/tmp/LAmmo.java`）的 MindustryMIT 逻辑扩展，非原版内容

## Bug 1: `rebuild()` 中 `build()` 与 `resetValue()` 顺序颠倒

**位置**: `LAmmo.java:287-290`
```java
public void rebuild(Table table) {
    build(table);      // ← 先构建 UI（使用当前 value）
    resetValue();      // ← 再将 value 重置为默认
}
```

`build(table)` 用当前 `value` 字段设 UI 文本框内容，然后 `resetValue()` 将 `value` 改为默认值。
结果：UI 显示旧值，但底层 `value` 已被重写，后续操作读到的是默认而非当前值。
**修复**: 先 `resetValue()`, 再 `build(table)`。

## Bug 2: `catched` 字段为 `static` 造成跨实例污染

**位置**: `LAmmo.java:180`
```java
static Class<? extends BulletType> catched;
```

多个 `SetAmmoStatement` 实例共享同一个 `catched` 状态。
当多个语句同时存在时，一个实例的缓存修改会污染另一个实例的预测行为。
**修复**: 改为实例字段，或在 `SetAmmoStatement` 加独立的 `selectedClass` 字段。