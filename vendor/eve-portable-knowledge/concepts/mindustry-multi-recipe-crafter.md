# Mindustry 多合成工厂

## 概述

Mindustry 的“多合成”通常指一个工厂/合成器支持**多套配方**，并能根据输入物品、液体或逻辑条件切换产物。实现上，JSON 里可以做的有限；更稳妥的方式通常是 **Kotlin/Java 自定义 Block + Building**。

## 适用场景

- 同一个建筑支持多个配方
- 依据输入物品自动匹配产物
- 依据手动选择切换当前配方
- 想做比单一 `GenericCrafter` 更灵活的工厂

## 实现思路

### 方案 1：自定义 Block / Building

最常见也最可控：

- 在 `Block` 中保存配方列表
- 在 `Building` 中判断当前满足哪个配方
- `updateTile()` 中执行消耗与生产
- 用 `draw()` / `setBars()` / `buildConfiguration()` 提供 UI 交互

### 方案 2：多个独立方块

如果不需要运行时切换，可以拆成多个方块：

- 最简单
- 最稳定
- 便于 JSON 配置
- 缺点是占用多个内容项

### 方案 3：JSON + 少量代码扩展

如果只是想让 JSON 定义配方数据，仍然可以：

- 用 JSON 保存配方表
- 在 Kotlin/Java 中读取这些数据
- 自定义逻辑执行配方匹配

## 关键点

- `consumes` 只负责声明消耗，不负责多配方切换
- 多合成的核心是“**配方选择逻辑**”
- 如果需要兼容 UI，建议把当前配方状态存到 `Building` 实例里
- 如果要给玩家选择配方，可以利用 configuration UI

## 经验建议

- 先做“能跑”的最小版本：固定多个配方，自动匹配一个
- 再加手动切换、优先级、锁定机制
- 不要一开始就把 UI、液体、动画、缓存全堆上去

## 相关页面

- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md)
- [Mindustry JSON 多液体输入](mindustry-json-multi-liquid-input.md)
- [Mindustry Kotlin 多合成工厂](mindustry-kotlin-multi-recipe-crafter.md)
