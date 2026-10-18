# Mindustry 伤害类型系统候选方案分析

> Source: 源码分析 `BulletType.hitEntity()` / `ShieldComp.damage()` / `BuildingComp.collision()` / `Damage.damage()`；参考 NewHorizon `TypeDamageBulletType` 实现

## 背景：原版伤害管线

```
BulletType.hitEntity(b, entity, health)
  → h.damage(damage)            // 普通伤害
  → h.damagePierce(damage)      // 穿甲
  → h.damageArmorMult(d, mult)  // 护甲倍率

ShieldComp.damage(amount)       // Unit 伤害入口
  → rawDamage(Damage.applyArmor(amount, armor) / healthMultiplier / unitHealth)

BuildingComp.collision(bullet)
  → Damage.applyArmor(damage, block.armor * armorMultiplier * blockArmorMultiplier)
  → damage(team, finalDamage)
```

**关键事实**：
- 原版 `damage(float)` 无类型信息，只有数值+护甲+护盾
- `BulletType` 已有 `pierceArmor`、`armorMultiplier`、`shieldDamageMultiplier` 等字段，但没有通用类型枚举
- `Bullet.data` 是 `Object` 类型，可用于传递任意数据
- `Bullet.owner` 可追溯到发射者（Unit / Building）
- NewHorizon 通过 `TypeDamageBulletType` 接口将伤害拆为动能/能量两类，用 `shieldDamageMultiplier` 区分

---

## 方案 A：BulletType 子类 + 接口标记

### 思路
每种伤害类型写一个 `BulletType` 子类（如 `ElectroBulletType`），实现统一的 `DamageTypeProvider` 接口。伤害计算完全在子类的 `hitEntity()` / `hitTile()` 里完成。

### 结构
```java
// 标记接口
public interface DamageTypeProvider {
    DamageType damageType();
    ObjectMap<String, Object> damageData(); // 额外数据
}

// 伤害类型枚举
public enum DamageType {
    KINETIC, ELECTROMAGNETIC, FIRE, ICE, CORROSION, ENERGY, EXPLOSIVE, TRUE
}

// 子类示例
public class ElectroBulletType extends BasicBulletType implements DamageTypeProvider {
    public DamageType damageType() { return DamageType.ELECTROMAGNETIC; }
    
    @Override
    public void hitEntity(Bullet b, Hitboxc entity, float health) {
        float finalDamage = b.damage;
        if (entity instanceof Unit u && u.type != null) {
            // 检查标签
            if (UnitTags.has(u.type, "mechanical")) {
                finalDamage *= 1.25f; // 机械单位 +25%
            }
        }
        // 写回后走原版
        b.damage = finalDamage;
        super.hitEntity(b, entity, health);
    }
}
```

### 优点
- **零侵入**：不改原版任何文件，纯扩展
- **类型安全**：编译期确定每种子弹的伤害类型
- **NewHorizon 验证过**：`TypeDamageBulletType` 用类似模式在生产中运行
- **每种子弹可有独特的计算逻辑**，不受通用框架约束

### 缺点
- **每种类型都要写子类**：5 种类型 × 5 种子弹形态 = 25 个类，爆炸式增长
- **跨子弹类型无法共享规则**：电磁增伤逻辑在 `ElectroBulletType` 里，但激光子弹是 `LaserBulletType`，需要每个子类各自实现
- **无法处理原版子弹**：原版 `BasicBulletType` 没有类型信息
- **Splash Damage 路径遗漏**：`Damage.damage()` 是静态方法，不走 `hitEntity()`，类型增伤在那里失效

### 适用场景
类型少（2-3种）、子弹种类少、不需要覆盖原版

---

## 方案 B：DamageContext 中间层（推荐首选）

### 思路
在 `BulletType.hitEntity()` 入口之前/之后插入一个 `DamageContext` 解析层。不改 `hitEntity` 本身，而是通过**继承 + 委派**包装。所有类型规则、标签检查、数据附加都在 `DamageContext` 里集中处理。

