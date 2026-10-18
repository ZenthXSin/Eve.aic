# Mindustry Rules 系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/game/Rules.java` 直接验证，2026-06-11

## 游戏模式判断

```java
public Gamemode mode() {
    if(pvp) return Gamemode.pvp;
    if(editor) return Gamemode.editor;
    if(attackMode) return Gamemode.attack;
    if(infiniteResources) return Gamemode.sandbox;
    return Gamemode.survival;
}
```

## 核心规则字段

### 波次系统

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `waves` | false | 是否启用波次 |
| `waveTimer` | true | 波次是否自动计时 |
| `waveSending` | true | 是否允许手动召唤波次 |
| `waveSpacing` | 2 min | 波次间隔（tick） |
| `initialWaveSpacing` | 0f | 初始波次间隔（0=使用 waveSpacing*2） |
| `winWave` | 0 | 获胜波次（≤0=禁用） |
| `waitEnemies` | false | 等待敌人消灭后才开始计时 |

### 队伍设置

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `defaultTeam` | Team.sharded | 玩家默认队伍 |
| `waveTeam` | Team.crux | 敌方波次队伍 |
| `pvp` | false | PvP 模式 |
| `pvpAutoPause` | true | PvP 自动暂停 |
| `attackMode` | false | 攻击模式 |
| `editor` | false | 编辑器模式 |

### 资源与建造

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `infiniteResources` | false | 无限资源 |
| `buildCostMultiplier` | 1f | 建造成本倍率 |
| `buildSpeedMultiplier` | 1f | 建造速度倍率 |
| `deconstructRefundMultiplier` | 0.5f | 拆除返还比例 |
| `instantBuild` | false | 即时建造（实验性） |
| `loadout` | 100 copper | 初始物品 |

### 单位系统

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `unitCap` | 0 | 基础单位上限 |
| `disableUnitCap` | false | 禁用单位上限 |
| `unitCapVariable` | true | 核心增加单位上限 |
| `unitBuildSpeedMultiplier` | 1f | 单位建造速度 |
| `unitCostMultiplier` | 1f | 单位成本倍率 |
| `unitDamageMultiplier` | 1f | 单位伤害倍率 |
| `unitHealthMultiplier` | 1f | 单位生命倍率 |
| `unitCrashDamageMultiplier` | 1f | 坠毁伤害倍率 |
| `unitMineSpeedMultiplier` | 1f | 采矿速度倍率 |
| `unitFactoryActivationDelay` | 0f | 工厂激活延迟 |

### 方块系统

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `blockHealthMultiplier` | 1f | 方块生命倍率 |
| `blockDamageMultiplier` | 1f | 方块伤害倍率 |
| `reactorExplosions` | true | 反应堆爆炸 |
| `damageExplosions` | true | 友军爆炸伤害 |
| `fire` | true | 火灾启用 |
| `ghostBlocks` | true | 幽灵方块 |
| `derelictRepair` | true | 废弃方块可修复 |

### 逻辑系统

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `logicUnitControl` | true | 逻辑控制单位 |
| `logicUnitBuild` | true | 逻辑建造 |
| `logicUnitDeconstruct` | false | 逻辑拆除 |
| `worldProcessorPlayerLink` | true | 世界处理器可链接玩家建筑 |
| `allowEditWorldProcessors` | false | 允许编辑世界处理器 |
| `disableWorldProcessors` | false | 禁用世界处理器 |
| `allowLogicData` | false | 允许 data 指令 |

### 视觉与环境

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `env` | terrestrial\|spores\|... | 环境标志 |
| `fog` | false | 战争迷雾 |
| `staticFog` | true | 静态迷雾 |
| `lighting` | false | 环境光照 |
| `ambientLight` | (0.01,0.01,0.04,0.99) | 环境光颜色 |
| `borderDarkness` | true | 边缘黑暗 |
| `solarMultiplier` | 1f | 太阳能倍率 |

### 核心保护

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `enemyCoreBuildRadius` | 400f | 敌方核心禁建半径 |
| `polygonCoreProtection` | false | 多边形核心保护 |
| `placeRangeCheck` | false | 放置范围检查 |
| `protectCores` | true | TeamRule 中的核心保护 |
| `coreCapture` | false | 核心被摧毁后转换队伍 |
| `coreIncinerates` | true | 核心满时焚烧物品 |
| `coreDestroyClear` | false | 核心死亡时清除周围敌方方块 |

### 其他重要字段

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `dropZoneRadius` | 300f | 生成区域半径 |
| `hideSpawns` | true | 隐藏生成点 |
| `hideBannedBlocks` | false | 隐藏被禁方块 |
| `blockWhitelist` | false | 禁用列表变白名单 |
| `unitWhitelist` | false | 禁用列表变白名单 |
| `onlyDepositCore` | false | 只能存入核心 |
| `allowCoreUnloaders` | true | 允许核心卸载器 |
| `itemDepositCooldown` | 0.5f | 物品存入冷却（秒） |
| `planet` | Planets.serpulo | 星球 |
| `sector` | null | 战役区块 |
| `tags` | StringMap | 自定义标签 |

## TeamRule（每队伍规则）

```java
public static class TeamRule {
    public boolean aiCoreSpawn = true;        // AI 从核心生成
    public boolean protectCores = true;       // 核心保护
    public boolean checkPlacement = true;     // 放置检查
    public boolean cheat;                     // 作弊模式
    public boolean fillItems;                 // 核心填满物品
    public boolean infiniteResources;         // 无限资源
    public boolean prebuildAi;               // 预建造 AI（实验性）
    public boolean buildAi;                  // 建造 AI
    public float buildAiTier = 1f;           // AI 等级 [0,1]
    public boolean rtsAi;                    // RTS AI
    public int rtsMinSquad = 4;              // 最小攻击小队
    public int rtsMaxSquad = 50;             // 最大小队
    public float rtsMinWeight = 1.2f;        // 最小攻击权重
    
    // 倍率（与全局 Rules 相乘）
    public float unitBuildSpeedMultiplier = 1f;
    public float unitDamageMultiplier = 1f;
    public float unitHealthMultiplier = 1f;
    public float blockHealthMultiplier = 1f;
    public float blockDamageMultiplier = 1f;
    public float buildSpeedMultiplier = 1f;
    public float extraCoreBuildRadius = 0f;
}
```

## 倍率计算方法

```java
// 最终值 = 全局倍率 × 队伍倍率
public float unitHealth(Team team) {
    return Math.max(unitHealthMultiplier * teams.get(team).unitHealthMultiplier, 0.000001f);
}

public float buildRadius(Team team) {
    return !teams.get(team).protectCores ? 0f : 
        enemyCoreBuildRadius + teams.get(team).extraCoreBuildRadius;
}
```

## 常见误区

- **waveSpacing 默认 2 分钟**：不是 1 分钟
- **defaultTeam 是 Team.sharded**：不是 Team.blue
- **waveTeam 是 Team.crux**：不是 Team.red
- **deconstructRefundMultiplier 默认 0.5f**：返还 50%
- **enemyCoreBuildRadius 默认 400f**：不是 200
- **unitHealth 有最小值保护**：不会是 0
- **TeamRule 的倍率与全局相乘**：不是覆盖
- **derelict 的 protectCores=false**：废弃队伍无保护
- **mode() 按优先级判断**：pvp > editor > attack > sandbox > survival
- **loadout 默认 100 铜**：不是 0

## Related

- [Mindustry GameState/Rules](mindustry-content-loading-gamestate-rules-save.md)
- [Mindustry Team System](mindustry-item-team-attribute-schematic-techtree.md)
