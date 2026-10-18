# Mindustry JSON 不存在的 Block `type` 回退为 `Block`

> Source: 对话分析 + Mindustry 155.4 源码核对

当方块 JSON 中填写的 `type` 不是真实存在且可解析的类名时，Mindustry 会在解析阶段把它回退为基础 `Block`，而不是报出“这个方块一定完全不可用”的单一结果。

## 结论

- `CombustionGenerator` **不是** Mindustry 155.4 中可直接用于 block JSON 的真实 block 类名。
- 原版对应的真实类名是 [`ConsumeGenerator`](../concepts/mindustry-json-invalid-block-type-fallback.md) 所代表的发电机方块类。
- 在 `ContentParser` 里，block 内容创建使用 `resolve(value.getString("type", ""), Block.class)`。
- 如果 `type` 无法从 `ClassMap` 解析，解析器会 **fallback 到 `Block.class`**。
- 因此，一个本来想写成发电机的 JSON，如果 `type` 写错，最终会变成“基础 Block 行为 + 你仍然写进去的一些通用字段”。

## 关键源码证据

### 1. block 解析时的默认回退
`core/src/mindustry/mod/ContentParser.java`

```java
block = make(resolve(value.getString("type", ""), Block.class), mod + "-" + name);
```

这说明 block JSON 的 `type` 解析失败时，默认类型就是 `Block.class`。

### 2. `resolve` 的失败回退逻辑
`core/src/mindustry/mod/ContentParser.java`

```java
if(def != null){
    if(warn) warn("[@] No type '" + base + "' found, defaulting to type '" + def.getSimpleName() + "'", ...);
    return def;
}
```

也就是说：找不到类型时，会警告并回退到传入的默认类。

### 3. 可解析类名来自 `ClassMap`
`core/src/mindustry/mod/ClassMap.java`

```java
classes.put("ConsumeGenerator", mindustry.world.blocks.power.ConsumeGenerator.class);
```

存在的是 `ConsumeGenerator`，不是 `CombustionGenerator`。

## 为什么会出现“基础信息/生命值/选中异常”印象

当内容退化成基础 `Block` 后：

- 不再具备 `ConsumeGenerator` / `PowerGenerator` 的专属行为
- `consumes`、`powerProduction`、发电条目、液体输出等发电机语义不会按预期工作
- 某些界面显示会退化成基础方块显示
- 如果再叠加贴图、实体创建、放置状态或其他配置问题，用户就会看到“像假方块/空壳方块一样”的表现

## 排查建议

1. **先验真**：确认 `type` 是否真实存在于 `ClassMap` / parser 支持范围内。
2. block JSON 里原本写 `CombustionGenerator` 时，应优先改查是否真实类名是 `ConsumeGenerator`。
3. 再检查：
   - 贴图是否存在
   - 名称与文件路径是否正确
   - 是否成功放置为建筑实体
   - 日志里是否出现 `No type 'xxx' found, defaulting to type 'Block'`

## 相关

- [Mindustry JSON 中的 `type` 是否真实存在](../concepts/mindustry-json-type-real-existence.md) — `type` 的真实含义与验证原则
- [Mindustry JSON 接口真实存在性检查](../concepts/mindustry-json-interface-existence-check.md) — JSON 先验真规则
- [Mindustry JSON 问题解决流程](../concepts/json-problem-solving-flow.md) — 推荐排查顺序