### 结构
```java
// 伤害上下文 - 贯穿整个伤害计算
public class DamageContext {
    public Bullet source;        // 伤害来源子弹
    public Healthc target;       // 受击目标
    public Entityc attacker;     // 攻击者 (owner)
    public DamageType type;      // 伤害类型
    public float baseDamage;     // 原始伤害
    public float finalDamage;    // 最终伤害
    public ObjectMap<String, Object> data; // 扩展数据
    
    // 计算最终伤害
    public float resolve() {
        float dmg = baseDamage;
        
        // 1. 类型 vs 目标标签 修正
        if (target instanceof Unit u) {
            for (DamageRule rule : DamageTypeSystem.rules) {
                dmg = rule.modify(dmg, this);
            }
        }
        
        // 2. 护甲（保持原版逻辑）
        if (!source.type.pierceArmor) {
            float armor = target instanceof Unit u ? u.armor : 
                          target instanceof Building b ? b.block.armor : 0;
            dmg = Damage.applyArmor(dmg, armor * source.type.armorMultiplier);
        }
        
        finalDamage = dmg;
        return dmg;
    }
}

// 规则接口
public interface DamageRule {
    float modify(float damage, DamageContext ctx);
}

// 中心化管理
public class DamageTypeSystem {
    public static ObjectMap<UnitType, ObjectSet<String>> unitTags = new ObjectMap<>();
    public static ObjectMap<DamageType, ObjectMap<String, Float>> typeMultipliers = new ObjectMap<>();
    public static Seq<DamageRule> rules = new Seq<>();
    
    // 注册标签
    public static void tag(UnitType type, String tag) {
        unitTags.get(type, ObjectSet::new).add(tag);
    }
    
    // 注册类型对标签的倍率
    public static void setMultiplier(DamageType type, String tag, float mult) {
        typeMultipliers.get(type, () -> new ObjectMap<>()).put(tag, mult);
    }
    
    // 自动组装规则
    static {
        rules.add((dmg, ctx) -> {
            if (ctx.target instanceof Unit u) {
                ObjectSet<String> tags = unitTags.get(u.type);
                if (tags != null) {
                    ObjectMap<String, Float> mults = typeMultipliers.get(ctx.type);
                    if (mults != null) {
                        for (String tag : tags) {
                            Float m = mults.get(tag);
                            if (m != null) dmg *= m;
                        }
                    }
                }
            }
            return dmg;
        });
    }
}

// 用法：一个通用 BulletType 包装器
public class TypedBulletType extends BasicBulletType {
    public DamageType damageType = DamageType.KINETIC;
    
    @Override
    public void hitEntity(Bullet b, Hitboxc entity, float health) {
        DamageContext ctx = new DamageContext(b, entity, damageType);
        float oldDamage = b.damage;
        b.damage = ctx.resolve(); // 替换为计算后的伤害
        super.hitEntity(b, entity, health);
        b.damage = oldDamage;     // 还原
    }
}
```

### 用法示例
```java
// 初始化时注册
DamageTypeSystem.tag(UnitTypes.mace, "mechanical");
DamageTypeSystem.tag(UnitTypes.dagger, "mechanical");
DamageTypeSystem.tag(UnitTypes.crawler, "organic");
DamageTypeSystem.setMultiplier(DamageType.ELECTROMAGNETIC, "mechanical", 1.25f);
DamageTypeSystem.setMultiplier(DamageType.FIRE, "organic", 1.5f);

// JSON mod 里用 TypedBulletType
// 或 Java mod 里 new TypedBulletType() {{ damageType = DamageType.ELECTROMAGNETIC; }};
```

### 优点
- **集中管理**：所有类型规则、标签、倍率在一个地方，数据驱动
- **低侵入**：不改原版类，通过继承 `TypedBulletType` 扩展
- **可扩展**：加新类型 = 加枚举值 + 注册倍率，不写新类
- **支持原版子弹**：可以用 `Events.on(UnitDamageEvent.class)` 桥接原版子弹
- **数据携带**：`DamageContext.data` 可存任意 KV（如元素层数、持续时间、减抗值）
- **规则可叠加**：多个 `DamageRule` 按优先级叠加，不做 if-else 地狱

### 缺点
- **Splash 路径仍需处理**：`Damage.damage()` 是静态方法，需要额外桥接
- **子弹创建时需指定类型**：不能在命中时动态决定（但一般够用）
- **需要维护一个 `DamageTypeSystem` 单例**，初始化时机要在内容加载后

### Splash 桥接方案
```java
// 在 TypedBulletType 里重写 createSplashDamage
@Override
public void createSplashDamage(Bullet b, float x, float y) {
    if (splashDamageRadius > 0 && !b.absorbed) {
        // 用自定义的 Damage.typedDamage 替代
        DamageTypeSystem.damageArea(b, x, y, splashDamageRadius, 
            splashDamage * b.damageMultiplier(), damageType);
    }
}
```

---

## 方案 C：事件总线拦截

### 思路
利用 Mindustry 已有的 `Events` 系统，在 `UnitDamageEvent` / `BuildingBulletDestroyEvent` 等事件触发时，通过监听器修改伤害。不改任何 `hitEntity`，纯事件驱动。

