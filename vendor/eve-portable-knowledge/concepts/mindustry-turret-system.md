# Mindustry 炮塔系统源码验证

> Source: `mindustry/world/blocks/defense/turrets/` 全部源码，2026-06-08

## 继承链（源码验证）

```
Block
  └── BaseTurret          # range=80, rotateSpeed=5, coolant, fogRadius
        ├── ReloadTurret    # reload, coolant 逻辑
        │     ├── Turret       # 射击核心（瞄准/射击/弹药管理/绘制）
        │     │     ├── ItemTurret        # 物品弹药（ammoTypes, hasItems=true）
        │     │     ├── PowerTurret       # 电力弹药（shootType, hasPower=true）
        │     │     ├── LiquidTurret      # 液体弹药（ammoTypes, hasLiquids=true）
        │     │     ├── ContinuousTurret  # 持续光束（shootType, 无 reload）
        │     │     │     └── ContinuousLiquidTurret  # 液体+持续光束
        │     │     ├── LaserTurret       # 有限弹药激光（extends PowerTurret）
        │     │     └── PayloadAmmoTurret # 载荷弹药
        │     └── PointDefenseTurret  # 点防御（extends ReloadTurret, 不是 Turret）
        └── TractorBeamTurret  # 牵引光束（extends BaseTurret, 不是 Turret）
```

**易错**：TractorBeamTurret 和 PointDefenseTurret 不继承 Turret，分别继承 BaseTurret 和 ReloadTurret

## Turret 关键字段（源码验证）

### 弹药相关
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `maxAmmo` | int | 30 | 最大弹药量 |
| `ammoPerShot` | int | 1 | 每次射击消耗弹药 |
| `consumeAmmoOnce` | boolean | true | 每次触发只消耗一次弹药（不论子弹数） |

### 射击精度/模式
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `inaccuracy` | float | 0 | 子弹角度随机偏移（度） |
| `velocityRnd` | float | 0 | 速度随机比例 |
| `shootCone` | float | 8 | 允许射击的最大角度差（度） |
| `shootX/shootY` | float | 0, size*tilesize/2 | 射击点偏移 |
| `xRand` | float | 0 | X轴随机散布 |
| `shoot` | ShootPattern | new ShootPattern() | 射击模式（**不是** `shootPattern`） |
| `minWarmup` | float | 0 | 最低预热值才开火 |
| `alwaysShooting` | boolean | false | 有弹药就无条件射击 |

### 目标选择
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `targetAir` | boolean | true | 对空 |
| `targetGround` | boolean | true | 对地 |
| `targetBlocks` | boolean | true | 攻击建筑 |
| `targetHealing` | boolean | false | 治疗友方 |
| `targetUnderBlocks` | boolean | true | 攻击传送带下建筑 |
| `predictTarget` | boolean | true | 预判移动目标 |
| `unitSort` | Sortf | UnitSorts.closest | 目标排序函数 |
| `unitFilter` | Boolf\<Unit\> | u -> true | 单位过滤器 |
| `buildingFilter` | Boolf\<Building\> | ... | 建筑过滤器 |
| `minRange` | float | 0 | 最小射程（仅炮兵） |
| `trackingRange` | float | 0 | 锁定范围（≥range） |

### 热量/充能
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `heatRequirement` | float | -1 | 需要的热量输入（-1=不用） |
| `maxHeatEfficiency` | float | 3 | 最大热量效率倍率 |
| `moveWhileCharging` | boolean | true | 充能时能否移动 |
| `reloadWhileCharging` | boolean | true | 充能时能否装填 |
| `accurateDelay` | boolean | true | 精准延迟射击 |

### 视觉/音效
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `recoil` | float | 1 | 后退视觉量 |
| `recoils` | int | -1 | 多后退计数器 |
| `recoilTime` | float | -1 | 后退回复时间（默认=reload） |
| `recoilPow` | float | 1.8 | 后退曲线指数 |
| `shake` | float | 0 | 屏幕震动 |
| `heatColor` | Color | Pal.turretHeat | 热量覆盖层颜色 |
| `drawer` | DrawBlock | DrawTurret() | 绘制器（**字段名是 drawer**） |
| `shootEffect` | Effect | null | 射击特效覆盖 |
| `smokeEffect` | Effect | null | 烟雾特效覆盖 |
| `ammoUseEffect` | Effect | Fx.none | 弹药消耗特效 |
| `shootSound` | Sound | Sounds.shootDuo | 射击音效 |
| `chargeSound` | Sound | Sounds.none | 充能音效 |
| `loopSound` | Sound | Sounds.none | 循环音效 |

### 构造函数设置（源码验证）
```java
// Turret 构造函数
liquidCapacity = 20;
sync = true;
rotate = true;
quickRotate = false;
drawArrow = false;
ignoreLineRotation = true;
regionRotated1 = 1;
regionRotated2 = 2;

// BaseTurret 构造函数
update = true;
solid = true;
attacks = true;
priority = TargetPriority.turret;
group = BlockGroup.turrets;
flags = EnumSet.of(BlockFlag.turret);
```

## 各子类特有字段

### ItemTurret
```java
ObjectMap<Item, BulletType> ammoTypes = new OrderedMap<>();  // 注意是 OrderedMap
// 构造：hasItems = true
// ammo(Object... objects) 方法：交替 [item1, bullet1, item2, bullet2...]
```

### PowerTurret
```java
BulletType shootType;  // 不是 ammoTypes，是单个 shootType
// 构造：hasPower = true
```

