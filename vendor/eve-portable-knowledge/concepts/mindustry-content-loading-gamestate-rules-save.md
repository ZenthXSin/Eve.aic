# Mindustry 内容加载链、GameState、Rules、存档系统源码验证

> Source: `源码和示例/Mindustry/core/src/` 直接验证，2026-06-08

## 1. 内容加载链（ContentLoader + ContentParser + Mods）

### 加载顺序

```
1. ContentLoader.createBaseContent()  — 加载原版内容
   → UnitCommand.loadAll()
   → TeamEntries.load()
   → Items.load()
   → UnitStance.loadAll()
   → StatusEffects.load()
   → Liquids.load()
   → Bullets.load()
   → UnitTypes.load()
   → Blocks.load()
   → Loadouts.load()
   → Weathers.load()
   → Planets.load()
   → SectorPresets.load()
   → SerpuloTechTree.load()
   → ErekirTechTree.load()

2. ContentLoader.createModContent()  — 加载 mod 内容
   → mods.loadContent()

3. ContentLoader.load()  — 加载资源（图标等）
   → initialize(Content::loadIcon)
   → initialize(Content::load)

4. ContentLoader.init()  — 初始化所有内容
   → initialize(Content::init)
   → initialize(Content::postInit)
   → logicVars.init()
   → Events.fire(ContentInitEvent)
```

### Mods.loadContent() 逻辑

1. **先加载 Java mod**：遍历 orderedMods()，调用 `mod.main.loadContent()`
2. **再加载 JSON mod**：
   - 遍历每个 mod 的 `content/` 目录
   - 目录名 = `ContentType.name().toLowerCase() + "s"`（如 `blocks/`、`units/`）
   - 先按 `contentOrder` 排序，再按文件名字母排序
   - 调用 `ContentParser.parse()` 解析每个 JSON/HJSON 文件

### ContentParser.parse() 逻辑

```java
public Content parse(LoadedMod mod, String name, String json, Fi file, ContentType type) {
    // 移除未转义的 # 字符
    json = json.replace("#", "\\#");
    // 解析 JSON
    JsonValue value = parser.fromJson(null, Jval.read(json).toString(Jformat.plain));
    // 按 type 查找对应的 parser
    Content c = parsers.get(type).parse(mod.name, name, value);
    // 加入待处理列表
    toBeParsed.add(c);
    return c;
}
```

### 常见误区

- **加载顺序是固定的**：先原版后 mod，先 Java 后 JSON
- **目录名必须以 s 结尾**：`content/blocks/` 不是 `content/block/`
- **contentOrder 可选**：mod.json 中可指定内容加载顺序
- **JSON 中 `#` 会被转义**：`json.replace("#", "\\#")`
- **mod.main 可为 null**：纯 JSON mod 没有主类
- **hidden mod 不加载内容**：`mod.meta.hidden` 为 true 时跳过
- **ContentParser 有 finishParsing()**：所有内容解析完后调用，处理延迟依赖

## 2. GameState（`core/GameState.java`，123行）

### 字段

```java
public class GameState {
    public int wave = 1;                    // 当前波次
    public float wavetime;                  // 波次倒计时（tick）
    public double tick;                     // 逻辑 tick
    public long updateId;                   // 持续递增的更新 ID
    public boolean gameOver = false;        // 是否游戏结束
    public boolean afterGameOver = false;   // 战役后游戏结束状态
    public boolean won = false;             // 是否胜利
    public int serverTps = -1;              // 服务器 TPS（多人模式）
    public Map map = emptyMap;              // 当前地图
    public Rules rules = new Rules();       // 游戏规则
    public GameStats stats = new GameStats(); // 统计
    public MapMarkers markers = new MapMarkers(); // 标记
    public MapLocales mapLocales = new MapLocales(); // 本地化
    public Attributes envAttrs = new Attributes(); // 环境属性
    public Teams teams = new Teams();       // 队伍数据
    public DataPatcher patcher = new DataPatcher(); // JSON 补丁
    public int enemies;                     // 敌人数
    public @Nullable Map playtestingMap;    // 测试地图
    private State state = State.menu;       // 游戏状态
}
```

