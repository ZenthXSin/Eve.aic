# Mindustry 内容类型解析链、Block 生命周期、DrawBlock 解析

> Source: `源码和示例/Mindustry/core/src/` 直接验证，2026-06-08

## 1. ContentType 枚举（`ctype/ContentType.java`，38行）

```java
public enum ContentType {
    item(Item.class),
    block(Block.class),
    mech_UNUSED(null),           // 已废弃
    bullet(BulletType.class),
    liquid(Liquid.class),
    status(StatusEffect.class),
    unit(UnitType.class),
    weather(Weather.class),
    effect_UNUSED(null),         // 已废弃
    sector(SectorPreset.class),
    loadout_UNUSED(null),        // 已废弃
    typeid_UNUSED(null),         // 已废弃
    error(null),
    planet(Planet.class),
    ammo_UNUSED(null),           // 已废弃
    team(TeamEntry.class),
    unitCommand(UnitCommand.class),
    unitStance(UnitStance.class);

    public static final ContentType[] all = values();
    public final @Nullable Class<? extends Content> contentClass;
}
```

**关键**：
- **不要重排枚举顺序**：注释写着"Do not rearrange, ever!"——ID 与存档绑定
- 有 5 个 `_UNUSED` 条目：`mech_UNUSED`, `effect_UNUSED`, `loadout_UNUSED`, `typeid_UNUSED`, `ammo_UNUSED`
- `error` 是特殊类型，用于错误内容
- `team` 类型是 `TeamEntry`，不是 `Team`（Team 是运行时对象）

## 2. 内容目录命名规则（源码验证）

```java
// Mods.loadContent() 第 926-927 行
String lower = type.name().toLowerCase(Locale.ROOT);
Fi folder = contentRoot.child(lower + (lower.endsWith("s") ? "" : "s"));
```

**推导**：
- `item` → `items/`
- `block` → `blocks/`
- `bullet` → `bullets/`
- `liquid` → `liquids/`
- `status` → `statuses/`（加 s）
- `unit` → `units/`
- `weather` → `weathers/`（加 s）
- `sector` → `sectors/`（加 s）
- `planet` → `planets/`（加 s）
- `team` → `teams/`（加 s）
- `unitCommand` → `unitcommands/`（加 s）
- `unitStance` → `unitstances/`（加 s）

**常见误区**：
- `content/block/` 不会被扫描，必须是 `content/blocks/`
- `content/status/` 不会被扫描，必须是 `content/statuses/`
- 目录名是小写，不是大写
- `unitCommand` 的目录名是 `unitcommands/`（全小写）

## 3. JSON Block 解析链（ContentParser）

```java
// ContentParser 第 561-598 行
ContentType.block, (TypeParser<Block>)(mod, name, value) -> {
    readBundle(ContentType.block, name, value);  // 读取 bundle 翻译

    Block block;

    if(locate(ContentType.block, name) != null) {
        // 已存在同名 block（覆盖原版）
        if(value.has("type")) {
            // 有 type 字段 → 创建新 block（警告：不能改变原版 type）
            warn("re-declares a type...");
            block = make(resolve(value.getString("type", ""), Block.class), mod + "-" + name);
        } else {
            // 无 type 字段 → 直接修改原版 block
            block = locate(ContentType.block, name);
        }
    } else {
        // 新 block
        block = make(resolve(value.getString("type", ""), Block.class), mod + "-" + name);
    }

    // 解析 consumes 字段（特殊处理）
    if(value.has("consumes") && value.get("consumes").isObject()) {
        readBlockConsumers(block, value.get("consumes"));
        value.remove("consumes");
    }

    // 解析其他字段
    readFields(block, value, true);

    // 自动设置可见性
    if(value.has("requirements") && block.buildVisibility == BuildVisibility.hidden) {
        block.buildVisibility = BuildVisibility.shown;
    }

    return block;
}
```

### 关键流程

1. **查找已有内容**：`locate(ContentType.block, name)` 检查是否已存在
2. **type 解析**：`resolve(value.getString("type", ""), Block.class)`
   - 空字符串 → 默认 `Block.class`
   - 首字母小写 → 自动 capitalize（`"mechanicalDrill"` → `"MechanicalDrill"`）
   - 带点号 → `Class.forName()` 直接加载
