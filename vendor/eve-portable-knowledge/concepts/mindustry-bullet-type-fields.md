# Mindustry BulletType 关键字段默认值（源码验证）

> Source: `core/src/mindustry/entities/bullet/BulletType.java` (900+行), `BasicBulletType.java` (80行), 26个子类

## 碰撞与穿透

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `collides` | **true** | 是否碰撞任何东西 |
| `collidesAir` | **true** | 是否碰撞空中单位 |
| `collidesGround` | **true** | 是否碰撞地面单位 |
| `collidesTiles` | **true** | 是否碰撞建筑 |
| `collidesTeam` | **false** | 是否碰撞友方建筑 |
| `pierce` | **false** | 是否穿透单位 |
| `pierceBuilding` | **false** | 是否穿透建筑 |
| `pierceCap` | -1 | 最大穿透数（-1=无限） |
| `pierceArmor` | **false** | 是否无视护甲 |

## 反射与吸收

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `hittable` | **true** | 能否被点防击中 |
| `reflectable` | **true** | 能否被反射 |
| `absorbable` | **true** | 能否被护盾吸收 |
| `laserAbsorb` | **true** | 激光是否被塑料墙吸收 |

## 伤害与效果

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `splashDamage` | 0f | 溅射伤害 |
| `scaledSplashDamage` | **false** | 溅射伤害是否按体型缩放 |
| `buildingDamageMultiplier` | 1f | 建筑伤害倍率 |
| `shieldDamageMultiplier` | 1f | 护盾伤害倍率 |
| `makeFire` | **false** | 是否点燃 |
| `pierceArmor` | **false** | 是否无视护甲 |

## 碎片

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `fragOnHit` | **true** | 命中时产生碎片 |
| `fragOnDespawn` | **true** | 消失时产生碎片 |
| `fragOnAbsorb` | **true** | 被吸收时产生碎片 |

## 基础字段

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `damage` | 1f | 基础伤害 |
| `speed` | 1f | 速度 |
| `lifetime` | 40f | 生命周期(ticks) |
| `hitSize` | 4 | 碰撞体积 |
| `drag` | 0f | 阻力 |
| `accel` | 0f | 加速度 |
| `layer` | Layer.bullet | 渲染层 |
| `status` | StatusEffects.none | 状态效果 |
| `statusDuration` | 480f (8秒) | 状态持续时间 |
| `ammoMultiplier` | 2f | 弹药倍率 |
| `reloadMultiplier` | 1f | 装填倍率 |
| `createChance` | 1 | 生成概率 |
| `recoil` | 0f | 后坐力 |

## 颜色与视觉

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `hitColor` | Color.white | 命中特效颜色 |
| `trailColor` | Pal.missileYellowBack | 拖尾颜色 |
| `lightColor` | Pal.powerLight | 光照颜色 |
| `healColor` | Pal.heal | 治疗颜色 |
| `hitEffect` | Fx.hitBulletSmall | 命中特效 |
| `despawnEffect` | Fx.hitBulletSmall | 消失特效 |
| `smokeEffect` | Fx.shootSmallSmoke | 烟雾特效 |
| `trailEffect` | Fx.missileTrail | 拖尾特效 |
| `trailLength` | -1 | 拖尾长度(-1禁用) |
| `trailWidth` | 2f | 拖尾宽度 |

## 闪电

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `lightning` | 0 | 闪电根数 |
| `lightningLength` | 5 | 闪电长度 |
| `lightningDamage` | -1 (-1=用damage) | 闪电伤害 |
| `lightningCone` | 360f | 闪电范围 |
| `lightningColor` | Pal.surge | 闪电颜色 |

## 碎片

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `fragBullet` | null | 碎片类型 |
| `fragBullets` | 9 | 碎片数量 |
| `fragLifeMin` | 1f | 碎片最小生命 |
| `fragLifeMax` | 1f | 碎片最大生命 |
| `fragOnHit` | **true** | 命中时产生碎片 |
| `fragOnDespawn` | **true** | 消失时产生碎片 |
| `fragOnAbsorb` | **true** | 被吸收时产生碎片 |

## 间隔子弹

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `intervalBullet` | null | 间隔子弹类型 |
| `bulletInterval` | 20f | 间隔时间 |
| `intervalBullets` | 1 | 每次数量 |

## 生成单位

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `spawnUnit` | null | 生成单位类型 |
| `despawnUnit` | null | 消失时生成单位 |
| `despawnUnitChance` | 1 | 生成概率 |
| `despawnUnitCount` | 1 | 生成数量 |

## 治疗

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `healPercent` | 0f | 治疗百分比 |
| `healAmount` | 0f | 治疗固定值 |
| `lifesteal` | 0f | 生命偷取 |

## BasicBulletType 扩展字段

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `backColor` | Pal.bulletYellowBack | 背面颜色 |
| `frontColor` | Pal.bulletYellow | 正面颜色 |
| `width` | 5f | 宽度 |
| `height` | 7f | 高度 |
| `shrinkX` | 0f | X轴收缩 |
| `shrinkY` | 0.5f | Y轴收缩 |
| `spin` | 0 | 旋转速度 |
| `sprite` | "bullet" | 精灵图 |

## 继承链

```
BulletType (900+行)
├── BasicBulletType (shrinkX/Y, spin, frontColor, backColor)
│   ├── MissileBulletType (homing)
│   ├── FlakBulletType (explodeRange, explodeDelay)
│   ├── ArtilleryBulletType
│   ├── LaserBoltBulletType
│   └── MultiBulletType
├── ContinuousBulletType
│   ├── ContinuousLaserBulletType
│   └── ContinuousFlameBulletType
├── LaserBulletType (length, width, sideLength)
├── LightningBulletType
├── LiquidBulletType
├── SapBulletType
├── RailBulletType
├── PointBulletType
├── PointLaserBulletType
├── ShrapnelBulletType
├── BombBulletType
├── ExplosionBulletType
├── FireBulletType
├── EmpBulletType
├── EmptyBulletType
├── InterceptorBulletType
├── MassDriverBolt
└── SpaceLiquidBulletType
```

## 常见误区

- ❌ `collides` 默认false → ✅ 默认true
- ❌ `hittable` 默认false → ✅ 默认true（可被点防击中）
- ❌ `reflectable` 默认false → ✅ 默认true（可被反射）
- ❌ `absorbable` 默认false → ✅ 默认true（可被护盾吸收）
- ❌ `pierce` 默认true → ✅ 默认false
- ❌ `makeFire` 默认true → ✅ 默认false
- ❌ `pierceArmor` 默认true → ✅ 默认false
- ❌ `shrinkX/shrinkY` 在BulletType → ✅ 在BasicBulletType
- ❌ `frontColor/backColor` 在BulletType → ✅ 在BasicBulletType
- ❌ `spin` 在BulletType → ✅ 在BasicBulletType
- ❌ `trailLength` 默认0 → ✅ 默认-1（禁用）
- ❌ `fragBullets` 默认1 → ✅ 默认9
- ❌ `statusDuration` 默认60f → ✅ 默认480f（8秒）
