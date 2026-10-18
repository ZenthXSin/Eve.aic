# Mindustry Planet / Sector / Weather / Payload 源码验证

> Source: `源码和示例/Mindustry/core/src/` 直接验证，2026-06-08

## 1. Planet 类（`type/Planet.java`，700行）

### 构造函数

```java
public Planet(String name, Planet parent, float radius)
public Planet(String name, Planet parent, float radius, int sectorSize)
```

**构造时自动计算**：
- `orbitRadius` = parent.totalRadius + parent.orbitSpacing + totalRadius（parent为null时为0）
- `orbitTime` = pow(orbitRadius, 1.5) * 1000（开普勒第三定律）
- `orbitOffset` = Mathf.randomSeed(id + 1, 360)
- `solarSystem` = 沿 parent 链向上追溯到 parent==null 的那个
- 自动加入 parent.children
- `allowCampaignRules` = isVanilla()

**sectorSize 构造函数额外**：
- 创建 `PlanetGrid.create(sectorSize)` → 生成 grid
- 为每个 grid tile 创建 Sector → 加入 sectors 列表
- 计算 sectorApproxRadius

### 字段

```java
// 核心关系
public @Nullable Planet parent;           // 父行星（太阳为 null）
public Planet solarSystem;                // 所属恒星系（parent链顶端）
public Seq<Planet> children = new Seq<>(); // 子行星列表
public Seq<Sector> sectors = new Seq<>();  // 扇区列表

// 物理参数
public float radius;                       // 星球半径
public float orbitRadius;                  // 公转半径（自动计算）
public float orbitTime;                    // 公转周期（自动计算）
public float orbitSpacing = 12f;           // 轨道间距
public float totalRadius;                  // 总半径（含子星）
public float rotateTime = 24f * 60f;       // 自转周期
public float orbitOffset;                  // 轨道偏移角
public float camRadius;                    // 相机半径
public float minZoom = 0.5f, maxZoom = 2f;

// 渲染
public boolean drawOrbit = true;
public float atmosphereRadIn = 0, atmosphereRadOut = 0.3f;
public boolean hasAtmosphere = true;
public boolean bloom = false;
public boolean visible = true;
public Color atmosphereColor = new Color(0.3f, 0.7f, 1.0f);
public Color landCloudColor = new Color(1f, 1f, 1f, 0.5f);
public Color lightColor = Color.white.cpy();
public Color iconColor = Color.white.cpy();

// 环境
public int defaultEnv = Env.terrestrial | Env.spores | Env.groundOil | Env.groundWater | Env.oxygen;
public boolean tidalLock = false;
public boolean updateLighting = true;
public float lightSrcFrom = 0f, lightSrcTo = 0.8f, lightDstFrom = 0.2f, lightDstTo = 1f;

// 战役
public boolean accessible = true;
public int startSector = 0;
public int sectorSeed = -1;
public float launchCapacityMultiplier = 0.25f;
public boolean allowLaunchSchematics = false;
public boolean allowLaunchLoadout = false;
public boolean allowSectorInvasion = false;
public boolean allowLegacyLaunchPads = false;
public boolean clearSectorOnLose = false;
public boolean prebuildBase = true;
public boolean allowWaves = false;
public boolean allowLaunchToNumbered = true;
public boolean allowCampaignRules = false;
public boolean allowSelfSectorLaunch;
public boolean autoAssignPlanet = true;
public boolean enemyInfiniteItems = true;
public boolean enemyCoreSpawnReplace = false;
public float enemyBuildSpeedMultiplier = 1f;
public float enemyFactoryActivationDelay = 0;

// 生成器
public @Nullable PlanetGrid grid;
public @Nullable PlanetGenerator generator;
public @Nullable GenericMesh mesh, cloudMesh;
public @Nullable Mesh gridMesh;

// 科技树
public @Nullable TechNode techTree;
public Seq<UnlockableContent> unlockedOnLand = new Seq<>();

// 其他
public Seq<Planet> launchCandidates = new Seq<>();
public @Nullable Planet statParent;
public boolean showRtsAIRule = false;
public String icon = "planet";
```

### init() 逻辑

```java
public void init() {
    applyDefaultRules(campaignRules);
    loadRules();
    loadStats();
    if(techTree == null) techTree = TechTree.roots.find(n -> n.planet == this);
    // 为每个 sector 加载信息
    for(Sector sector : sectors) sector.loadInfo();
    // 生成器生成每个 sector
    if(generator != null) {
        for(Sector sector : sectors) generator.generateSector(sector);
        updateBaseCoverage();
    }
}
```

### 常见误区