### State 枚举

```java
public enum State {
    menu,       // 主菜单
    playing,    // 游戏中
    paused,     // 暂停
    editor,     // 地图编辑器
}
```

### 关键方法

```java
boolean hasSpawns()     // 是否有生成点（waves + spawns/attackMode）
boolean isCampaign()    // 是否战役模式
boolean hasSector()     // 是否有扇区
Sector getSector()      // 获取扇区
Planet getPlanet()      // 获取行星
boolean isEditor()      // 是否编辑器
boolean isPaused()      // 是否暂停
boolean isPlaying()     // 是否游戏中
boolean isGame()        // 是否游戏状态（playing 或 paused）
boolean is(State)       // 是否某状态
void set(State)         // 设置状态（触发 StateChangeEvent）
```

### 常见误区

- **wave 默认 1**：不是 0，第一波是 wave 1
- **wavetime 是 tick 不是秒**：需要除以 60 得到秒数
- **tick 是 double**：不是 int/float，可以非常大
- **state 是 private**：通过 `is()` 和 `set()` 访问
- **rules 是 new Rules()**：每次新游戏都创建新 Rules 实例
- **serverTps 默认 -1**：单人模式下为 -1

## 3. Rules（`game/Rules.java`，405行）

### 关键字段（精选高频使用的）

```java
// 游戏模式
public boolean waves = false;                // 是否有波次
public boolean pvp = false;                  // PvP 模式
public boolean attackMode = false;           // 攻击模式
public boolean editor = false;               // 编辑器模式
public boolean infiniteResources = false;    // 无限资源

// 波次
public boolean waveTimer = true;             // 波次计时器
public boolean waveSending = true;           // 是否发送波次
public float waveSpacing = 2 * Time.toMinutes; // 波次间隔（2分钟）
public float initialWaveSpacing = 0f;        // 初始波次间隔
public int winWave = 0;                      // 胜利波次（<=0 禁用）
public boolean randomWaveAI = false;         // 随机波次 AI

// 单位
public int unitCap = 0;                      // 单位上限
public boolean disableUnitCap = false;       // 禁用单位上限
public float unitBuildSpeedMultiplier = 1f;
public float unitCostMultiplier = 1f;
public float unitDamageMultiplier = 1f;
public float unitHealthMultiplier = 1f;
public float unitCrashDamageMultiplier = 1f;

// 建筑
public float blockHealthMultiplier = 1f;
public float blockDamageMultiplier = 1f;
public float buildCostMultiplier = 1f;
public float buildSpeedMultiplier = 1f;
public float deconstructRefundMultiplier = 0.5f;
public boolean ghostBlocks = true;
public boolean instantBuild = false;         // 瞬间建造

// 核心
public float enemyCoreBuildRadius = 400f;    // 敌方核心建造半径
public boolean coreCapture = false;          // 核心可被占领
public boolean canGameOver = true;           // 可能游戏结束
public boolean reactorExplosions = true;     // 反应堆爆炸

// 逻辑
public boolean logicUnitControl = true;      // 逻辑单位控制
public boolean logicUnitBuild = true;        // 逻辑单位建造
public boolean logicUnitDeconstruct = false; // 逻辑单位拆除
public boolean disableWorldProcessors = false; // 禁用世界处理器

// 环境
public int env = Vars.defaultEnv;            // 环境标志位
public Attributes attributes = new Attributes(); // 环境属性
public float dragMultiplier = 1f;            // 阻力倍率
public float solarMultiplier = 1f;           // 太阳能倍率
public boolean fire = true;                  // 是否允许火焰

// 物品
public float itemDepositCooldown = 0.5f;     // 物品存放冷却
public boolean onlyDepositCore = false;      // 只能存入核心

// 内容限制
public ObjectSet<Block> bannedBlocks = new ObjectSet<>();
public ObjectSet<UnitType> bannedUnits = new ObjectSet<>();
public ObjectSet<Block> revealedBlocks = new ObjectSet<>();
public ObjectSet<UnlockableContent> researched = new ObjectSet<>();
public boolean blockWhitelist = false;       // 白名单模式
public boolean unitWhitelist = false;

// 生成
public Seq<SpawnGroup> spawns = new Seq<>();
public Seq<ItemStack> loadout = ItemStack.list(Items.copper, 100); // 默认 100 铜
public Seq<WeatherEntry> weather = new Seq<>(1);
public @Nullable Sector sector;

// 队伍规则
public TeamRules teams = new TeamRules();    // 每个队伍独立规则
```

