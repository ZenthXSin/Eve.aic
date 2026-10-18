# 多合成工厂

> Source: 对话总结（2026-04-21）

Mindustry 中实现“多合成/可切换配方工厂”时，最稳妥的方式通常不是直接依赖原生 `GenericCrafter`，而是通过 **自定义 `Block` + 自定义 `Building`** 来维护配方状态、输入校验、进度和输出逻辑。

## 核心思路

- 用 `recipeIndex` 记录当前选中的配方
- 每条配方定义自己的输入、输出和合成时间
- 在 `updateTile()` 中按当前配方检查库存并推进进度
- 支持 `write/read`，保证存档后配方选择不丢失
- 需要的话可扩展为“自动识别输入并匹配配方”

## 典型结构

### 配方数据

```kotlin
data class Recipe(
    val input: ItemStack,
    val output: ItemStack,
    val craftTime: Float
)
```

### 建筑状态

- `recipeIndex`: 当前配方编号
- `progress`: 当前合成进度

### 更新逻辑

- 若库存满足当前配方输入：推进 `progress`
- `progress >= 1f` 时：扣除输入并产出结果
- 若库存不足：重置进度或保持等待状态

## 实现建议

- 如果配方数量少、玩家需要手动选择：用 UI 按钮切换 `recipeIndex`
- 如果配方数量多、输入明确：用“输入物品查表”的自动匹配方式更省心
- 如果存在多输入槽：建议做成配方表 + 统一校验函数，而不是硬编码分支

## 相关

- [Mindustry JSON 多液体输入](../concepts/mindustry-json-multi-liquid-input.md) — 同样属于“一个建筑支持多种输入”的结构设计问题
- [Mindustry Kotlin 多合成工厂](../concepts/mindustry-kotlin-multi-recipe-crafter.md) — 更完整的示例实现
