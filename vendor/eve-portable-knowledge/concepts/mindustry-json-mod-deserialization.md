# Mindustry JSON Mod 反序列化流程

## 结论
Mindustry 的 JSON mod 不是“直接把文件映射成 Java 对象”这么简单，而是经过一套 **自定义 Json + ContentParser + DataPatcher** 的内容解析链。它的核心目标是：

- 先让内容名可引用
- 再分阶段把字段填回对象
- 最后做 block consumers、content init/load、补丁回滚等后处理

---

## 1. 入口层：JsonIO 提供统一的反序列化规则

`JsonIO` 里通过 `Json.setSerializer(...)` 给常见原版类型注册短名序列化器，比如：

- `Block`：按 name 读写
- `Item` / `Liquid` / `UnitType` / `Planet` / `Weather`：按内容名读写
- `ItemStack`：支持 `{item, amount}` 结构
- `UnlockableContent`：按内容名或空值解析

这意味着：

- JSON 里大量字段可以直接写成内容名字符串
- 读写时不需要完整包名
- 原版内容系统能在反序列化阶段完成名字到实例的绑定

---

## 2. ContentParser：真正处理 mod content 的核心

`ContentParser` 是 mod 内容 JSON 的主解析器。它不是简单地调用反射，而是做了很多定制化处理。

### 2.1 自定义 Json 实例
它内部维护一个 `Json parser`，并重写了：

- `resolveClass(...)`
- `newInstance(...)`
- `readValue(...)`
- `internalRead(...)`

这样做的意义是：

1. **允许短类名/受控类名解析**
2. **在对象创建后立即触发钩子**
3. **在读完值后补校验与事件通知**

### 2.2 classParsers：类型级特化反序列化
它对很多关键类型做了专门解析，比如：

- `Effect`
- `BulletType`
- `DrawBlock`
- `ShootPattern`
- `Consume`
- `Ability`
- `Weapon`
- `Sound` / `Music`
- `Team`
- `PartProgress`
- `Vec3` / `Mat3D`

这说明 JSON mod 的内容不是“全靠通用反射”，而是**大量依赖类型级定制解析器**。

---

## 3. 内容字段解析：readFields / assign / assignValue

`ContentParser` 不是一次性把整个对象塞进去，而是分层处理：

### 3.1 readFields
会遍历 JSON 对象的字段，逐个交给 `assign(...)`。

### 3.2 assign
负责：

- 处理嵌套字段路径（如 `a.b.c`）
- 处理数组/Seq/Map/ObjectSet 等容器类型
- 处理特殊字段，比如 `consumes`
- 处理 `UnitType.controller`、`aiController` 等特化字段
- 找不到字段时，按策略忽略或警告

### 3.3 assignValue
真正执行字段设置，并在需要时：

- 先记录修改前状态，方便回滚
- 递归读取嵌套对象
- 对对象创建触发 `created(...)`
- 对 content 对象触发 `init() / postInit() / load()`

---

## 4. Content 的创建与后处理顺序

从源码可以看出，内容对象的生命周期大致是：

1. **newInstance / make 创建对象**
2. **readFields 填充字段**
3. **created(...) 回调**
   - `Content.init()`
   - `Content.postInit()`
   - 非 headless 下执行 `load()`
4. **内容名绑定后再做引用解析**
5. **最后触发 afterPatch / listeners**

这套流程的重点是：

- 先注册名字，再解析互相引用
- 避免“对象还没创建就引用它”的问题

---

## 5. Block 的特殊解析：consumes

在 `DataPatcher` 里可以看到，`Block` 的 `consumes` 字段有专门逻辑：

- 先保存旧的 consumeBuilder
- 调用 `parser.readBlockConsumers(bl, jsv)`
- 再 `reinitializeConsumers()`
- 出错则回滚

这说明：

> **Block JSON 的消耗系统不是普通字段反射，而是专门的 DSL 解析。**

---

## 6. DataPatcher：JSON patch / 运行时内容修改器

`DataPatcher` 说明 Mindustry 的 JSON mod 还支持“补丁式”内容修改：

- 先复制当前 `Vars.content`
- 再对 patch JSON 进行 assign
- 支持字段追加、删除、替换
- 支持回滚

它的特点：

- 不是单纯的静态资源加载
- 而是可对已有内容进行结构化修改
- 对 mod 合并和兼容性很重要

---

## 7. 对 JSON mod 的实际意义

从这套源码可以得出几个很重要的结论：

### 7.1 JSON mod 是“数据驱动 + 类型特化”混合架构
不是只靠 HJSON/JSON 配置，而是：
- 基础字段由反序列化器处理
- 特殊字段由 classParsers / 专用方法处理

### 7.2 内容名优先于对象引用
绝大多数原版内容字段都通过 `name` 查找，而不是直接序列化 Java 对象。

### 7.3 解析分两阶段
- 第一阶段：创建对象、绑定名字、填基本字段
- 第二阶段：解析互相引用、消耗、科技树、后处理

### 7.4 兼容性靠“忽略未知字段 + 专门字段钩子”维持
源码里有 `ignoreUnknownFields = true`，说明 mod JSON 倾向于：
- 多写字段尽量不炸
- 但关键字段仍走专门逻辑

---

## 8. 对你之前“Block 科技树 JSON”的修正

结合源码，可以更准确地说：

- **Block 本体 JSON** 通常可以描述 block 的大部分属性
- **科技树** 并不是 block JSON 的普通字段
- `TechTree` 更像是一个独立的内容构建阶段
- JSON mod 如果要控制科技树，需要看是否有单独的 tech tree patch/构建入口，而不是只靠 block 文件本身

---

## 9. 一句话总结

**Mindustry 的 JSON mod 反序列化是“自定义 Json 规则 + 类型特化解析 + 分阶段字段注入 + 后处理回调”的组合体系。**

它的目标不是把 JSON 变成对象这么简单，而是让对象之间的依赖、引用、消耗、加载、回滚都能安全地工作。
