# Block 科技树的 JSON 定义方式

> Source: `mindustry.mod.ContentParser.java` v158 lines 1243-1373

## 核心机制：`research` 字段

`research` 是 `readFields()` 中从 JSON 提取并**移除**的字段，在反序列化时被单独处理——它不是 Block 类的公开字段，而是在所有 `UnlockableContent` 子类（Block/Item/Unit/Liquid/Planet 等）的 JSON 中通用。

## 两种格式

### 1. 字符串格式（指定父节点名）
```json
"research": "graphite-press"
```
- `parent` = `"graphite-press"`
- 需求使用 `unlock.researchRequirements()` 默认值

### 2. 对象格式
```json
"research": {
  "parent": "graphite-press",
  "requirements": ["copper/50", "lead/30"],
  "objectives": [{ ... }],
  "planet": "serpulo",
  "root": false,
  "requiresUnlock": false,
  "name": "custom-node-name"
}
```

## 关键细节（源验证）

- **处理时机**：`research` 的处理包裹在 `postreads.add(() -> {...})` 中，即所有内容反序列化完成后才执行节点挂接
- **父节点匹配**：`TechTree.all.find()` 尝试三种名称匹配：
  1. 原始名称
  2. `mod名-名称`（自动加前缀）
  3. `SaveVersion.mapFallback(researchName)`（兼容旧版本名）
- **重复节点**：如果 `TechTree.all` 中已存在相同 `content` 的旧节点，先移除再创建
- **自动 Produce**：Item/Liquid 如果没有其他 `Produce` objective，自动添加 `Produce(unlock)`
- **Planet**：`research.planet` 设置行星；非 root 节点默认继承父节点的 planet
- **root 节点**：独立加入 `TechTree.roots`，不挂到其他节点下；必须提供 `name`
- **需求**：对象格式提供 `requirements` 时直接使用，否则调用 `unlock.researchRequirements()`
- **旧节点引用**：若旧节点已有 parent，旧节点会被从 `parent.children` 中移除

## 与旧知识库的差异

已有的 tech-tree 知识文档指的是 `TechTreeNode` 的独立 JSON 格式，但 **v158 源码里没有对应的独立 JSON 解析逻辑**。原版 JSON mod 的 tech tree 挂接就是通过 Block/Item/Unit/Liquid 等内容的 `research` 字段逐节点完成的，不需要额外的 "独立科技树 JSON 文件"。

如果需要完整的树结构，是在 Java/Kotlin 的 `TechTree` 构建中完成的（如 `mindustry.content.TechTree` 中的 `nodeRoot`/`node`/`nodeProduce` 调用链），JSON mod 只负责把自己挂到现有节点上。

## Related

- [Mindustry JSON 可用接口概览](../concepts/mindustry-json-interfaces.md)
- [JSON 问题解决流程](../concepts/json-problem-solving-flow.md)
