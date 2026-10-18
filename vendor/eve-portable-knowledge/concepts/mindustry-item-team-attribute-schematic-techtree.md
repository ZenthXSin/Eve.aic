# Mindustry Item、Team、Attribute、Schematic、TechTree 源码验证

> Source: `源码和示例/Mindustry/core/src/` 直接验证，2026-06-08

## 1. Item 类（`type/Item.java`，165行）

### 字段

```java
public class Item extends UnlockableContent implements Senseable {
    public Color color;                          // 颜色（构造函数中初始化）
    public float explosiveness = 0f;             // 爆炸性（0~1）
    public float flammability = 0f;              // 可燃性（0~1）
    public float radioactivity;                  // 放射性（0~1）
    public float charge = 0f;                    // 电荷
    public int hardness = 0;                     // 硬度（影响钻头采矿）
    public float cost = 1f;                      // 成本（影响建造时间）
    public float healthScaling = 0f;             // 健康缩放（影响建筑血量）
    public boolean lowPriority;                  // 低优先级（AI 不主动采集）
    public int frames = 0;                       // 动画帧数
    public int transitionFrames = 0;             // 过渡帧数
    public float frameTime = 5f;                 // 每帧时间
    public boolean buildable = true;             // 是否可用于建造
    public boolean hidden = false;               // 是否隐藏
}
```

### 常见误区

- **color 在构造函数中设置**：不是在 init() 或 load() 中
- **explosiveness/flammability 默认 0f**：不是自动计算的
- **radioactivity 没有默认值**：默认 0（Java float 默认值）
- **cost 默认 1f**：不是 0，影响建造时间计算
- **healthScaling 默认 0f**：影响建筑血量计算（`scaledHealth *= scaling`）
- **hardness 默认 0**：影响钻头采矿速度
- **lowPriority 默认 false**：AI 会主动采集
- **buildable 默认 true**：可用于建造
- **hidden 默认 false**：不隐藏
- **frames 默认 0**：无动画（静态图标）
- **Item 不是 enum**：是类，通过 Items.load() 注册

## 2. Team 类（`game/Team.java`，183行）

### 字段

```java
public class Team implements Comparable<Team>, Senseable {
    public final int id;                         // 队伍 ID（0-255）
    public final Color color;                    // 队伍颜色
    public final Color[] palette = {new Color(), new Color(), new Color()}; // 调色板
    public final int[] palettei = new int[3];    // 调色板整数形式
    public boolean ignoreUnitCap = false;        // 是否忽略单位上限
    public String emoji = "";                    // 表情符号
    public boolean hasPalette;                   // 是否有调色板
    public String name;                          // 队伍名称

    public static final Team[] all = new Team[256]; // 所有队伍
    public static final Team[] baseTeams = new Team[6]; // 基础队伍
}
```

### 内置队伍

```java
public final static Team
    derelict = new Team(0, "derelict", Color.valueOf("4d4e58")),    // 废弃
    sharded = new Team(1, "sharded", Pal.accent.cpy(), ...),        // 玩家
    crux = new Team(2, "crux", Color.valueOf("f25555"), ...),       // 敌对
    malis = new Team(3, "malis", Color.valueOf("a27ce5"), ...),     // 紫色
    green = new Team(4, "green", Color.valueOf("54d67d")),          // 绿色
    blue = new Team(5, "blue", Color.valueOf("6c87fd")),            // 蓝色
    neoplastic = new Team(6, "neoplastic", Color.valueOf("e05438")); // 肿瘤
```

### 关键方法

```java
static Team get(int id)          // 按 ID 获取队伍
TeamRule rules()                 // 获取队伍规则
TeamData data()                  // 获取队伍数据
boolean active()                 // 是否活跃
boolean isAlive()                // 是否存活
boolean isAI()                   // 是否 AI
boolean isOnlyAI()               // 是否只有 AI
boolean needsFlowField()         // 是否需要流场
boolean activateUnitFactories()  // 是否激活单位工厂
String localized()               // 本地化名称
String coloredName()             // 彩色名称
```

### 常见误区

- **Team 是运行时对象**：不是 UnlockableContent，不通过 ContentType 注册
- **Team 有 256 个**：`all = new Team[256]`，前 7 个是内置的
- **neoplastic.ignoreUnitCap = true**：肿瘤队忽略单位上限
- **baseTeams 只有 6 个**：不包括 neoplastic
- **palette 有 3 个颜色**：用于队伍颜色渲染
- **调色板队伍**：sharded/crux/malis 有完整调色板，green/blue 没有

## 3. TeamEntry 类（`type/TeamEntry.java`，31行）

```java
/** This class is only for displaying team lore in the content database. */
public class TeamEntry extends UnlockableContent {
    public final Team team;

    public TeamEntry(String name, Team team) { ... }
    public TeamEntry(Team team) { ... }
}
```

### 常见误区

- **TeamEntry 只用于显示**：是 UnlockableContent，用于内容数据库显示
- **TeamEntry 持有 Team 引用**：不是 Team 本身
- **ContentType.team 对应 TeamEntry**：不是 Team
- **Team 是运行时对象**：TeamEntry 是内容对象

