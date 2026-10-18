# Mindustry 科技树与研究系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/content/TechTree.java` (202行)

## TechTree 类

### 静态字段
| 字段 | 类型 | 说明 |
|------|------|------|
| all | Seq\<TechNode\> | 所有节点 |
| roots | Seq\<TechNode\> | 根节点列表 |
| context | TechNode（private） | 当前构建上下文 |

### 构建方法
| 方法 | 参数 | 说明 |
|------|------|------|
| nodeRoot(name, content, children) | 名称/内容/子节点 | 创建根节点 |
| nodeRoot(name, content, requireUnlock, children) | 名称/内容/需解锁/子节点 | 创建需解锁根节点 |
| node(content, children) | 内容/子节点 | 自动取 content.researchRequirements() |
| node(content, requirements, children) | 内容/需求/子节点 | 自定义需求 |
| node(content, requirements, objectives, children) | 内容/需求/目标/子节点 | 完整版 |
| node(content, objectives, children) | 内容/目标/子节点 | 自动取 researchRequirements() |

### 构建模式
TechTree 使用**上下文栈**构建（不是递归构造）：
```java
nodeRoot("core", Blocks.core, () -> {
    node(Blocks.router, () -> {
        node(Blocks.sorter);
    });
});
```
内部通过 `context = node; children.run(); context = prev;` 实现

## TechNode 字段

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| depth | int | 0 | 树深度 |
| icon | Drawable | null | 图标（null=content.uiIcon） |
| name | String | null | 根节点名称 |
| requiresUnlock | boolean | false | 根节点是否需解锁 |
| parent | TechNode | null | 父节点 |
| rootNode | TechNode | null | 根节点（parent.rootNode == null ? parent : parent.rootNode） |
| researchCostMultipliers | ObjectFloatMap\<Item\> | null | 研究费用倍率（继承自父） |
| content | UnlockableContent | - | 研究目标内容 |
| requirements | ItemStack[] | - | 物品需求 |
| finishedRequirements | ItemStack[] | - | 已完成需求（持久化到 settings） |
| objectives | Seq\<Objective\> | empty | 额外目标 |
| children | Seq\<TechNode\> | empty | 子节点 |
| planet | Planet | null（自动检测） | 关联星球 |

### TechNode 构造函数逻辑
1. 设置 parent/rootNode/depth
2. 继承 researchCostMultipliers
3. 应用费用倍率到 requirements
4. content.getDependencies() → 自动添加 Research 目标
5. 设置 content.techNode 和 content.techNodes
6. 加入 all 列表

## Objective 类型
| 类 | 字段 | 说明 |
|----|------|------|
| Research | content | 研究某内容 |
| SectorComplete | preset | 完成扇区 |
| SectorOnPlanet | planet | 到达星球 |
| Produce | item | 生产物品 |
| Unlock | content | 解锁内容 |

## 研究进度持久化
- 存储位置：`Core.settings`
- 键名格式：`"req-" + content.name + "-" + item.name`
- 值类型：int（已完成数量）
- reset() 清零所有 finishedRequirements 并保存
- save() 将当前进度写入 settings

## 自动依赖注入
TechNode 构造时自动调用 `content.getDependencies()`：
- 遍历所有依赖内容
- 自动添加 `new Research(d)` 到 objectives
- 用 ObjectSet 去重

## 根节点特殊逻辑
- nodeRoot 设置 `name` 和 `requiresUnlock`
- requiresUnlock=true：在研究界面中需先解锁才可选择（不在当前星球时不适用）
- 根节点通过 addDatabaseTab() 和 addPlanet() 影响数据库标签页

## 硬性规则

1. **TechNode 是 TechTree 的静态内部类** — 不是独立文件
2. **构建用上下文栈不是递归构造** — context 字段控制
3. **requirements 自动应用 researchCostMultipliers** — 构造函数中复制并乘以倍率
4. **dependencies 自动转为 Research objectives** — 不需手动添加
5. **finishedRequirements 持久化到 Core.settings** — 不是存档，是全局设置
6. **rootNode 自动向上查找** — parent.rootNode == null ? parent : parent.rootNode
7. **planet 默认继承父节点** — 可覆盖
8. **requiresUnlock 仅对根节点有效** — 非根节点此字段无意义
9. **node() 不返回值时用 nodeRoot/node 的 Runnable 版本** — 但实际都返回 TechNode
10. **SectorComplete 自动插入** — 当 context 是 SectorPreset 时自动添加

## 关联
- [Research Tech Tree System](research-tech-tree-system.md)
- [Content System](content-system.md)
- [Campaign System](campaign-system.md)
