# Learn Mindustry Mod: Kotlin Developer View

> Source: knowledge/sources/learn-mindustry-mod-github-io/overview.md and related overview pages

从 Kotlin 开发者角度看，LMM 主要是在教你如何把 Mindustry 模组开发拆成一套可执行的工程化流程，而不是只背 API。

## 它讲什么

- **环境与工程结构**：Gradle、项目目录、模板、部署方式
- **内容注册**：物品、液体、方块、工厂、炮塔、单位、科技树、星球
- **运行机制**：方块与建筑、消耗与生产、物流、电力、事件、载荷
- **表现层**：绘图、动画、Shader、FrameBuffer、UI、渲染流程
- **调试与排错**：常见报错、类型查找、开发环境问题

## 对 Kotlin 开发者的意义

- 核心是把 **Java 风格的 Mindustry API** 转成 Kotlin 能舒服处理的工程代码。
- 重点不是“会不会写语法”，而是 **如何组织内容注册、生命周期、渲染和交互逻辑**。
- 你会持续碰到 `Block`、`Building`、`UnitType`、`Weapon`、`Effect` 这类原版类型，所以教程本质上是在教你怎么围绕这些类型搭项目。
- 如果你做的是 Kotlin 模组，LMM 的主价值是帮你建立 **Mindustry 内容系统 + 渲染系统 + 程序结构** 的整体认知。

## 一句话总结

LMM 不是 Kotlin 语法课，而是一套面向 Mindustry 模组的工程教材：教你用 Kotlin 思维去组织内容、逻辑、渲染和调试。

## Related

- [Learn Mindustry Mod GitHub Pages Overview](../sources/learn-mindustry-mod-github-io/overview.md)
- [Java/Kotlin Tutorial Overview](../sources/learn-mindustry-mod-github-io/java-tutorial-overview.md)
- [Mindustry 渲染系统](../mindustry/rendering-system.md)