- **parent 可为 null**：太阳/恒星的 parent 为 null
- **orbitRadius 是自动计算的**：不是手动设置
- **sectorSize 必须 > 0**：否则 sectors 为空，不能着陆
- **generator 在 init() 中运行**：不是 load()，不是构造函数
- **techTree 自动查找**：从 TechTree.roots 中找匹配的 planet
- **defaultEnv 是位标志**：用 `|` 组合，不是枚举
- **launchCapacityMultiplier 默认 0.25f**：不是 1f

## 2. Sector 类（`type/Sector.java`）

```java
public class Sector {
    public Planet planet;                        // 所属行星
    public Ptile tile;                           // 行星网格 tile
    public @Nullable SaveSlot save;              // 存档槽
    public @Nullable SectorPreset preset;        // 预设（如有）
    public @Nullable Sector shieldTarget;        // 护盾目标
    public SectorInfo info = new SectorInfo();   // 扇区信息
    public float threat;                         // 威胁等级（0~1）
    public boolean generateEnemyBase;            // 是否生成敌方基地
}
```

**关键方法**：
- `near()` → 返回邻近扇区列表
- `unlocked()` → 是否已解锁
- `isShielded()` → 是否被护盾保护
- `isAttacked()` → 是否正在被攻击
- `hasBase()` → 是否有玩家基地
- `hasEnemyBase()` → 是否有敌方基地
- `isBeingPlayed()` → 是否正在被游玩
- `isCaptured()` → 是否已占领
- `name()` → 扇区名称
- `items()` → 返回 ItemSeq（资源统计）

**常见误区**：
- Sector 不是 UnlockableContent 的子类（Planet/SectorPreset 是）
- Sector 的威胁等级 `threat` 是 float（0~1），不是 int
- `preset` 可为 null（程序生成的扇区没有 preset）

## 3. SectorPreset 类（`type/SectorPreset.java`，124行）

```java
public class SectorPreset extends UnlockableContent {
    public FileMapGenerator generator;             // 地图生成器
    public Planet planet;                          // 所属行星
    public Sector sector;                          // 关联扇区
    public int captureWave = 0;                    // 占领波次（0=不限）
    public Cons<Rules> rules = rules -> rules.winWave = captureWave; // 规则修改器
    public float difficulty;                       // 难度 0-10
    public float startWaveTimeMultiplier = 2f;     // 开始波次时间倍率
    public boolean addStartingItems = false;       // 是否添加起始物品
    public boolean noLighting = false;             // 是否禁用光照
    public boolean isLastSector;                   // 是否为最后一个扇区
    public boolean requireUnlock = true;           // 是否需要解锁才能着陆
    public boolean showHidden = false;             // 是否显示隐藏扇区
    public boolean showSectorLandInfo = true;      // 是否显示着陆信息
    public boolean overrideLaunchDefaults = false;  // 是否覆盖发射默认值
    public boolean allowLaunchSchematics = false;
    public boolean allowLaunchLoadout = false;
    public boolean attackAfterWaves = false;       // 波次后切换为攻击模式
    public int originalPosition;                   // 原始位置（迁移用）
    public Seq<Sector> shieldSectors = new Seq<>(); // 护盾扇区列表
}
```

**构造函数**：
```java
public SectorPreset(String name, Planet planet, int sector)
public SectorPreset(String name, String fileName, Planet planet, int sector)
```

**initialize() 逻辑**：
1. 设置 planet 和 generator（FileMapGenerator）
2. 自动重映射 sector 位置（基于 PlanetData）
3. 调用 `planet.preset(sector, this)` 注册

**常见误区**：
- `generator` 类型是 `FileMapGenerator`，不是 `PlanetGenerator`
- `captureWave` 默认 0（表示不限，不是第0波）
- `requireUnlock` 默认 true（需要解锁）
- `rules` 是 `Cons<Rules>` 函数，不是 Rules 对象

## 4. Weather 类（`type/Weather.java`，363行）

### 字段

```java
public float duration = 10f * Time.toMinutes;  // 持续时间（默认10分钟）
public float opacityMultiplier = 1f;            // 透明度倍率
public Sound sound = Sounds.none;               // 音效
public float soundVol = 0.1f, soundVolMin = 0f;
public float soundVolOscMag = 0f, soundVolOscScl = 20f;
public boolean hidden = false;                  // 是否隐藏

public Prov<WeatherState> type = WeatherState::create;  // 状态构造器
public StatusEffect status = StatusEffects.none;        // 施加的状态效果
public float statusDuration = 60f * 2;                  // 状态持续时间（默认2秒）
public boolean statusAir = true, statusGround = true;   // 对空/对地生效
```

### WeatherEntry（内部类）

