# Campaign System

## 概述
Mindustry 战役系统基于星球（Planet）+ 扇区（Sector）+ 核心（Core）的三层结构。玩家通过占领扇区、解锁科技树、发射资源来推进战役进度。

## 核心类

### GameState（122行）
- 包路径：`mindustry.core.GameState`
- 战役判断：`isCampaign()` = `rules.sector != null`
- 关键字段：
  - `wave: int` — 当前波次（非波次模式也可用）
  - `wavetime: float` — 波次倒计时（ticks）
  - `tick: double` — 逻辑 tick
  - `gameOver: boolean` — 游戏结束状态
  - `won: boolean` — 玩家是否胜利
  - `map: Map` — 当前地图
  - `rules: Rules` — 游戏规则
  - `stats: GameStats` — 统计数据
  - `teams: Teams` — 队伍数据
  - `patcher: DataPatcher` — JSON 补丁系统
  - `state: State` — 游戏状态（menu/playing/paused）

### Planet 系统
- 包路径：`mindustry.type.Planet`
- 星球定义：Serpulo、Erekir、Tantros 等
- 关键字段：
  - `sectors: Seq<Sector>` — 所有扇区
  - `defaultCore: Block` — 默认核心方块
  - `launchCapacity: int` — 发射容量
  - `ruleSetter: Cons<Rules>` — 规则设置器

### Sector 系统
- 包路径：`mindustry.type.Sector`
- 扇区是星球上的可占领区域
- 关键字段：
  - `planet: Planet` — 所属星球
  - `id: int` — 扇区 ID
  - `preset: SectorPreset` — 预设地图（如有）
  - `save: SectorSave` — 存档数据
  - `info: SectorInfo` — 扇区信息（资源、威胁等）

### SectorPreset
- 包路径：`mindustry.maps.SectorPreset`
- 预设扇区有固定地图和任务
- 非预设扇区是程序生成的

### SectorInfo
- 扇区状态信息
- 关键字段：
  - `resources: Seq<Item>` — 可用资源
  - `waves: int` — 总波次
  - `threat: float` — 威胁等级（0~1）
  - `waveVersion: int` — 波次版本（用于检测变更）
  - `hasCore: boolean` — 是否有核心
  - `isBeingPlayed: boolean` — 是否正在游玩

### SectorSave
- 扇区存档数据
- 保存在 `saves/` 目录下
- 格式为 `.msav`（与普通存档相同）

## 战役流程

### 1. 扇区选择
```
PlanetDialog → 选择星球 → 选择扇区 → 加载/生成地图
```

### 2. 扇区占领
```
开始游戏 → 防御波次 → 击败所有敌人 → 扇区归属玩家
```

### 3. 资源发射
```
发射台（LaunchPad）→ 收集资源 → 达到容量 → 发射到太空
```

### 4. 科技解锁
```
研究核心（Research Core）→ 消耗资源 → 解锁新内容
```

## 关键机制

### 核心系统
- 每个扇区有一个或多个核心
- 核心是玩家的主基地
- 核心被摧毁 = 游戏结束
- 核心存储资源和单位

### 发射台系统
- `LaunchPad` 方块收集资源
- 达到 `launchCapacity` 后自动发射
- 发射的资源可用于解锁科技或支援其他扇区
- 发射台有冷却时间

### 波次系统
- `WaveSpawner` 生成敌人
- 波次间隔：默认 2 分钟（`waveSpacing = 2 * 60 * 60`）
- 威胁等级影响敌人数量和类型
- 最后一波后可能触发入侵

### 入侵系统
- 已占领扇区可能被敌人入侵
- `baseInvasionChance = 1/100`（每回合 1% 概率）
- `invasionGracePeriod = 20分钟`（占领后 20 分钟内不会被入侵）
- `turnDuration = 2分钟`（每回合时长）

### 科技树
- `TechTree` 管理所有科技节点
- 节点需要资源和前置条件
- 解锁后永久保存（跨存档）

## Vars 中的战役相关常量

```java
// 关键常量（来自 Vars.java）
tilesize = 8;                          // 瓦片尺寸
maxBlockSize = 16;                     // 最大方块尺寸
mineTransferRange = 220f;              // 自动传输范围
buildingRange = 220f;                  // 建造范围
itemTransferRange = 220f;              // 物品传输范围
turnDuration = 2 * Time.toMinutes;     // 回合时长（2分钟）
baseInvasionChance = 1f / 100f;        // 入侵概率（1%）
invasionGracePeriod = 20;              // 入侵保护期（分钟）
minArmorDamage = 0.1f;                 // 最小护甲伤害（10%）
```

## 关联
- [World System](world-system.md) — 世界坐标和瓦片系统
- [Save and Load System](save-load-system.md) — 存档格式和加载
- [Research and Tech Tree System](research-tech-tree-system.md) — 科技解锁
- [Vars Game](vars-game.md) — 游戏状态全局变量
