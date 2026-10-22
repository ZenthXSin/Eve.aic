# crystal-works bundle key 格式规范

## 现状（Kotlin 内容注册）

crystal-works 已从 HJSON 迁移到 Kotlin 内容注册。Kotlin 代码中的内容名自带 `crystal-works-` 前缀：

```kotlin
Drill("crystal-works-crystal-drill")  // 内容名为 crystal-works-crystal-drill
```

因此 bundle key 必须匹配这个前缀：

```
block.crystal-works-crystal-drill.name = 水晶钻机
item.crystal-works-crystal.name = 水晶
```

## Key 格式规则

Mindustry bundle key 格式：`type.内容名.field`

- `type`：`block`、`item`、`liquid`、`status`、`mod`、`bar`、`stat`、`recipe`
- `内容名`：Kotlin 注册时传入的字符串（如 `crystal-works-crystal-drill`）
- `field`：`name`、`description`

## 对比

| 注册方式 | 内容名示例 | bundle key 示例 |
|---------|-----------|----------------|
| HJSON（旧） | `crystal-drill`（来自文件名） | `block.crystal-drill.name` |
| Kotlin（当前） | `crystal-works-crystal-drill`（代码硬编码） | `block.crystal-works-crystal-drill.name` |

bundle key 必须与内容名完全一致，键名前缀取决于注册时的内容名本身是否包含 modname。