3. **命名**：新 block 名称 = `mod名 + "-" + 文件名`
4. **consumes 特殊处理**：在 `readFields` 之前单独处理
5. **自动可见性**：有 requirements 时自动设置为 shown

### 常见误区

- **覆盖原版 block 时不能改 type**：写 type 会创建新 block，不写才能修改原版
- **type 默认是 Block**：不写 type 字段时默认 `Block`，不是 `GenericCrafter`
- **type 名称首字母自动大写**：`"mechanicalDrill"` 会自动转为 `"MechanicalDrill"`
- **consumes 是特殊字段**：在 `readFields` 之前单独处理，不是普通反射
- **block 名自动加 mod 前缀**：`mod名-文件名`，避免冲突

## 4. 其他类型的 type 解析

### BulletType

```java
// 先尝试 capitalize + "BulletType"
Class<?> alternate = resolve(Strings.capitalize(data.getString("type", "basic")) + "BulletType", Object.class, false);
// 不存在则直接 resolve
Class<?> bc = alternate == Object.class ? resolve(data.getString("type", ""), BasicBulletType.class) : alternate;
```

- `"type": "basic"` → 先找 `BasicBulletType`，存在则用，否则直接 resolve
- 默认是 `BasicBulletType`
- 数组形式 → `MultiBulletType`
- 字符串形式 → 从 `Bullets` 类查找常量

### DrawBlock

```java
// 字符串形式 → 直接实例化
if(data.isString()) return make(resolve(data.asString()));
// 数组形式 → DrawMulti
if(data.isArray()) return new DrawMulti(parser.readValue(DrawBlock[].class, data));
// 对象形式 → 按 type 解析，默认 DrawDefault
var bc = resolve(data.getString("type", ""), DrawDefault.class);
```

- 默认是 `DrawDefault`
- 数组形式自动创建 `DrawMulti`
- 字符串形式直接实例化（无参数）

### ShootPattern

```java
var bc = resolve(data.getString("type", ""), ShootPattern.class);
```

- 默认是 `ShootPattern`（基础类）

### DrawPart

```java
Class<?> bc = resolve(data.getString("type", ""), RegionPart.class);
```

- 默认是 `RegionPart`

### Ability

```java
Class<? extends Ability> oc = resolve(data.getString("type", ""));
```

- **无默认值**：必须指定 type，否则报错

### StatusEffect

```java
// 字符串形式 → 从 StatusEffects 查找
if(data.isString()) return field(StatusEffects.class, data);
// 对象形式 → 按 type 解析
var effect = make(resolve(data.getString("type", ""), StatusEffect.class));
```

- 默认是 `StatusEffect`

### Consume 类型

```java
Class<?> consumeType = resolve("Consume" + Strings.capitalize(type), Consume.class);
```

- `"items"` → `ConsumeItems`
- `"liquid"` → `ConsumeLiquid`
- `"power"` → `ConsumePower`

## 5. resolve() 方法逻辑

```java
<T> Class<T> resolve(String base, Class<T> def, boolean warn) {
    // 空字符串 → 返回默认值
    if((base == null || base.isEmpty()) && def != null) return def;

    // 从 ClassMap 全局映射查找（自动 capitalize 首字母）
    var out = ClassMap.classes.get(
        !base.isEmpty() && Character.isLowerCase(base.charAt(0))
            ? Strings.capitalize(base) : base
    );
    if(out != null) return (Class<T>)out;

    // 带点号的类名 → Class.forName()
    if(base.indexOf('.') != -1 && allowClassResolution) {
        try { return (Class<T>)Class.forName(base); }
        catch(Exception ignored) {
            try { return (Class<T>)Class.forName(base, true, mods.mainLoader()); }
            catch(Exception ignore) {}
        }
    }

    // 都找不到 → 返回默认值或抛异常
    if(def != null) {
        if(warn) warn("No type found, defaulting to...");
        return def;
    }
    throw new IllegalArgumentException("Type not found: " + base);
}
```

### 关键