### TeamRule 内部类

```java
public static class TeamRule {
    public boolean aiCoreSpawn = true;       // AI 核心生成
    public boolean protectCores = true;      // 保护核心
    public boolean checkPlacement = true;    // 检查放置
    public boolean cheat;                    // 作弊模式
    public boolean fillItems;                // 填充物品
    public boolean infiniteResources;        // 无限资源
    public boolean prebuildAi;               // 预建造 AI
    public boolean buildAi;                  // 建造 AI
    public float buildAiTier = 1f;           // AI 等级
    public boolean rtsAi;                    // RTS AI
    public int rtsMinSquad = 4;
    public int rtsMaxSquad = 50;
    public float rtsMinWeight = 1.2f;
    // ... 更多倍率字段
}
```

### 常见误区

- **waves 默认 false**：不是 true，需要手动开启
- **winWave 默认 0**：<=0 表示禁用，不是第 0 波获胜
- **waveSpacing 默认 2 分钟**：`2 * Time.toMinutes`
- **loadout 默认 100 铜**：不是空的
- **deconstructRefundMultiplier 默认 0.5f**：拆建筑返还 50%
- **enemyCoreBuildRadius 默认 400f**：不是无限
- **env 是位标志**：用 `|` 组合
- **每个队伍有独立 TeamRule**：通过 `teams.get(team)` 访问

## 4. 存档系统（SaveIO + SaveVersion）

### 版本管理

```java
// SaveIO.java
public static final Seq<SaveVersion> versionArray = Seq.with(
    new Save1(), new Save2(), ..., new Save12()
);
// 当前最新版本是 Save12
public static SaveVersion getSaveWriter() { return versionArray.peek(); }
```

### SaveVersion 关键方法

```java
// 写入
void write(DataOutputStream stream, StringMap extraTags)
void writeMeta(DataOutput stream, StringMap tags)
void writeMap(DataOutput stream)
void writeTeamBlocks(DataOutput stream)
void writeWorldEntities(DataOutput stream)
void writeEntities(DataOutput stream)
void writeMarkers(DataOutput stream)
void writeCustomChunks(DataOutput stream, boolean net)

// 读取
void read(DataInputStream stream, CounterInputStream counter, WorldContext context)
void readMeta(DataInput stream, WorldContext context)
void readMap(DataInput stream, WorldContext context)
void readTeamBlocks(DataInput stream)
void readWorldEntities(DataInput stream, Prov[] mapping)
void readEntities(DataInput stream)
void readMarkers(DataInput stream)
void readCustomChunks(DataInput stream)
```

### 存档结构

```
存档文件 (.msav):
├── Meta 块（版本、地图名、规则等）
├── Map 块（tile 数据：floor、overlay、block、build）
├── Team Blocks 块（每个队伍的建筑列表）
├── World Entities 块（实体映射）
├── Entities 块（所有实体数据）
├── Markers 块（标记数据）
├── Data Patches 块（JSON 补丁）
└── Custom Chunks 块（mod 自定义数据）
```

### 常见误区

- **存档格式是二进制**：不是 JSON，不是文本
- **版本号从 1 到 12**：当前最新是 12，旧版自动升级
- **tile 数据和实体数据分开存储**：先存 tile，再存实体
- **mod 自定义数据在 Custom Chunks**：不破坏存档兼容性
- **SaveSlot 是 Saves 的内部类**：不是独立类
