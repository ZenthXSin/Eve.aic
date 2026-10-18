# Mindustry 统计系统源码验证

## GameStats（单局统计）

- 路径：`mindustry.game.GameStats`
- 当前局的游戏统计

### 字段

| 字段 | 类型 | 含义 |
|------|------|------|
| `enemyUnitsDestroyed` | int | 敌方单位摧毁数 |
| `wavesLasted` | int | 存活波次数 |
| `buildingsBuilt` | int | 友方建筑建造数 |
| `buildingsDeconstructed` | int | 友方建筑拆除数 |
| `buildingsDestroyed` | int | 友方建筑被摧毁数 |
| `unitsCreated` | int | 总单位创建数 |
| `placedBlockCount` | ObjectIntMap<Block> | 放置方块计数（用于目标） |
| `destroyedBlockCount` | ObjectIntMap<Block> | 摧毁敌方方块计数 |
| `coreItemCount` | ObjectIntMap<Item> | 进入核心的物品计数 |

### 方法

- `getPlaced(Block)` — 获取某方块放置数
- `getDestroyed(Block)` — 获取某方块摧毁数

### 注意

- `coreItemCount` 只统计通过运输块进入核心的物品
- 可以被卸载器（unloader）"欺骗"，不应用于重要逻辑
- `placedBlockCount` 用于目标系统

## CampaignStats（战役统计）

- 路径：`mindustry.game.CampaignStats`
- 单个星球的战役统计

### 字段

| 字段 | 类型 | 含义 |
|------|------|------|
| `enemyUnitsDestroyed` | ObjectIntMap<UnitType> | 敌方单位按类型统计 |
| `enemyBuildingsDestroyed` | ObjectIntMap<Block> | 敌方建筑按类型统计 |
| `unitsProduced` | ObjectIntMap<UnitType> | 我方单位生产统计 |
| `unitsDestroyed` | ObjectIntMap<UnitType> | 我方单位损失统计 |
| `buildingsBuilt` | ObjectIntMap<Block> | 建造方块统计 |
| `buildingsDeconstructed` | ObjectIntMap<Block> | 拆除方块统计 |
| `buildingsDestroyed` | ObjectIntMap<Block> | 被毁方块统计 |
| `playtime` | long | 战役总游戏时间（毫秒） |
| `sectorsLost` | int | 丢失扇区数 |
| `sectorsCaptured` | int | 占领扇区数（含重新占领） |
| `wavesLasted` | int | 存活波次数 |

### 注意

- `sectorsCaptured` 包含重新夺回的扇区
- `playtime` 是毫秒，不是秒
- CampaignStats 比 GameStats 更详细，按类型分类

## Stat / StatCat / StatUnit / StatValue（方块统计展示）

- `Stat` — 统计项枚举（health, size, buildTime, powerUse 等）
- `StatCat` — 统计分类（general, power, liquids, items, defense, production）
- `StatUnit` — 统计单位（none, perSecond, powerSecond, liquidSecond 等）
- `StatValue` — 统计值接口，用于自定义显示

### Stat 关键值

- `Stat.health` — 生命值
- `Stat.size` — 尺寸
- `Stat.buildTime` — 建造时间
- `Stat.buildCost` — 建造成本
- `Stat.powerCapacity` — 电力容量
- `Stat.itemCapacity` — 物品容量
- `Stat.memoryCapacity` — 内存容量

## SStat（服务统计）

- 路径：`mindustry.service.SStat`
- Steam/平台服务统计

## 硬性规则

1. GameStats 是单局统计，重启后清零
2. CampaignStats 是按星球持久化
3. `coreItemCount` 可被卸载器欺骗
4. `playtime` 单位是毫秒
5. `sectorsCaptured` 包含重新占领
6. Stat 枚举用于方块信息面板展示
7. StatCat 有 6 个分类：general/power/liquids/items/defense/production
8. GameStats 和 CampaignStats 是独立类，不互相继承
9. CampaignStats 按 UnitType/Block 分类，比 GameStats 更精细

## 关联

- [Block and Building System](../mindustry/block-building-system.md)
- [Campaign System](../mindustry/campaign-system.md)
- [Content System](../mindustry/content-system.md)
