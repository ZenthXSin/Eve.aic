# Mindustry 单位系统源码验证

> Source: `mindustry/type/UnitType.java` + `mindustry/type/Weapon.java`，2026-06-08

## UnitType 继承链（源码验证）

```
UnlockableContent
  └── UnitType          # 单位类型定义（2065行）
        ├── 单位实体类（通过 constructor 指定）
        │   ├── Unit           # 基础单位实体
        │   ├── MechUnit       # 机甲单位
        │   ├── TankUnit       # 坦克单位
        │   ├── NavalUnit      # 海军单位
        │   ├── PayloadUnit    # 载荷单位
        │   ├── BuilderUnit    # 建造者单位
        │   ├── MinerUnit      # 矿工单位
        │   └── ...            # 其他单位实体
        └── 单位类型子类
            ├── BlockUnitType      # 方块单位
            ├── MissileUnitType    # 导弹单位
            ├── PayloadUnitType    # 载荷单位
            └── ...                # 其他类型
```

## UnitType 核心字段（源码验证）

### 环境与移动
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `envRequired` | int | 0 | 必需的环境标志 |
| `envEnabled` | int | Env.terrestrial | 允许的环境 |
| `envDisabled` | int | Env.scorching | 禁用的环境 |
| `speed` | float | 1.1f | 移动速度（世界单位/帧） |
| `boostMultiplier` | float | 1f | 加速时的速度倍率 |
| `rotateSpeed` | float | 5f | 机体旋转速度（度/帧） |
| `drag` | float | 0.3f | 移动阻力 |
| `accel` | float | 0.5f | 加速度（速度的比例） |
| `hitSize` | float | 6f | 碰撞箱大小 |
| `strafePenalty` | float | 0.5f | 反向移动惩罚 |

### 生存属性
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `health` | float | 200f | 生命值 |
| `armor` | float | 0f | 护甲（减少伤害） |
| `crashDamageMultiplier` | float | 1f | 坠毁伤害倍率 |
| `wreckHealthMultiplier` | float | 0.25f | 残骸生命倍率 |
| `drownTimeMultiplier` | float | 1f | 溺水时间倍率 |

### 范围与能力
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `range` | float | -1 | 最小武器范围（-1=自动） |
| `maxRange` | float | -1f | 最大武器范围 |
| `mineRange` | float | 70f | 采矿范围 |
| `buildRange` | float | Vars.buildingRange | 建造范围 |
| `mineTier` | int | -1 | 可采矿硬度（-1=禁用） |
| `mineSpeed` | float | 1f | 采矿速度 |
| `buildSpeed` | float | -1f | 建造速度（-1=禁用） |
| `itemCapacity` | int | -1 | 物品容量（-1=按hitSize） |
| `payloadCapacity` | float | 8 | 载荷容量（世界单位^2） |

### 布尔标志（默认值重要！）
| 字段 | 默认值 | 说明 |
|------|--------|------|
| `flying` | **false** | 是否飞行 |
| `isEnemy` | true | 是否在波次计数器中 |
| `wobble` | true | 飞行时是否晃动 |
| `targetAir` | true | 是否攻击空中 |
| `targetGround` | true | 是否攻击地面 |
| `faceTarget` | true | 射击时是否面向目标 |
| `circleTarget` | false | 是否环绕目标（轰炸机） |
| `canBoost` | false | 是否可加速 |
| `logicControllable` | true | 逻辑控制 |
| `playerControllable` | true | 玩家控制 |
| `hittable` | true | 可被命中 |
| `killable` | true | 可被杀死 |
| `targetable` | true | 可被锁定 |
| `canDrown` | true | 地面单位会溺水 |
| `useUnitCap` | true | 计入单位上限 |
| `createWreck` | true | 死亡产生残骸 |
| `createScorch` | true | 死亡产生焦痕 |
| `hovering` | false | 是否悬浮（忽略地形） |
| `omniMovement` | true | 全向移动 |
| `canHeal` | false | 是否可治疗（init中设置） |
| `singleTarget` | false | 所有武器攻击同一目标 |
| `canAttack` | true | 是否可攻击 |
| `naval` | false | 是否海军（init中设置） |
| `drawCell` | true | 绘制团队颜色 |
| `drawItems` | true | 绘制携带物品 |
| `drawShields` | true | 绘制护盾 |
| `drawBody` | true | 绘制本体 |
| `drawSoftShadow` | true | 绘制软阴影 |

