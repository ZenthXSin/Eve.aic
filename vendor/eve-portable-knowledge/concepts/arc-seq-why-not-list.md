# Arc Seq 为什么不实现 java.util.List

> Source: conversation + `源码和示例/Arc/arc-core/src/arc/struct/Seq.java`

`arc.struct.Seq` 没有实现 `java.util.List`，核心不是“作者忘了”，而是它故意做成了一套 **面向 Arc/Mindustry 运行时需求的轻量数组容器**，目标和标准集合接口并不完全一致。

## 结论

`Seq` 更像一个“高性能、可暴露底层数组、允许无序删除、带 Arc 风格函数工具”的专用容器，而不是一个严格遵守 Java Collection Framework 语义的 `List` 实现。

## 主要原因

### 1. `Seq` 的语义和 `List` 不完全兼容

`Seq` 支持 `ordered` / `unordered` 两种模式：

- `ordered = true` 时，删除元素会搬移后续元素，保持顺序。
- `ordered = false` 时，删除元素会把最后一个元素搬到被删除位置，避免整段数组拷贝。

这在源码里非常明显：

- `remove(int index)` 中，若 `ordered` 为假，执行 `items[index] = items[size];`
- `insert(int index, T value)` 中，若 `ordered` 为假，也会直接覆盖式挪位，而不是标准的顺序插入语义。

而 `List` 默认表达的是一个 **有稳定顺序的位置序列**。虽然 `List` 接口本身不强制复杂度，但大多数调用者会天然假设：

- 插入后后续元素顺移
- 删除后元素相对顺序保持
- `equals`/`hashCode` 遵循列表顺序语义

`Seq` 的无序模式会破坏这种预期，所以它不太适合伪装成一个通用 `List`。

### 2. `Seq` 公开了底层存储，目标就是“少抽象层”

`Seq` 直接暴露：

- `public T[] items;`
- `public int size;`
- `public boolean ordered;`

这说明它本来就不是为了提供标准封装，而是为了让 Arc 内部代码直接操作底层数组，减少中间层和额外检查。

而 `List` 的典型使用方式更偏向接口抽象，不鼓励外部直接碰内部数组。`Seq` 的设计哲学明显更接近游戏框架里常见的“可直接摸底层数据结构”。

### 3. 它追求 Arc 自己的 API 风格，不想被 `List` 契约绑住

`Seq` 里有大量 Arc 风格方法：

- `each`
- `map`
- `flatMap`
- `select`
- `retainAll(Boolf<T>)`
- `sort(Floatf<? super T>)`
- `random()`
- `first()` / `peek()` / `pop()`
- `asMap()` / `asSet()`

这些方法是围绕 Arc 的 `Boolf`、`Cons`、`Func`、`Floatf` 等函数接口设计的，不是围绕 Java 标准集合生态设计的。

如果它实现 `List`，就要额外承担很多标准集合兼容责任，例如：

- `ListIterator`
- `subList`
- `containsAll(Collection<?>)`
- `addAll(Collection<?>)`
- `removeAll(Collection<?>)`
- `retainAll(Collection<?>)`
- `toArray(T[])`

这会让类膨胀、增加适配代码，也会迫使 `Seq` 往 Java 标准集合语义靠拢，削弱它作为专用容器的简洁性。

### 4. `Seq` 自己的 `equals/hashCode` 就已经不是标准 `List` 路线

源码中：

- `hashCode()` 在 `ordered == false` 时直接退回 `super.hashCode()`。
- `equals(Object)` 只接受另一个 `Seq`，而且双方都必须是 `ordered`。

这意味着：

- 无序 `Seq` 根本不提供基于元素内容的标准集合相等性。
- `Seq` 也不打算和任意 `List` 做对等比较。

如果它实现了 `List`，这种行为会让人更容易误会它符合标准列表相等语义，但实际上它并不想承诺这一点。

### 5. 游戏框架更在意性能、分配和可控行为

从源码细节也能看出 `Seq` 的取向：

- 复用迭代器，尽量少分配对象
- `ensureCapacity` / `shrink` / `setSize` 直接服务底层数组管理
- `ordered = false` 时删除可避免 `System.arraycopy`
- 直接访问 `items[index]`

这些都很典型：它服务的是游戏运行时和框架内部热路径，而不是通用业务代码里的“标准集合互操作”。

## 可以怎么理解

可以把它理解成：

- **不是 Arc 版 `ArrayList` 接口实现**
- 而是 **Arc 版“可变动态数组工具类”**

它和 `ArrayList` 有相似用途，但设计目标更偏：

- 少分配
- 少包装
- 可暴露底层数组
- 为游戏逻辑提供额外便捷 API
- 在必要时允许放弃稳定顺序换性能

## 实际影响

所以你在 Mindustry / Arc 代码里会看到：

- 框架内部普遍直接用 `Seq`
- 需要 Java 标准集合兼容时，再显式转成 `ArrayList`，比如 `list()`

源码里也确实提供了：

- `public ArrayList<T> list()`

这正说明作者的意图是：

- 平时用 `Seq`
- 真要和 Java 集合生态对接，再手动转换

## 相关

- [Arc util 内容补全建议](arc-util-overview.md) — Arc 标准库方向的补充主题
- [Mindustry 项目结构](../mindustry/project-structure.md) — Arc 在 Mindustry 中的基础位置
