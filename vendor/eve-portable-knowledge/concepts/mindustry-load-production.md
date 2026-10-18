# Mindustry 工厂输入/输出载荷生产

## 概述

在 Mindustry 中，如果你说的是“让工厂能够输入/输出载荷进行生产”，通常有两种语境：

1. **输入/输出物品或液体** 来完成生产
2. **输入/输出载荷（payload）**，即把一个方块/单位/结构作为载荷搬运和处理

如果目标是“工厂吃进去一个载荷，吐出另一个载荷”，那就不是普通 `GenericCrafter` 的物品消耗逻辑了，而是 **Payload 系统**。

## 两条实现路线

### 路线 1：普通工厂输入/输出物品

适合：
- 输入矿物、液体、电力
- 输出物品、单位或简单产物

做法：
- 使用 `consumeItem(...)` / `consumeLiquid(...)` / `consumePower(...)`
- 产物通过 `outputItem`、`dumpOutput` 等方式输出
- 这是最常见的“工厂生产”

### 路线 2：Payload 工厂

适合：
- 输入一个 payload，经过加工再输出
- 处理 unit payload / block payload
- 做装配厂、拆解厂、改造厂

做法：
- 使用 `PayloadBlock` 或其相关子类
- 通过 payload 输入/输出接口处理载荷
- 在 Building 中检查 `payload` 是否满足条件
- 使用 `payloads` 或 `acceptPayload()` / `handlePayload()` 之类机制（具体依版本而定）

## 关键判断

- **如果你的“载荷”指物品/液体**：用普通 `GenericCrafter` + consumes 就够了
- **如果你的“载荷”指 payload 载荷**：要上 Payload 体系，不能只靠 `consumes`

## 实践建议

- 想做“能搬东西进工厂再加工出来”的感觉，先明确你要搬的是：
  - item
  - liquid
  - unit payload
  - block payload
- 如果只是生产链，多数情况下用物品输入输出更简单、稳定
- 如果确实要 payload，就从现成的 payload block 结构入手，而不是在普通 crafter 上硬加字段

## 相关知识

- [Mindustry 多合成工厂](mindustry-multi-recipe-crafter.md)
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md)
