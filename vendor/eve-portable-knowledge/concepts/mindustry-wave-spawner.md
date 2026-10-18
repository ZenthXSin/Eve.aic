# Mindustry 波次生成系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/game/SpawnGroup.java` + `Rules.java` + `WaveSpawner.java`
> 验证版本：v146 (本地源码)

## SpawnGroup（生成组）

### 关键字段
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| type | UnitType | UnitTypes.dagger | 单位类型 |
| end | int | Integer.MAX_VALUE | 结束波次 |
| begin | int | 0 | 开始波次 |
| spacing | int | 1 | 每N波出现一次 |
| max | int | 40 | 最大数量上限 |
| unitScaling | float | never | 每波增量（float版） |
| shields | float | 0 | 初始护盾 |
| shieldScaling | float | 0 | 每波护盾增量 |
| unitAmount | int | 1 | 每次生成数量 |
| spawn | int | -1 | 生成点索引（-1=随机） |
| payloads | Seq<UnitType> | null | 载荷（用于攻城单位） |
| items | ItemStack | null | 初始携带物品 |
| abilities | Seq<Ability> | null | 能力 |
| effectiveTier | int | 0 | 有效等级（内部） |

### getSpawned（获取某波次生成数）
```java
int getSpawned(int wave){
    if(wave < begin || (wave - begin) % spacing != 0) return 0;
    int max = this.max;
    if(unitScaling != never) max = (int)(max + (wave - begin) * unitScaling);
    return min(unitAmount * (1 + (wave - begin) / spacing), max);
}
```

### getShield（获取某波次护盾）
```java
float getShield(int wave){
    return shields + shieldScaling * max(0, (wave - begin) / spacing);
}
```

## Rules 波次相关字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| waves | boolean | true | 是否启用波次 |
| waveSending | boolean | true | 是否可手动召唤波次 |
| wavesSpawnAtCores | boolean | true | 攻击模式下在核心生成 |
| attackMode | boolean | false | 攻击模式 |
| waveSpacing | float | 2min | 波次间隔(ticks) |
| initialWaveSpacing | float | 0 | 初始波次间隔(<=0则=waveSpacing*2) |
| winWave | int | 0 | 胜利波次(0=无) |
| unitCostMultiplier | float | 1f | 单位成本倍率 |
| unitDamageMultiplier | float | 1f | 单位伤害倍率 |
| unitCrashDamageMultiplier | float | 1f | 坠毁伤害倍率 |
| blockHealthMultiplier | float | 1f | 方块生命倍率 |
| blockDamageMultiplier | float | 1f | 方块伤害倍率 |
| buildCostMultiplier | float | 1f | 建造成本倍率 |
| buildSpeedMultiplier | float | 1f | 建造速度倍率 |
| deconstructRefundMultiplier | float | 0.5f | 拆除返还比例 |
| enemyCoreBuildRadius | float | 400f | 敌方核心建造禁止半径 |

## 关键规则

| 规则 | 说明 |
|------|------|
| spacing=1 每波出现 | spacing=2 每隔一波 |
| max=40 是数量上限 | 不是每波生成数 |
| unitScaling 是浮点增量 | 不是整数 |
| spawn=-1 随机生成点 | 可指定具体生成点索引 |
| waveSpacing 默认 2 分钟 | 120*60=7200 ticks |
| winWave=0 无胜利条件 | 通常战役设为具体波次 |
| deconstructRefundMultiplier 默认 0.5 | 拆除返还50% |

## JSON 常见误区

| 误区 | 正确 |
|------|------|
| 认为 max 是每波生成数 | max 是最大数量上限 |
| 忘记 begin/end 控制出现范围 | 默认 begin=0, end=MAX_VALUE |
| 认为 spacing=0 有效 | spacing 默认 1，每波出现 |
| unitScaling 当 int 用 | 是 float，支持小数增量 |
| 认为 waves 默认 false | 默认 true |
| waveSpacing 默认 60 ticks | 默认 2 分钟 (7200 ticks) |
