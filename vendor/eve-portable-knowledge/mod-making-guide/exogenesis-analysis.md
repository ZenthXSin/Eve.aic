# ExoGenesis — 浅层分析

- **Stars**: 440
- **作者**: AureusStratus
- **语言**: Java/JSON
- **定位**: 多种族全战役大型内容扩展
- **仓库**: https://github.com/AureusStratus/ExoGenesis
- **拉取日期**: 2026-05-18

## 为什么受欢迎

### 1. 5 个独立种族/阵营
- Genesux（冰霜）、Solran（熔岩）、Elecian（电击）、Terretux（黑暗）、Quantra（水生）
- 每个种族有独立 lore、武器、防御、单位
- 对应 3 个不同的恒星系统

### 2. 完整的战役系统
- 每个种族有独立的行星和卫星
- 自洽的 lore 背景，增加了沉浸感

### 3. 内容量
- 大量自定义方块、单位、炮塔
- 规模堪比小型独立游戏

## 技术要点（浅层）

- Java + JSON 混合架构
- 多种族通过独立的 tech tree 分支实现
- 主入口 `Exogenesis.java`

> **待深层分析**: 多科技线实现方式、种族间平衡策略、战役加载机制