### 集合字段
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `abilities` | Seq<Ability> | [] | 能力列表 |
| `weapons` | Seq<Weapon> | [] | 武器列表 |
| `immunities` | ObjectSet<StatusEffect> | [] | 免疫状态 |
| `commands` | Seq<UnitCommand> | [] | 可用命令 |
| `stances` | Seq<UnitStance> | [] | 可用姿态 |

## Weapon 核心字段（源码验证）

### 武器属性
| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `name` | String | "" | 武器名称 |
| `bullet` | BulletType | placeholder | 子弹类型 |
| `mirror` | boolean | true | 是否镜像（自动创建副本） |
| `alternate` | boolean | true | 镜像时是否交替射击 |
| `rotate` | boolean | false | 是否独立旋转 |
| `top` | boolean | true | 是否绘制在顶层 |
| `continuous` | boolean | false | 是否持续射击 |
| `alwaysContinuous` | boolean | false | 是否永远持续（无需装填） |
| `reload` | float | 1 | 装填时间（帧） |
| `inaccuracy` | float | 0f | 散射角度 |
| `recoil` | float | 1.5f | 视觉后坐力 |
| `shootX` | float | 0f | 射击点X偏移 |
| `shootY` | float | 3f | 射击点Y偏移 |
| `x` | float | 5f | 武器位置X偏移 |
| `y` | float | 0f | 武器位置Y偏移 |
| `shootCone` | float | 5f | 射击锥角 |
| `rotationLimit` | float | 361f | 旋转限制 |
| `rotateSpeed` | float | 20f | 旋转速度（度/帧） |
| `layerOffset` | float | 0f | 绘制层级偏移 |

### 布尔标志
| 字段 | 默认值 | 说明 |
|------|--------|------|
| `display` | true | 是否在统计中显示 |
| `flipSprite` | false | 是否翻转精灵图 |
| `controllable` | true | 玩家可控制 |
| `aiControllable` | true | AI可控制 |
| `alwaysShooting` | false | 是否总是射击 |
| `autoTarget` | false | 自动锁定目标 |
| `predictTarget` | true | 预测目标位置 |
| `useAttackRange` | true | 用于计算攻击范围 |
| `ignoreRotation` | false | 忽略射击者旋转 |
| `noAttack` | false | 不能攻击 |
| `shootOnDeath` | false | 死亡时射击 |
| `parentizeEffects` | false | 特效跟随单位 |

## 单位类型（JSON type 字段）

| type | 对应类 | 说明 |
|------|--------|------|
| `MechUnit` | MechUnit | 机甲单位 |
| `TankUnit` | TankUnit | 坦克单位 |
| `NavalUnit` | NavalUnit | 海军单位 |
| `PayloadUnit` | PayloadUnit | 载荷单位 |
| `BuilderUnit` | BuilderUnit | 建造者单位 |
| `MinerUnit` | MinerUnit | 矿工单位 |
| `BlockUnitType` | BlockUnitType | 方块单位 |
| `MissileUnitType` | MissileUnitType | 导弹单位 |

## UnitType.init() 自动计算（源码验证）

```java
// 自动设置 naval 属性
if(example instanceof WaterMovec || example instanceof WaterCrawlc){
    naval = true;
    canDrown = false;
    emitWalkSound = false;
    omniMovement = false;
    immunities.add(StatusEffects.wet);
}

// 飞行单位自动启用太空环境
if(flying){
    envEnabled |= Env.space;
}

// itemCapacity 自动计算
if(itemCapacity < 0){
    itemCapacity = Math.max(Mathf.round((int)(hitSize * 4f), 10), 10);
}

// range 自动计算（从武器范围取最小值）
if(range < 0){
    range = Float.MAX_VALUE;
    for(Weapon weapon : weapons){
        if(!weapon.useAttackRange) continue;
        range = Math.min(range, weapon.range() - 4f);
        maxRange = Math.max(maxRange, weapon.range() - 4f);
    }
}

// 无武器时 range 使用 mineRange
if(!weapons.contains(w -> w.useAttackRange)){
    if(range < 0 || range == Float.MAX_VALUE) range = mineRange;
    if(maxRange < 0 || maxRange == Float.MAX_VALUE) maxRange = mineRange;
}
```

