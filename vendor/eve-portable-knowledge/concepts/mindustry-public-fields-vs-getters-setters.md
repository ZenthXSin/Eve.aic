# Mindustry 模组里为什么倾向 public 字段而少用 Getter/Setter

> Source: Mindustry 源码贡献规范与 Arc/Mindustry 序列化实现；结合本次对话整理

Mindustry / Arc 的代码风格整体偏向“数据对象直接暴露字段”，而不是传统 Java Bean 风格的大量 getter/setter。

## 结论

教程里说“减少一切 Getter 或 Setter，让属性尽量 public”，核心不是教条，而是三点：

1. **符合原版代码风格**：Mindustry 源码大量内容对象、方块、单位、武器、液体等都直接公开字段。
2. **方便反射式读取与 JSON 内容解析**：Arc 的 `Json` 会直接枚举字段并按字段名读写；Mindustry 的 `ContentParser` 也依赖这一套把 JSON 字段灌进对象。
3. **减少样板代码**：大量纯数据配置对象如果全写 getter/setter，会很臃肿，收益很低。

## 源码证据

### 1. 官方贡献规范明确建议少写 getter/setter

`源码和示例/Mindustry/CONTRIBUTING.md`：

- “Avoid bloated code and unnecessary getters/setters.”
- “Public or protected fields should suffice for most things.”
- “If something needs to be encapsulated in the future, IntelliJ can handle it with a few clicks.”

这说明它首先是 **官方推荐的工程风格**。

### 2. Arc 的 JSON 序列化器直接按字段反射，不是按 getter/setter

`源码和示例/Arc/arc-core/src/arc/util/serialization/Json.java` 中：

- `getFields(Class type)` 会遍历 `getDeclaredFields()`
- 过滤掉 `static/transient`
- `field.setAccessible(true)`
- 按 `field.getName()` 建立字段表
- `readFields(...)` 时直接 `field.set(object, readValue(...))`

这说明 Arc 的通用 JSON 机制天然偏向：

- **字段名 = 配置名**
- **直接给字段赋值**
- **并不要求 Java Bean 的 getter/setter**

### 3. Mindustry 的内容解析器沿用这套字段写入方式

`源码和示例/Mindustry/core/src/mindustry/mod/ContentParser.java`：

- 内容解析最终会走 `readFields(block, value, true)` 一类逻辑
- 底层仍调用 Arc `Json` 的字段反射赋值机制
- `consumes/liquid/liquids/items` 等配置也都是先解析再直接写入对象字段或调用内容配置方法

所以在 Mindustry mod 里，很多对象本质上更像：

- 配置载体
- 数据结构
- 引擎可直接读写的内容定义

而不是强调封装层层包裹的业务对象。

## 为什么教程会这么说

因为在 Mindustry mod 开发里，很多类本来就是“声明参数给引擎读”的：

- `Liquid` 的颜色、温度、黏度等
- `Weapon` 的 `reload`、`shake`、`bullet` 等
- `UnitType` 的各种数值和行为字段
- `Block` 及其子类上的范围、消耗、贴图、效果配置

这类字段：

- 经常要被 JSON / patch / 反射系统识别
- 主要用途是“存值”而不是“保护值”
- 频繁写 getter/setter 只会增加样板代码

## 这不代表“永远不要 getter/setter”

原文也说了：**unless absolutely necessary**。

适合保留 getter/setter 或封装的场景：

- 赋值时必须做校验
- 需要维持多个字段之间的一致性
- 对外只想暴露只读视图
- 赋值会触发副作用/重建缓存/同步状态
- 想隐藏内部实现细节

也就是说：

- **纯配置字段** → public 很正常
- **带约束、带副作用、带不变量的状态** → 可以封装

## 对 mod 作者的实际建议

### 适合 public 字段的内容

- 方块参数
- 单位参数
- 武器参数
- 液体/物品/子弹的静态配置
- 需要被 JSON / patch / 反射直接识别的字段

### 适合方法封装的内容

- 运行时状态切换逻辑
- 需要保证合法范围的赋值
- 一次赋值要联动刷新多个缓存
- 不希望外部随便改的内部状态

## 容易误解的一点

“字段 public” 不等于 “代码随便写”。

Mindustry 的意思更接近：

- **对数据类少写无意义封装**
- **对引擎配置对象保持扁平直接**
- **把复杂度放在真正有逻辑的地方，而不是样板方法上**

## Related

- [Mindustry Java Mod Structure](../mindustry/java-mod-structure.md) — Java Mod 的基础结构与内容组织
- [Mindustry JSON Mod 反序列化流程](../concepts/mindustry-json-mod-deserialization.md) — JSON 内容如何通过解析器与字段反射进入对象
- [Mindustry JSON 接口真实存在性检查](../concepts/mindustry-json-interface-existence-check.md) — JSON / 反射问题先确认真实字段与类型