### LiquidTurret
```java
ObjectMap<Liquid, BulletType> ammoTypes = new ObjectMap<>();
boolean extinguish = true;  // 自动灭火
// 构造：hasLiquids = true, loopSound = Sounds.loopSpray
```

### ContinuousTurret
```java
BulletType shootType = Bullets.placeholder;
float aimChangeSpeed = Float.POSITIVE_INFINITY;  // 激光瞄准速度
boolean scaleDamageEfficiency = false;
// 构造：coolantMultiplier = 1f, displayAmmoMultiplier = false
// 无 reload 概念，始终射击
// hasAmmo() 永远返回 true（通过 canConsume 判断）
```

### ContinuousLiquidTurret
```java
ObjectMap<Liquid, BulletType> ammoTypes = new ObjectMap<>();
float liquidConsumed = 1f / 60f;  // 每 tick 消耗液体
// 液体不足时 activated=false 停止射击
```

### TractorBeamTurret（extends BaseTurret, 不是 Turret）
```java
float retargetTime = 5f;
float shootCone = 6f;        // 不是 Turret 的 8f
float shootLength = 5f;
float laserWidth = 0.6f;
float force = 0.3f;
float scaledForce = 0f;
float damage = 0f;
boolean targetAir = true, targetGround = false;  // 注意：默认不对地！
Color laserColor = Color.white;
StatusEffect status = StatusEffects.none;
float statusDuration = 300;
// 无 ammoTypes/shootType，纯力场牵引
```

### PointDefenseTurret（extends ReloadTurret, 不是 Turret）
```java
// 继承 ReloadTurret 的 reload 逻辑
// 无 ammoTypes，使用 shootType 单子弹
// 自动拦截附近子弹
```

### PayloadAmmoTurret
```java
// 载荷弹药，payload 作为弹药消耗
```

## ShootPattern 类型（源码验证）

`mindustry.entities.pattern` 包：
- **ShootPattern** — 基类，`shots`(每次子弹数), `firstShotDelay`(首发延迟), `shotDelay`(间隔延迟)
- **ShootSpread** — 扇形散布，`spread`(度)
- **ShootAlternate** — 交替枪管，`barrels`(枪管数), `spread`(世界单位), `mirror`, `barrelOffset`
- **ShootBarrel** — 从指定枪管射击
- **ShootHelix** — 螺旋模式
- **ShootMulti** — 组合多个 ShootPattern
- **ShootSine** — 正弦曲线
- **ShootSummon** — 召唤式

**JSON 写法**：`"shoot": {"type": "ShootSpread", "shots": 3, "spread": 1}`

## TurretBuild 关键运行时状态

```java
Seq<AmmoEntry> ammo;        // 弹药栈
int totalAmmo;              // 当前弹药总量
float curRecoil;            // 当前后退量
float heat;                 // 当前热量
float shootWarmup;          // 射击预热（0-1）
float charge;               // 充能进度
int totalShots;             // 总射击计数
int barrelCounter;          // 枪管计数器
boolean logicShooting;      // 逻辑控制射击
Posc target;                // 当前目标
Vec2 targetPos;             // 目标位置
boolean wasShooting;        // 上一帧是否射击
```

## 射击流程（updateTile 核心逻辑）

1. 验证目标，失效则清空
2. 更新 warmup（线性或曲线）
3. 更新后退/热量/充能
4. 若 `reloadWhileCharging || !charging()` → 装填+冷却
5. 激活定时器倒计时
6. 有弹药时：
   - 定时寻找目标 (`timerTarget`)
   - 验证目标有效性
   - 玩家控制 → 用 unit.aimX/Y
   - 逻辑控制 → 用 logicShooting
   - AI → targetPosition(target)，判断距离
   - 装填完成且在射击锥内 → `shoot(peekAmmo())`

## JSON 炮塔常见错误

1. **字段名错**：`draw` → 正确是 `drawer`
2. **PowerTurret 用 ammoTypes** → 正确是 `shootType`
3. **ContinuousTurret 指定 reload** → 持续光束无 reload
4. **shoot 类型写错**：`"type": "shootSpread"` → 正确是 `"ShootSpread"`（首字母大写）
5. **ammoTypes 格式错** → 必须是交替数组 `[item1, bullet1, item2, bullet2...]`（Java 调用），JSON 中是对象映射
6. **LiquidTurret 不需要 hasLiquids** → 构造函数已自动设置
7. **ItemTurret 不需要 hasItems** → 构造函数已自动设置
8. **coolant 写在 consumes 里** → 正确，但 BaseTurret.init() 会自动查找 ConsumeCoolant 并设置 booster=true, optional=true

## Turret 关键默认值（源码验证）

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `shootCone` | 8f | 射击锥角 |
| `targetAir` | true | 对空 |
| `targetGround` | true | 对地 |
| `targetBlocks` | true | 对建筑 |
| `playerControllable` | true | 玩家可控 |
| `alwaysShooting` | false | 总是射击 |
| `predictTarget` | true | 预测目标 |
| `recoil` | 1f | 后坐力 |
| `consumeAmmoOnce` | true | 每次射击消耗一次弹药 |

### 常见误区
- ❌ `shootCone` 默认5f → ✅ 默认8f
- ❌ `targetAir`/`targetGround` 需要手动设置 → ✅ 默认true
- ❌ `alwaysShooting` 默认true → ✅ 默认false