## 4. Attribute 类（`world/meta/Attribute.java`，72行）

### 内置属性

```java
public class Attribute {
    public static final Attribute
        heat = add("heat"),      // 热量（热发电机）
        spores = add("spores"),  // 孢子（培养器）
        water = add("water"),    // 水（抽水机）
        oil = add("oil"),        // 油（采油机）
        light = add("light"),    // 光照（太阳能板）
        sand = add("sand"),      // 沙（采沙机）
        steam = add("steam");    // 蒸汽（Erekir 通风口）

    public static Attribute[] all = {};  // 所有属性
    public static ObjectMap<String, Attribute> map = new ObjectMap<>(); // 名称映射

    public final int id;
    public final String name;
}
```

### 关键方法

```java
float env()                    // 获取当前环境的属性值
static Attribute get(String name)      // 按名称获取（不存在则抛异常）
static @Nullable Attribute getOrNull(String name) // 按名称获取（可能 null）
static boolean exists(String name)     // 是否存在
static Attribute add(String name)      // 注册新属性（mod 用）
```

### 常见误区

- **Attribute 是可扩展的**：mod 可以通过 `Attribute.add()` 注册新属性
- **env() 从 state.envAttrs 获取**：不是从 Block 或 Tile 获取
- **light 可以为负值**：负值降低太阳能板效率
- **steam 仅 Erekir 使用**：不是通用属性

## 5. Attributes 类（`world/blocks/Attributes.java`，84行）

```java
public class Attributes implements JsonSerializable {
    private float[] arr = new float[Attribute.all.length];

    float get(Attribute attr)           // 获取属性值
    void set(Attribute attr, float value) // 设置属性值
    void add(Attributes other)          // 叠加另一个 Attributes
    void add(Attributes other, float scl) // 按比例叠加
    void clear()                        // 清零
}
```

### 常见误区

- **Attributes 是 float 数组封装**：按 Attribute.id 索引
- **arr 长度会动态扩展**：当 mod 注册新 Attribute 时自动扩展
- **add 是累加**：不是覆盖

## 6. Schematic 类（`game/Schematic.java`，158行）

### 字段

```java
public class Schematic implements Publishable, Comparable<Schematic> {
    public final Seq<Stile> tiles;               // 方块列表
    public Seq<String> labels = new Seq<>();     // 标签
    public StringMap tags;                       // 元数据标签
    public int width, height;                    // 尺寸
    public @Nullable Fi file;                    // 文件
    public @Nullable LoadedMod mod;              // 关联 mod
}
```

### 关键方法

```java
float powerProduction()      // 总发电量
float powerConsumption()     // 总耗电量
ItemSeq requirements()       // 总需求物品
boolean hasCore()            // 是否包含核心
String name()                // 名称（从 tags 获取）
String description()         // 描述
```

### 常见误区

- **tiles 是 Seq<Stile>**：Stile 包含 block、x、y、config、rotation
- **tags 是 StringMap**：键值对存储元数据
- **labels 是 Seq<String>**：用于 UI 标签筛选
- **file 可为 null**：内存中的蓝图没有文件
- **mod 可为 null**：非 mod 蓝图

## 7. TechTree/TechNode（`content/TechTree.java`）

### TechNode 字段

```java
public static class TechNode {
    public int depth;                            // 深度
    public @Nullable Drawable icon;              // 图标
    public @Nullable String name;                // 根节点名称
    public boolean requiresUnlock = false;       // 是否需要解锁
    public @Nullable TechNode parent, rootNode;  // 父节点、根节点
    public @Nullable ObjectFloatMap<Item> researchCostMultipliers; // 研究成本倍率
    public UnlockableContent content;            // 关联内容
    public ItemStack[] requirements;             // 研究需求
    public ItemStack[] finishedRequirements;     // 已完成需求
    public Seq<Objective> objectives = new Seq<>(); // 额外目标
    public final Seq<TechNode> children = new Seq<>(); // 子节点
    public @Nullable Planet planet;              // 关联行星
}
```

### 构造函数逻辑

```java
public TechNode(@Nullable TechNode parent, UnlockableContent content, ItemStack[] requirements) {
    if(parent != null) {
        parent.children.add(this);
        rootNode = parent.rootNode == null ? parent : parent.rootNode;
        planet = parent.planet;
        researchCostMultipliers = parent.researchCostMultipliers;
    }
    this.parent = parent;
    this.content = content;
    this.depth = parent == null ? 0 : parent.depth + 1;
}
```

### 常见误区

- **requiresUnlock 默认 false**：根节点默认不需要解锁
- **planet 从父节点继承**：不是手动设置的
- **researchCostMultipliers 从父节点继承**：不是每个节点独立
- **rootNode 是根节点**：parent 为 null 时 rootNode 为 null
- **depth 从 0 开始**：根节点 depth = 0
- **objectives 是额外目标**：不是主要需求
- **finishedRequirements 长度与 requirements 相同**：用于跟踪进度