1. **首字母自动大写**：`"mechanicalDrill"` → `"MechanicalDrill"`
2. **ClassMap 全局映射**：包含所有原版类
3. **带点号的类名**：`"com.example.MyBlock"` → `Class.forName()`
4. **mod 类加载器**：`mods.mainLoader()` 用于加载 mod 的 Java 类
5. **找不到时用默认值**：不会报错，只 warn

## 6. Block.init() 生命周期

```java
public void init() {
    super.init();

    // 1. 设置音效（根据 size）
    if(destroySound == Sounds.unset) destroySound = size >= 3 ? ... : ...;
    if(placeSound == Sounds.unset) placeSound = size >= 3 ? ... : ...;
    if(breakSound == Sounds.unset) breakSound = size >= 3 ? ... : ...;

    // 2. 自定义阴影
    if(customShadow) hasShadow = false;

    // 3. 优先级
    if(underBullets) priority = TargetPriority.under;

    // 4. 标志
    if(fogRadius > 0) flags = flags.with(BlockFlag.hasFogRadius);
    if(sync) flags = flags.with(BlockFlag.synced);

    // 5. 健康值计算
    if(health == -1) {
        if(scaledHealth < 0) {
            scaledHealth = 40;
            // 按需求物品的 healthScaling 缩放
            for(var stack : requirements) scaling += stack.item.healthScaling;
            scaledHealth *= scaling;
        }
        health = size * size * scaledHealth;
    }

    // 6. 裁剪大小
    clipSize = Math.max(clipSize, size * tilesize);

    // 7. 液体容量自动计算
    if(liquidCapacity < 0) {
        // 根据消费者计算
        liquidCapacity = Mathf.round(10f * consumeAmount);
    }

    // 8. 运输类自动 acceptsItems
    if(group == BlockGroup.transportation || category == Category.distribution)
        acceptsItems = true;

    // 9. 偏移计算
    offset = ((size + 1) % 2) * tilesize / 2f;
    sizeOffset = -((size - 1) / 2);

    // 10. 建造时间自动计算
    if(requirements.length > 0 && buildTime < 0) {
        buildTime = 0f;
        for(ItemStack stack : requirements) buildTime += stack.amount * stack.item.cost;
    }

    // 11. 消费者数组初始化
    consumers = consumeBuilder.toArray(Consume.class);
    // ... 分类为 optional/nonOptional/update

    // 12. 设置 UI 条
    setBars();
}
```

### 常见误区

- **health 默认 -1**：自动按 `size * size * scaledHealth` 计算
- **scaledHealth 默认 40**：再按需求物品的 healthScaling 缩放
- **liquidCapacity 默认 -1**：自动按消费者计算
- **buildTime 默认 -1**：自动按需求物品的 cost 计算
- **buildTime *= buildCostMultiplier**：最后乘以全局倍率
- **consumers 在 init() 中分类**：optional/nonOptional/update 三组
- **acceptsItems 自动设置**：运输类方块自动为 true
- **音效按 size 分级**：1/2/3+ 三档

## 7. Block.load() 生命周期

```java
public void load() {
    super.load();

    // 1. 主纹理
    region = Core.atlas.find(name);

    // 2. ContentRegions 加载
    ContentRegions.loadRegions(this);

    // 3. 队伍颜色纹理
    teamRegions = new TextureRegion[Team.all.length];
    for(Team team : Team.all) {
        teamRegions[team.id] = teamRegion.found() && team.hasPalette
            ? Core.atlas.find(name + "-team-" + team.name, teamRegion)
            : teamRegion;
    }

    // 4. 变体纹理
    if(variants != 0) {
        variantRegions = new TextureRegion[variants];
        for(int i = 0; i < variants; i++) {
            variantRegions[i] = Core.atlas.find(name + (i + 1));
        }
        region = variantRegions[0];
    }
}
```

### 常见误区

- **region 名称 = block name**：`Core.atlas.find(name)`
- **队伍纹理命名**：`name + "-team-" + team.name`
- **变体纹理命名**：`name + (i + 1)`（从 1 开始，不是 0）
- **ContentRegions.loadRegions**：自动加载标准后缀（-top, -bottom, -liquid 等）
- **region 被覆盖**：有 variants 时 region = variantRegions[0]
