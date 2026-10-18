# Mindustry Recipe Graph 多合成选择器

> Source: 对话总结（2026-05-23）：基于 `RecipeGraphNode` 的多合成系统设计

把多合成工厂拆成三层：图结构、选择器、Building 运行态。

## 核心分层

- `ResourceNode`：单个资源概念，不直接参与一次合成扣除。
- `ResourcePackNode`：一次配方需要/产出的资源包，适合对应库存检查和输出。
- `RecipeNode`：从 input pack 到 output pack 的边，携带输入/输出处理器。
- `RecipeSelector`：从可用配方列表里选择当前配方，支持手动、自动、优先级、随机、按库存最优等策略。
- `Building.State`：只保存运行态资源、当前配方 id/index、progress，不把图逻辑塞进 `updateTile()`。

## 推荐接口

```kotlin
fun interface RecipeSelector {
    fun select(build: Building, state: State, recipes: Seq<RecipeNode>): RecipeNode?
}
```

常见实现：

- `ManualSelector`：玩家 UI 指定 id。
- `FirstAvailableSelector`：第一个输入满足的配方。
- `WeightedSelector`：按权重或优先级选择。
- `DemandSelector`：根据输出缓存缺口选择。

## Building 运行流程

1. selector 选择 recipe。
2. `iHandler.canInput(build, state, recipe.input)` 判断输入是否满足。
3. 满足则推进 progress。
4. 完成后 `iHandler.consume(...)` 扣输入。
5. `oHandler.output(...)` 写入输出。
6. 保存 `selectedRecipeId/index + progress`，避免存档后状态丢失。

## 设计取舍

图节点负责描述关系，选择器负责决策，handler 负责实际 IO。不要让 `RecipeNode` 自己访问建筑库存，否则图结构会和 Mindustry Building 强耦合，后面扩展液体、载荷、热量会变成粪坑。

## Related

- [Mindustry Kotlin 多合成工厂](mindustry-kotlin-multi-recipe-crafter.md) — 多配方工厂的基础状态机

## 一切皆 Resource 扩展

把 `time`、地块、温度、热量、环境、权限等都视作 `ResourceType` 是可行的，但要分清两类资源：

- **Stock resource**：可存储、可扣除、可输出，例如 item/liquid/payload/energy-buffer。
- **Context resource**：来自环境或过程，不一定存进库存，例如 time、floor、overlay、sector、weather、team、nearby block。

推荐让 `ResourceType` 携带 `kind` 或 `domain`，再由不同 `InputHandler/OutputHandler` 解释：

```kotlin
enum class ResourceKind { Stock, Flow, Context }
```

`time` 更适合做 `Flow`：每 tick 由 `edelta()` 注入，配方消耗的是累计进度；地块更适合做 `Context`：只参与 `canInput` 判断，不从建筑库存里扣。这样“万物皆资源”不会把状态机搞成不可存档的粪坑。