```java
public static class WeatherEntry {
    public float minFrequency, maxFrequency;  // 频率范围
    public float minDuration, maxDuration;    // 持续时间范围
    public float cooldown;                    // 冷却时间
    public float intensity = 1f;              // 强度
    public boolean always = false;            // 是否永久
}
```

### Weather 子类

| 子类 | 关键字段 |
|------|----------|
| **ParticleWeather** | particleRegion, color, yspeed, xspeed, density, sizeMin, sizeMax, force, noiseScale, drawNoise, drawParticles, useWindVector |
| **RainWeather** | liquid (默认 water), yspeed, xspeed, density, stroke, sizeMin, sizeMax, color |
| **SolarFlare** | 无额外字段（继承 Weather） |
| **MagneticStorm** | 无额外字段（继承 Weather） |

### ParticleWeather 字段详情

```java
public String particleRegion = "circle-shadow";
public Color color = Color.white.cpy();
public float yspeed = -2f, xspeed = 0.25f;
public float padding = 16f;
public float sizeMin = 2.4f, sizeMax = 12f;
public float density = 1200f;
public float minAlpha = 1f, maxAlpha = 1f;
public float force = 0;
public float noiseScale = 2000f, baseSpeed = 6.1f;
public float sinSclMin = 30f, sinSclMax = 80f;
public float sinMagMin = 1f, sinMagMax = 7f;
public Color noiseColor = color;
public boolean drawNoise = false, drawParticles = true;
public boolean useWindVector = false, randomParticleRotation = false;
public int noiseLayers = 1;
public float noiseLayerSpeedM = 1.1f, noiseLayerAlphaM = 0.8f;
public float noiseLayerSclM = 0.99f, noiseLayerColorM = 1f;
public String noisePath = "noiseAlpha";
public @Nullable Texture noise;
```

### 常见误区

- `duration` 默认 10 分钟（`10f * Time.toMinutes`），不是 1 分钟
- `statusDuration` 默认 2 秒（`60f * 2`），不是 1 秒
- ParticleWeather 的 `force` 默认 0（无推力）
- RainWeather 的 `liquid` 默认 `Liquids.water`，不是 null
- SolarFlare 和 MagneticStorm 几乎无额外字段
- Weather 不是持续运行的——由 WeatherState 管理生命周期

## 5. Payload 系统

### Payload 接口（`world/blocks/payloads/Payload.java`）

```java
public interface Payload extends Position {
    int payloadUnit = 0, payloadBlock = 1;  // 类型标识
    void set(float x, float y, float rotation);
    void draw();
    void drawShadow(float alpha);
    float size();
    float buildTime();
    boolean contentEquals(Payload other);
    void write(Writes write);
}
```

**两个实现**：
- `BuildPayload`：包装 Building，`build` 字段为 Building 实例
- `UnitPayload`：包装 Unit

### PayloadBlock（`world/blocks/payloads/PayloadBlock.java`）

```java
public class PayloadBlock extends Block {
    public float payloadSpeed = 0.7f;        // 载荷移动速度
    public float payloadRotateSpeed = 5f;    // 载荷旋转速度
    public String regionSuffix = "";
}
```

**构造函数默认值**：
- `update = true`
- `sync = true`
- `group = BlockGroup.payloads`
- `acceptsUnitPayloads = true`
- `envEnabled |= Env.space | Env.underwater`

**Building 内部类字段**：
- `@Nullable T payload` — 当前持有的载荷
- `float payRotation` — 载荷旋转角度
- `boolean carried` — 是否被携带

### PayloadConveyor（`world/blocks/payloads/PayloadConveyor.java`）

```java
public class PayloadConveyor extends Block {
    public float moveTime = 45f;             // 移动时间
    public float moveForce = 201f;           // 推力
    public Interp interp = Interp.pow5;      // 插值函数
    public float payloadLimit = 3f;          // 载荷大小限制
    public boolean pushUnits = true;         // 是否推动单位
}
```

**构造函数默认值**：
- `size = 3`（3x3）
- `rotate = true`
- `outputsPayload = true`
- `noUpdateDisabled = true`
- `acceptsUnitPayloads = true`
- `underBullets = true`
- `sync = true`

**Building 内部类字段**：
- `@Nullable Payload item` — 当前载荷
- `float progress, itemRotation, animation`
- `@Nullable Building next` — 下一个建筑
- `boolean blocked` — 是否被阻挡
- `int step, stepAccepted` — 步骤

### 常见误区

- **PayloadBlock 不是 PayloadConveyor 的父类**：两者都直接继承 Block
- **PayloadConveyor 默认 size=3**：不是 1
- **Payload 接口不是类**：BuildPayload 和 UnitPayload 是两个独立实现
- **BuildPayload 构造需要 Block+Team**：不是只传 Block
- **payloadLimit 默认 3f**：载荷最大尺寸限制