### 结构
```java
// 自定义事件
public class TypedDamageEvent {
    public Bullet source;
    public Healthc target;
    public DamageType type;
    public float damage;
    public ObjectMap<String, Object> data;
    public float multiplier = 1f; // 监听器修改此值
}

// 注册监听器
Events.on(UnitDamageEvent.class, e -> {
    Bullet b = e.bullet;
    if (b == null) return;
    
    // 从子弹获取类型信息
    DamageType type = getDamageType(b); // 查表或从 data 读取
    TypedDamageEvent event = new TypedDamageEvent();
    event.source = b;
    event.target = e.unit;
    event.type = type;
    event.damage = b.damage;
    
    Events.fire(event);
    
    // 应用倍率
    if (event.multiplier != 1f) {
        e.unit.damage(b.damage * (event.multiplier - 1f)); // 补偿差额
    }
});
```

### 优点
- **完全零侵入**：不继承、不改类、纯监听
- **动态灵活**：监听器可以运行时增删

### 缺点
- **时序问题致命**：`UnitDamageEvent` 在 `hitEntity()` **之后**触发，伤害已经扣了，只能事后补偿
- **补偿不精确**：护甲、护盾已经应用过，补偿差额会产生误差（护甲扣两次）
- **性能差**：每帧大量事件触发，每个伤害都要走事件分发
- **不可靠**：事件顺序不保证，多个监听器可能冲突
- **无法阻止伤害**：只能事后补偿，不能事前拦截

### 结论
**不推荐作为主方案**，但可作为辅助（如记录伤害日志、触发视觉效果）

---

## 方案 D：Entity 级数据注入（Tag 系统）

### 思路
不在子弹侧做类型，而在**目标侧**做标签。通过 `ObjectMap<UnitType, ObjectSet<String>>` 给 UnitType 挂标签，然后在通用的伤害修正点检查标签。

### 结构
```java
// 标签注册表（静态，不改 UnitType）
public class EntityTags {
    private static final ObjectMap<UnitType, ObjectSet<String>> unitTags = new ObjectMap<>();
    private static final ObjectMap<BuildingType, ObjectSet<String>> buildingTags = new ObjectMap<>();
    
    public static void add(UnitType type, String... tags) {
        ObjectSet<String> set = unitTags.get(type, ObjectSet::new);
        for (String t : tags) set.add(t);
    }
    
    public static boolean has(UnitType type, String tag) {
        ObjectSet<String> set = unitTags.get(type);
        return set != null && set.contains(tag);
    }
    
    public static boolean has(Unit unit, String tag) {
        return has(unit.type, tag);
    }
}

// 初始化
EntityTags.add(UnitTypes.mace, "mechanical", "armored");
EntityTags.add(UnitTypes.dagger, "mechanical");
EntityTags.add(UnitTypes.crawler, "organic");
EntityTags.add(UnitTypes.flare, "air", "mechanical");
```

### 优点
- **极度简单**：只有标签查询，没有复杂的伤害计算框架
- **零侵入**：纯外部 ObjectMap
- **与方案 B 可组合**：标签系统是方案 B 的子集

### 缺点
- **只解决了标签，没解决伤害修正**：增伤逻辑仍然需要在某个地方实现
- **单独使用不够**：必须配合其他方案

---

## 方案 E：Patching HealthComp（最激进）

### 思路
直接修改原版 `HealthComp` / `ShieldComp` 的 `damage()` 方法，加入类型参数。

### 结构
```java
// 修改 HealthComp
void damage(float amount) { ... }           // 原版保持兼容
void damage(float amount, DamageType type) { // 新增
    // 类型修正
    amount = DamageTypeSystem.resolve(amount, type, this);
    // 走原版逻辑
    damage(amount);
}

// 修改 ShieldComp
public void damage(float amount) { ... }
public void damage(float amount, DamageType type) {
    rawDamage(Damage.applyArmor(amount, armor) / healthMultiplier / unitHealth);
}
```

### 优点
- **全局覆盖**：所有伤害路径都能拿到类型信息
- **Splash 路径自然覆盖**：`Damage.damage()` 里直接调用带类型的 damage

### 缺点
- **侵入性极高**：改 HealthComp 意味着改 Mindustry 核心，其他 mod 可能冲突
- **不兼容**：原版调用 `damage(float)` 的地方不会传类型
- **维护噩梦**：Mindustry 更新后需要重新合并
- **JVM mod 才能做**：JS mod 无法修改组件类

