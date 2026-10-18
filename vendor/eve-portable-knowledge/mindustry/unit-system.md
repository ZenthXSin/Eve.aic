# Mindustry 单位系统源码验证

> Source: `源码和示例/Mindustry/core/src/mindustry/type/UnitType.java` (2065行)

## UnitType 继承
```
Content → MappableContent → UnlockableContent → UnitType
```

## 环境字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| envRequired | int | 0 | 必需环境标志（0=任意） |
| envEnabled | int | Env.terrestrial | 可用环境 |
| envDisabled | int | Env.scorching | 禁用环境 |

## 移动字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| speed | float | 1.1f | 移动速度（世界单位/tick） |
| boostMultiplier | float | 1f | 推进速度倍率 |
| floorMultiplier | float | 1f | 地形影响倍率 |
| rotateSpeed | float | 5f | 旋转速度（度/tick） |
| baseRotateSpeed | float | 5f | 机甲基座旋转速度 |
| drag | float | 0.3f | 移动阻力 |
| accel | float | 0.5f | 加速度（速度的分数） |
| hitSize | float | 6f | 碰撞箱大小 |
| strafePenalty | float | 0.5f | 反向移动惩罚 |

## 生命与防御
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| health | float | 200f | 生命值 |
| armor | float | 0f | 护甲（减少伤害） |
| deathShake | float | -1f | 死亡震动 |
| crashDamageMultiplier | float | 1f | 坠毁伤害倍率 |
| wreckHealthMultiplier | float | 0.25f | 残骸生命倍率 |
| drownTimeMultiplier | float | 1f | 溺亡速度倍率 |

## 范围字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| range | float | -1 | 最小范围（-1=自动） |
| maxRange | float | -1f | 最大武器范围 |
| mineRange | float | 70f | 采矿范围 |
| buildRange | float | Vars.buildingRange | 建造范围 |
| circleTargetRadius | float | 80f | 环绕目标半径 |

## 载荷与容量
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| payloadCapacity | float | 8 | 载荷容量（世界单位²） |
| itemCapacity | int | -1 | 物品容量（-1=按体型计算） |
| ammoCapacity | int | 1 | 弹药容量 |

## 采矿字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| mineTier | int | -1 | 采矿等级 |
| mineSpeed | float | 1f | 采矿速度 |
| mineWalls | boolean | false | 是否采矿墙 |
| mineFloor | boolean | true | 是否采矿地板 |
| mineHardnessScaling | boolean | true | 硬度影响采矿速度 |
| mineItems | Seq\<Item\> | [copper,lead,titanium,thorium] | 可采矿物 |

## 腿部字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| legCount | int | 4 | 腿数量 |
| legGroupSize | int | 2 | 腿组大小 |
| legLength | float | 10f | 腿长度 |
| legBaseUnder | boolean | false | 腿基座是否在下方 |
| lockLegBase | boolean | false | 锁定腿基座 |
| legContinuousMove | boolean | false | 连续移动 |
| flipBackLegs | boolean | true | 翻转后腿 |
| flipLegSide | boolean | false | 翻转腿侧 |

## 机甲字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| mechLandShake | float | 0f | 着陆震动 |
| mechSideSway | float | 0.54f | 侧向摆动 |
| mechFrontSway | float | 0.1f | 前后摆动 |
| mechStride | float | -1f | 步幅（-1=自动） |
| mechStepParticles | boolean | false | 脚步粒子 |
| mechLegColor | Color | Pal.darkMetal | 腿部颜色 |

## 蛇形单位字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| segments | int | 0 | 节数（0=非蛇形） |
| segmentUnits | int | 1 | 每节单位数 |
| segmentLayerOrder | boolean | true | 节绘制层顺序 |
| segmentMag | float | 2f | 节摆动幅度 |

## 导弹字段
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| lifetime | float | 300f | 生存时间（ticks） |
| homingDelay | float | 10f | 追踪延迟 |
| missileAccelTime | float | 0f | 加速时间 |

## 布尔标志（关键，默认值）

### 核心标志
| 字段 | 默认 | 说明 |
|------|------|------|
| isEnemy | true | 波次计数中算作敌人 |
| flying | false | 是否飞行单位 |
| wobble | true | 飞行时是否晃动 |
| naval | false（init中自动设置） | 是否海军单位 |

### 战斗标志
| 字段 | 默认 | 说明 |
|------|------|------|
| targetAir | true | 攻击空中目标 |
| targetGround | true | 攻击地面目标 |
| faceTarget | true | 面向目标 |
| circleTarget | false | 环绕目标（轰炸机） |
| autoDropBombs | false | 自动投弹 |
| singleTarget | false | 所有武器攻击同一目标 |
| forceMultiTarget | false | 强制多目标 |
| canAttack | true | 可攻击 |
| alwaysShootWhenMoving | false | 移动时总是射击 |

### 控制标志
| 字段 | 默认 | 说明 |
|------|------|------|
| canBoost | false | 可推进 |
| boostWhenBuilding | true | 建造时推进 |
| boostWhenMining | true | 采矿时推进 |
| logicControllable | true | 逻辑可控制 |
| playerControllable | true | 玩家可控制 |
| controlSelectGlobal | true | 全局选择热键 |
| allowChangeCommands | true | 允许切换指令 |

