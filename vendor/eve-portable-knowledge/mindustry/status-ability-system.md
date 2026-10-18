# Mindustry 状态效果与能力系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/type/StatusEffect.java` (256行), `源码和示例/Mindustry/core/src/mindustry/entities/abilities/Ability.java` (63行), `源码和示例/Mindustry/core/src/mindustry/entities/abilities/*.java` (17个)

## StatusEffect 字段全表

### 倍率乘数（全部默认 1f）
| 字段 | 类型 | 说明 |
|------|------|------|
| damageMultiplier | float | 伤害倍率 |
| healthMultiplier | float | 生命倍率 |
| speedMultiplier | float | 速度倍率 |
| reloadMultiplier | float | 装填倍率 |
| buildSpeedMultiplier | float | 建造速度倍率 |
| dragMultiplier | float | 阻力倍率 |

### 伤害
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| damage | float | 0 | 每帧伤害 |
| intervalDamageTime | float | 0 | 间隔伤害周期(ticks)，<=0禁用 |
| intervalDamage | float | 0 | 间隔伤害值 |
| intervalDamagePierce | boolean | false | 间隔伤害是否穿甲 |
| transitionDamage | float | 0 | 亲和转化时一次性伤害 |

### 布尔标志
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| disarm | boolean | false | 缴械 |
| permanent | boolean | false | 永不消失 |
| reactive | boolean | false | 只反应不可施加 |
| dynamic | boolean | false | 特殊动态效果（内部用） |
| show | boolean | true | 数据库中显示 |
| parentizeEffect | boolean | false | 随机特效是否跟随父级 |
| applyExtend | boolean | false | 已有时是否仍显示施加特效 |
| parentizeApplyEffect | boolean | false | 施加特效是否跟随父级 |
| outline | boolean | true | 是否生成轮廓 |

### 视觉
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| color | Color | white | 着色 |
| effect | Effect | Fx.none | 随机特效 |
| applyEffect | Effect | Fx.none | 施加时特效 |
| applyColor | Color | white | 施加特效颜色 |
| effectChance | float | 0.15f | 特效出现概率 |

### 关系
| 字段 | 类型 | 说明 |
|------|------|------|
| affinities | ObjectSet\<StatusEffect\> | 亲和列表 |
| opposites | ObjectSet\<StatusEffect\> | 对立列表 |
| transitions | ObjectMap\<StatusEffect, TransitionHandler\> | 转化处理映射 |

## Ability 系统

### Ability 基类字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| display | boolean | true | 是否在统计中显示 |
| data | float | 0 | 唯一同步字段 |

### Ability 生命周期
```
created(unit) → init(type) → update(unit)每帧 → draw(unit)每帧 → death(unit)
```

### 原版17个Ability
| Ability | 核心字段 | 说明 |
|---------|----------|------|
| ArmorPlateAbility | armorMultiplier, displayStats | 护甲倍率 |
| EnergyFieldAbility | damage, hitSize, hitBuildings, hitUnits, maxTargets, cooldown, length, status, statusDuration | 能量场伤害（Aegires用） |
| ForceFieldAbility | radius, regen, max, cooldownBrokenBase, cooldownTime | 力场护盾 |
| LiquidExplodeAbility | liquid, amount, radDamageMax | 液体爆炸 |
| LiquidRegenAbility | liquid, amount, multiplier | 液体回血 |
| MoveEffectAbility | effect, color, y, interval, rotateWithParent, parentize | 移动特效 |
| MoveLightningAbility | minSpeed, maxLightnings, effectChance, lightningColor, effect | 移动闪电 |
| RegenAbility | amount | 直接回血 |
| RepairFieldAbility | repairRadius, repairAmount, active | 修复范围 |
| ShieldArcAbility | radius, angle, regen, max, cooldownBrokenBase, cooldownTime | 弧形护盾 |
| ShieldRegenFieldAbility | shieldRange, shieldAmount, reload | 护盾回复范围 |
| SpawnDeathAbility | unit, spawnAmount | 死亡时生成单位 |
| StatusFieldAbility | status, duration, reload, range, applyEffect, activeEffect | 状态场 |
| SuppressionFieldAbility | range, reload, duration, buildingDamageMultiplier, hitBuildings, hitUnits | 压制场 |
| UnitSpawnAbility | unit, spawnTime, spawnX, spawnY | 持续生成单位 |
| EmptyDataAbility | 无 | 空数据占位 |

## JSON 中使用 StatusEffect

```json
{
  "type": "StatusEffect",
  "name": "my-effect",
  "damage": 0.5,
  "speedMultiplier": 0.8,
  "color": "ff0000",
  "effect": "burning",
  "permanent": false
}
```

## JSON 中使用 Ability

```json
{
  "type": "UnitType",
  "abilities": [
    {
      "type": "StatusFieldAbility",
      "status": "shielded",
      "duration": 120,
      "reload": 60,
      "range": 80
    },
    {
      "type": "RegenAbility",
      "amount": 0.5
    }
  ]
}
```

## 硬性规则

1. **StatusEffect 继承 UnlockableContent** — 不是 Content，有数据库显示控制
2. **所有倍率默认 1f** — 不是 0 或其他值，1f 表示无效果
3. **intervalDamageTime 单位是 ticks** — <=0 禁用间隔伤害
4. **effectChance 默认 0.15f** — 不是 1.0
5. **reactive=true 只能反应不可施加** — 用于亲和系统
6. **permanent=true 永不消失** — 没有持续时间概念
7. **Ability 的 data 是唯一同步字段** — 用于联网同步
8. **Ability.display=false 隐藏统计** — 不影响实际效果
9. **EnergyFieldAbility 不是 ForceFieldAbility** — 前者伤害范围，后者护盾
10. **ShieldArcAbility 是弧形** — 有 angle 字段，不是全圆
11. **UnitSpawnAbility 死亡时生成用 SpawnDeathAbility** — 不是同一个
12. **StatusFieldAbility 和 SuppressionFieldAbility** — 前者施加状态，后者压制（禁用修复/建造）
13. **Ability 是 Cloneable** — copy() 返回 clone
14. **allDatabaseTabs = true** — StatusEffect 默认在所有数据库标签页显示

## 关联
- [Unit System](unit-system.md)
- [Block Building System](block-building-system.md)
- [Events System](events-system.md)
