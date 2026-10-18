# Vars System

## 概述
`mindustry.Vars` 是全局静态变量的容器，实现了 `Loadable` 接口。它是整个游戏的中枢，持有所有子系统的引用。几乎所有核心类都通过 `Vars.xxx` 访问。

## 核心字段分类

### 1. 游戏状态
```java
public static GameState state;          // 游戏状态（wave/tick/gameOver/won）
public static Rules rules;              // 游戏规则（快捷访问 state.rules）
public static Map emptyMap;             // 空地图
public static Tile emptyTile;           // 空瓦片（用于载荷）
public static @Nullable Player player;  // 当前玩家（客户端）
```

### 2. 世界系统
```java
public static World world;              // 世界管理器
public static Maps maps;                // 地图管理器
public static WaveSpawner spawner;      // 波次生成器
public static BlockIndexer indexer;     // 方块索引器
public static Pathfinder pathfinder;    // 寻路器
public static FogControl fogControl;    // 迷雾控制
```

### 3. 控制系统
```java
public static Control control;          // 控制器（输入/摄像机）
public static Logic logic;              // 逻辑系统
public static Renderer renderer;        // 渲染器
public static UI ui;                    // UI 系统
```

### 4. 网络系统
```java
public static Net net;                  // 网络协议层
public static NetServer netServer;      // 服务端
public static NetClient netClient;      // 客户端
```

### 5. 内容系统
```java
public static ContentLoader content;    // 内容加载器
public static Mods mods;                // Mod 管理器
public static Schematics schematics;    // 蓝图管理器
```

### 6. 文件系统
```java
public static Fi dataDirectory;         // 数据目录
public static Fi saveDirectory;         // 存档目录
public static Fi modDirectory;          // Mod 目录
public static Fi schematicDirectory;    // 蓝图目录
public static Fi customMapDirectory;    // 自定义地图目录
```

### 7. 平台标志
```java
public static boolean headless;         // 无头服务器模式
public static boolean mobile;           // 移动端
public static boolean ios;              // iOS
public static boolean android;          // Android
public static boolean steam;            // Steam 平台
public static boolean clientLoaded;     // 客户端是否加载完成
```

### 8. 游戏常量
```java
// 尺寸
public static final int tilesize = 8;                    // 瓦片尺寸（8像素）
public static final float tilePayload = 64f;             // 瓦片载荷（8*8）
public static final int maxBlockSize = 16;               // 最大方块尺寸

// 范围
public static final float mineTransferRange = 220f;      // 矿机自动传输范围
public static final float buildingRange = 220f;          // 建造范围
public static final float itemTransferRange = 220f;      // 物品传输范围
public static final float logicItemTransferRange = 45f;  // 逻辑单位物品范围

// 时间
public static final float turnDuration = 2 * 60 * 60;    // 回合时长（2分钟）
public static final float baseInvasionChance = 0.01f;    // 入侵概率（1%）
public static final float invasionGracePeriod = 20;      // 入侵保护期（分钟）

// 战斗
public static final float minArmorDamage = 0.1f;         // 最小护甲伤害（10%）
public static final float unitCollisionRadiusScale = 0.6f; // 单位碰撞半径缩放

// 网络
public static final int port = 6567;                     // 默认端口
public static final int maxTcpSize = 1100;               // 最大 TCP 包大小
public static final int maxTextLength = 150;             // 最大聊天长度
public static final int maxNameLength = 40;              // 最大名字长度

// 内容
public static final int minModGameVersion = 136;         // 最低 Mod 游戏版本
public static final int minJavaModGameVersion = 154;     // 最低 Java Mod 版本
public static final String schematicBaseStart = "bXNjaA"; // 蓝图 base64 前缀
```

## 初始化顺序

```java
// Vars.init() 调用顺序：
1. loadSettings()           // 加载设置
2. Groups.init()            // 初始化实体组
3. Version.init()           // 版本信息
4. CacheLayer.init()        // 缓存层
5. dataDirectory = ...      // 设置目录
6. mods = new Mods()        // Mod 管理器
7. content = new ContentLoader()  // 内容加载器
// ... 其他初始化
```

## 关键设计

### 1. 全局访问
- 所有核心系统通过 `Vars.xxx` 静态字段访问
- 避免了依赖注入的复杂性
- 缺点：强耦合，测试困难

### 2. 平台检测
- `headless` — 无头服务器（无 GUI）
- `mobile` — 移动设备
- `steam` — Steam 平台
- 不同平台有不同的输入处理和 UI

### 3. 版本控制
- `minModGameVersion = 136` — 所有 Mod 最低版本
- `minJavaModGameVersion = 154` — Java Mod 最低版本（更高，因为 API 变更多）
- Mod 必须声明兼容版本

### 4. 文件扩展名
- `.msav` — 存档和地图文件
- `.msch` — 蓝图文件

## 常见误区

### ❌ 错误：直接 new Vars()
- Vars 是工具类，不能实例化
- 所有字段都是 static

### ❌ 错误：在 init() 前访问子系统
- `Vars.init()` 前很多字段为 null
- 必须在游戏启动后访问

### ❌ 错误：修改常量
- `tilesize`、`maxBlockSize` 等是 final
- 修改会导致未定义行为

### ✅ 正确：通过 Vars 访问子系统
```java
// 正确
Vars.world.tile(x, y);
Vars.state.rules.waves;

// 错误
new World().tile(x, y);
```

## 关联
- [Core Package Overview](core-packages.md) — 核心包结构
- [Key Classes Analysis](key-classes.md) — 关键类分析
- [Mindustry Knowledge Structure](structure-overview.md) — 知识库总结构