### 物理标志
| 字段 | 默认 | 说明 |
|------|------|------|
| allowedInPayloads | true | 可进入载荷 |
| hittable | true | 可被子弹命中 |
| killable | true | 可被杀死 |
| targetable | true | 可被瞄准 |
| vulnerableWithPayloads | false | 有载荷时可被攻击 |
| pickupUnits | true | 可拾取单位 |
| physics | true | 物理碰撞 |
| canDrown | true | 可溺亡 |
| useUnitCap | true | 受单位上限限制 |
| bounded | true | 被地图边界推开 |
| hovering | false | 悬浮（不受地面影响） |

### 移动标志
| 字段 | 默认 | 说明 |
|------|------|------|
| omniMovement | true | 全向移动 |
| rotateMoveFirst | false | 先旋转再移动 |
| allowLegStep | false | 腿单位可踩方块 |
| legPhysicsLayer | true | 腿物理层 |

### 视觉标志
| 字段 | 默认 | 说明 |
|------|------|------|
| createWreck | true | 死亡产生残骸 |
| createScorch | true | 死亡产生焦痕 |
| lowAltitude | false | 低空绘制 |
| rotateToBuilding | true | 建造时面向建筑 |
| healFlash | true | 治疗闪烁 |
| canHeal | false（init中设置） | 可治疗 |
| hidden | false | 数据库中隐藏 |
| internal | false | 内部使用 |
| squareShape | false | 方形阴影 |

### 绘制标志
| 字段 | 默认 | 说明 |
|------|------|------|
| drawBuildBeam | true | 绘制建造光束 |
| drawMineBeam | true | 绘制采矿光束 |
| drawCell | true | 绘制细胞/队伍颜色 |
| drawItems | true | 绘制携带物品 |
| drawShields | true | 绘制护盾 |
| drawBody | true | 绘制身体 |
| drawSoftShadow | true | 绘制软阴影 |
| drawMinimap | true | 小地图显示 |

## AI 控制器
| 字段 | 默认 | 说明 |
|------|------|------|
| aiController | GroundAI/FlyingAI | 默认AI（按flying选择） |
| controller | CommandAI/AIController | 实际控制器函数 |

controller 逻辑：
- !playerControllable || (team.isAI() && !team.rules().rtsAi) → aiController
- 否则 → CommandAI

## 武器与能力
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| weapons | Seq\<Weapon\> | empty | 武器列表 |
| abilities | Seq\<Ability\> | empty | 能力列表 |
| immunities | ObjectSet\<StatusEffect\> | empty | 状态免疫 |

## 硬性规则

1. **flying 默认 false** — 不是 true，大多数单位是地面单位
2. **isEnemy 默认 true** — 支援单位设为 false
3. **targetAir/targetGround 都默认 true** — 同时空地攻击
4. **canBoost 默认 false** — 需要手动开启
5. **logicControllable/playerControllable 都默认 true** — 同时支持逻辑和玩家
6. **hittable/killable/targetable 都默认 true** — 三个独立控制
7. **physics 默认 true** — 物理碰撞开启
8. **canDrown 默认 true** — 地面单位会溺亡
9. **useUnitCap 默认 true** — 受单位上限限制
10. **omniMovement 默认 true** — 全向移动（不是只能前后）
11. **hovering 默认 false** — 受地面影响
12. **naval 在 init() 中自动设置** — 不要手动赋值
13. **canHeal 在 init() 中自动设置** — 基于武器是否有HealBullet
14. **itemCapacity=-1 按体型自动计算** — 不是固定值
15. **range=-1 自动取武器最小范围** — 不是固定值
16. **aiController 默认按 flying 选择** — GroundAI vs FlyingAI
17. **controller 区分玩家/AI** — 通过 playerControllable 和 team.isAI()
18. **segments=0 表示非蛇形** — 不是1
19. **legCount 默认 4** — 蜘蛛型
20. **mechStride=-1 自动计算** — 不是固定值

## 关联
- [Unit System](unit-system.md)
- [Weapon System](../concepts/mindustry-weapon-system.md)
- [AI System](ai-system.md)
- [Status and Ability System](status-ability-system.md)

## 关键默认值（源码验证）

### UnitType 字段
| 字段 | 默认值 | 说明 |
|------|--------|------|
| `speed` | 1.1f | 移动速度 |
| `health` | 200f | 生命值 |
| `armor` | 0f | 护甲 |
| `hitSize` | 6f | 碰撞箱大小 |
| `rotateSpeed` | 5f | 旋转速度 |
| `range` | -1 | 自动计算 |
| `mineRange` | 70f | 采矿范围 |
| `buildRange` | Vars.buildingRange | 建造范围 |

### 常见误区
- ❌ `range` 需要手动设置 → ✅ 默认-1自动计算
- ❌ `health` 默认100 → ✅ 默认200
- ❌ `hitSize` 默认8 → ✅ 默认6
