# Mindustry 音频系统源码验证

## SoundControl（音乐控制器）

- 路径：`mindustry.audio.SoundControl`
- 控制多音轨播放（背景音乐管理）

### 关键字段

| 字段 | 默认值 | 含义 |
|------|--------|------|
| `finTime` | 120f | 音乐淡入时间（帧） |
| `foutTime` | 120f | 音乐淡出时间（帧） |
| `musicInterval` | 3分钟 | 音乐播放间隔 |
| `musicChance` | 0.8f | 普通音乐播放概率 |
| `musicWaveChance` | 0.46f | 波次后播放音乐概率 |

### 音乐分类

- `ambientMusic` — 环境音乐，随时播放
- `darkMusic` — 暗黑音乐，冲突时播放
- `bossMusic` — Boss 音乐，Boss 生成后播放

### 波次音乐逻辑

- `WaveEvent` 触发后 8~15 秒随机延迟
- 如果有 Boss（`StatusEffects.boss`），播放 bossMusic
- 否则 46% 概率播放随机音乐

### AudioBus

- `uiBus` — UI 音效总线
- 所有 `assets/ui/` 目录下的 Sound 自动绑定到 uiBus

## SoundPriority（音效优先级系统）

- 路径：`mindustry.audio.SoundPriority`
- 管理音效优先级、并发数和分组

### 优先级等级

| 优先级 | 音效 |
|--------|------|
| 7 (最高) | beamPlasma, shootMeltdown, beamMeltdown |
| 3 | acceleratorLaunch, acceleratorCharge, coreLand, coreLaunch |
| 2 | beamMeltdown, beamLustre, beamPlasma, 大爆炸音效 |
| 1.5 | 大武器音效（shootMeltdown, shootSublimate, shootForeshadow 等） |
| 1 | 环境音（conveyor, smelter, drill 等循环音） |
| -1 | 修复音（blockHeal, healWave） |
| -2 (最低) | 脚步音（mechStep 等） |

### 并发控制

- `setMaxConcurrent(n)` — 同时播放最大数
- `setConcurrentGroup(id)` — 同组音效共享并发
- `setMinConcurrentInterrupt(t)` — 最短中断时间
- shootLancer: maxConcurrent=5
- shieldHit: maxConcurrent=4
- 脚步音: maxConcurrent=5，同组共享

### 音效分组

- shootFlame / shootFlamePlasma — 同组
- shootMissile / shootMissileShort / shootMissilePlasmaShort — 同组
- shootArc / shootPulsar — 同组

## SoundLoop（循环音效）

- 路径：`mindustry.audio.SoundLoop`
- 用于持续播放的音效（如传送带、钻机）

## 硬性规则

1. `headless` 模式下所有音效操作直接 return
2. 背景音乐不是每帧检查，有 `musicInterval`（3分钟）间隔
3. Boss 音乐判断依赖 `StatusEffects.boss` 状态效果
4. 音效优先级范围：-2 到 7
5. `musicChance` 不是 100%，只有 80% 概率播放
6. 波次音乐有 8~15 秒延迟，不是波次来了立刻播
7. UI 音效走 `uiBus`，游戏音效走默认 bus
8. `SoundControl` 在 `ClientLoadEvent` 时 reload
9. 爆炸音效有 `setFalloffOffset()` 衰减偏移
10. 循环音效（loop）不等于背景音乐，是方块/单位持续发出的

## 关联

- [Vars Audio](../mindustry/vars-audio.md)
- [Rendering System](../mindustry/rendering-system.md)
- [Status and Ability System](../mindustry/status-ability-system.md)
