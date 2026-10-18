# Mindustry Payload 输入/输出关键片段

## 说明

Payload 不是普通物品，不能只靠 `items.remove(...)` / `offload(...)` 处理。它通常依赖 `PayloadBlock`、`PayloadBuild` 以及 `acceptPayload()` / `handlePayload()` / `payload` 相关 API。

## 常见关键片段

### 1. 接收 payload
```kotlin
override fun acceptPayload(source: Building, payload: Payload): Boolean {
    return current == null && payload.block() != null
}
```

### 2. 处理收到的 payload
```kotlin
override fun handlePayload(source: Building, payload: Payload) {
    current = payload
    progress = 0f
}
```

### 3. 在工厂中推进处理
```kotlin
override fun updateTile() {
    if (current == null) return
    progress += edelta() / craftTime

    if (progress >= 1f) {
        progress = 0f
        val result = current
        current = null
        // 这里把 result 替换成新 payload 再输出
    }
}
```

### 4. 输出 payload
```kotlin
fun outputPayload(result: Payload) {
    // 具体输出 API 依版本不同，通常是把 payload 放回附近可接收的位置
}
```

## 典型流程

1. 外部把 payload 送入工厂
2. `acceptPayload(...)` 判断能否接收
3. `handlePayload(...)` 存入当前 payload
4. `updateTile()` 处理进度
5. 完成后生成新 payload 并输出

## 注意

- `Payload`、`UnitPayload`、`BuildPayload`、`PayloadBlock` 的具体类名和可用方法会随版本变化
- 如果你只需要“搬运方块”，优先参考现成 payload block 的实现
- 真正可编译的版本通常要配合目标 Mindustry 版本源码对照