### 结论
**仅适用于 fork 场景**，不适合作为 mod 方案

---

## 方案 F：DataPatcher + 单位运行时属性

### 思路
利用 `DataPatcher`（或自定义 unit 数据）给运行时的 Unit 实例注入属性（如 `isMechanical = true`），然后在伤害路径读取这些属性。

### 结构
```java
// 给 UnitType 注入运行时属性
public class UnitProperties {
    public static final ObjectMap<UnitType, Boolean> mechanical = new ObjectMap<>();
    public static final ObjectMap<UnitType, Float> emWeakness = new ObjectMap<>();
    
    static {
        mechanical.put(UnitTypes.mace, true);
        mechanical.put(UnitTypes.dagger, true);
        emWeakness.put(UnitTypes.mace, 1.25f);
    }
}
```

### 优点
- **简单直接**
- **可与 StatusEffect 结合**：机械单位默认挂一个 `mechanical` 状态

### 缺点
- **DataPatcher 是 MindustryX 的功能**，原版不可用
- **同步问题**：运行时属性不存档，重载后丢失
- **只解决了属性存储，没解决伤害修正**

---

## 综合对比

| 维度 | A 子类 | B Context | C 事件 | D 标签 | E Patch | F DataPatch |
|------|--------|-----------|--------|--------|---------|-------------|
| 侵入性 | 无 | 低 | 无 | 无 | 极高 | 中 |
| 可扩展性 | 低 | 高 | 中 | 低 | 高 | 中 |
| 性能 | 高 | 高 | 低 | 高 | 高 | 高 |
| 可靠性 | 高 | 高 | 低 | 高 | 中 | 中 |
| 覆盖范围 | 仅自定义 | 全部* | 全部 | N/A | 全部 | 全部 |
| 实现难度 | 中 | 中 | 低 | 低 | 高 | 低 |
| 类型多样性 | 低 | 高 | 中 | N/A | 高 | 中 |
| Splash支持 | 需额外处理 | 需桥接 | 事后补偿 | N/A | 原生 | 原生 |

*覆盖范围 "全部" 需要配合事件桥接原版子弹

---

## 推荐组合方案

**方案 B（DamageContext）+ 方案 D（标签系统）**是最优解：

```
┌─────────────────────────────────────┐
│         DamageTypeSystem            │
│  ┌───────────┐  ┌────────────────┐  │
│  │ EntityTags │  │ TypeMultipliers│  │
│  │ (方案 D)   │  │ (类型×标签表)  │  │
│  └───────────┘  └────────────────┘  │
│  ┌───────────────────────────────┐  │
│  │       DamageContext            │  │
│  │  source → type → tags → mult  │  │
│  │  → armor → shield → final     │  │
│  └───────────────────────────────┘  │
│  ┌───────────────────────────────┐  │
│  │     TypedBulletType            │  │
│  │  extends BasicBulletType       │  │
│  │  overrides hitEntity/hitTile   │  │
│  └───────────────────────────────┘  │
│  ┌───────────────────────────────┐  │
│  │  Events bridge (原版子弹)      │  │
│  │  后补偿方式处理原版伤害        │  │
│  └───────────────────────────────┘  │
└─────────────────────────────────────┘
```

**第一步**：实现 `EntityTags` + `DamageType` 枚举（1小时）
**第二步**：实现 `DamageContext` + `DamageRule` 接口（2小时）
**第三步**：实现 `TypedBulletType`，覆盖 `hitEntity` + `createSplashDamage`（1小时）
**第四步**：注册默认规则（电磁→机械+25% 等）（30分钟）
**第五步**：Events bridge 处理原版子弹（可选，1小时）

总计约 5-6 小时可落地可用版本。

---

## 电磁伤害→机械+25% 的具体实现

```java
// 1. 定义枚举
public enum DamageType {
    KINETIC, ELECTROMAGNETIC, FIRE, ICE, CORROSION, ENERGY, EXPLOSIVE, TRUE
}

// 2. 注册标签
EntityTags.add(UnitTypes.mace, "mechanical");
EntityTags.add(UnitTypes.dagger, "mechanical");
EntityTags.add(UnitTypes.crawler, "organic");
// ... 其他单位

// 3. 注册倍率
DamageTypeSystem.setMultiplier(DamageType.ELECTROMAGNETIC, "mechanical", 1.25f);

// 4. 使用
TypedBulletType emBullet = new TypedBulletType() {{
    damageType = DamageType.ELECTROMAGNETIC;
    damage = 20;
    // ... 其他配置
}};
// 命中机械单位时自动 20 * 1.25 = 25
```