## Weapon 关键默认值（源码验证）

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `bullet` | Bullets.placeholder | 子弹类型（无伤害） |
| `mirror` | true | 自动镜像 |
| `alternate` | true | 交替射击 |
| `rotate` | false | 独立旋转 |
| `continuous` | false | 持续射击 |
| `reload` | 1f | 装填时间（帧） |
| `shootCone` | 5f | 射击锥角 |
| `rotateSpeed` | 20f | 旋转速度 |
| `recoil` | 1.5f | 后坐力 |
| `x` | 5f | 武器X偏移 |
| `y` | 0f | 武器Y偏移 |
| `shootX` | 0f | 射击点X |
| `shootY` | 3f | 射击点Y |
| `shoot.firstShotDelay` | 0f | 首发延迟 |
| `shoot.shotDelay` | 0f | 间隔延迟 |
| `shoot.shots` | 1 | 每次子弹数 |

## JSON 常见错误

1. **type 不存在** → 回退到基础 UnitType
2. **weapons 缺少 bullet** → 使用 placeholder（无伤害）
3. **mirror=true 但手动写了两个武器** → 创建4个武器实例
4. **hitSize 过大** → 碰撞问题
5. **mineTier=-1 但设置了采矿AI** → 无法采矿
6. **flying=false 但设置了飞行AI** → 地面单位用飞行AI
7. **range=-1 但没有武器** → 使用 mineRange（70f）
8. **itemCapacity=-1** → 自动计算为 hitSize*4（四舍五入到10）
9. **buildSpeed=-1** → 禁用建造（不是自动计算）
10. **shoot.shots 和 shoot.firstShotDelay** → 不是 Weapon 直接字段，是 shoot 子对象字段

## 硬性规则

1. `health` 默认 200f，不是 100
2. `hitSize` 默认 6f，不是 8
3. `speed` 默认 1.1f
4. `range` 默认 -1（自动从武器计算）
5. `itemCapacity` 默认 -1（自动计算为 hitSize*4）
6. `buildSpeed` 默认 -1f（禁用建造，不是自动计算）
7. `flying` 默认 false
8. `targetAir`/`targetGround` 默认 true
9. `Weapon.shootCone` 默认 5f（不是 Turret 的 8f）
10. `Weapon.reload` 默认 1f（不是 Turret 的 0f）
11. `Weapon.mirror` 默认 true（自动创建镜像副本）
12. `Weapon.bullet` 默认 placeholder（无伤害，必须手动设置）

## 坐标换算关系（源码验证）

| 换算 | 公式 | 来源 |
|------|------|------|
| 1 tile = 8 世界坐标 | `Tile.worldx() = x * tilesize` | `Tile.java:162`, `Vars.java:135` |
| 1 tile = 32 像素 | `tilesize(8) * Draw.xscl默认(4f)` | 贴图绘制时的像素换算 |
| 1 世界坐标 = 4 像素 | `8 * 4 / 8 = 4` | 推导 |
| 8 hitSize = 1 tile | `hitSize / tilesize` 计算占格数 | `UnitType.java:791` |
| 1 hitSize = 1 像素 | `tilesize(8) / 8 = 1` | 推导（贴图绘制场景） |

**关键结论：**
- 世界坐标以 tile 为单位，1 tile = 8 世界坐标
- 贴图绘制时 `Draw.xscl` 默认 4f，所以 1 tile = 8 * 4 = 32 像素
- 单位绘制 `Draw.rect(region, unit.x, unit.y)` 中 `unit.x/unit.y` 是世界坐标（tile单位）
- 最终像素尺寸 = `region.width * region.scl() * Draw.xscl`

## 验证方法

```java
// 检查单位类型
UnitType type = content.unit("unit-name");
Log.info("Flying: " + type.flying);
Log.info("Weapons: " + type.weapons.size);
Log.info("HitSize: " + type.hitSize);

// 检查武器
for (Weapon w : type.weapons) {
    Log.info("Weapon: " + w.name + " Reload: " + w.reload);
    Log.info("Bullet: " + w.bullet.type);
}
```
