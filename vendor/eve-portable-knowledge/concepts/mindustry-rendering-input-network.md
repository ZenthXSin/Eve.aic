# Mindustry 渲染系统、网络包、输入系统源码验证

> Source: `源码和示例/Mindustry/core/src/` 直接验证，2026-06-08

## 1. Renderer（`core/Renderer.java`，629行）

### 子渲染器

```java
public class Renderer implements ApplicationListener {
    public final BlockRenderer blocks = new BlockRenderer();
    public final FogRenderer fog = new FogRenderer();
    public final MinimapRenderer minimap = new MinimapRenderer();
    public final OverlayRenderer overlays = new OverlayRenderer();
    public final LightRenderer lights = new LightRenderer();
    public final Pixelator pixelator = new Pixelator();
    public PlanetRenderer planets;  // 延迟初始化
}
```

### 字段

```java
// 全局静态变量
public static float laserOpacity = 0.5f;
public static float unitLaserOpacity = 1f;
public static float bridgeOpacity = 0.75f;

// FrameBuffer
public @Nullable Bloom bloom;
public @Nullable FrameBuffer backgroundBuffer;
public FrameBuffer effectBuffer = new FrameBuffer();

// 渲染开关
public boolean animateShields, animateWater, drawWeather = true;
public boolean drawStatus, enableEffects;
public boolean drawDisplays = true, drawLight = true;
public boolean pixelate = false, showPings = true, showOtherBuildPlans = true;
public float weatherAlpha;

// 缩放
public float minZoom = 1.5f, maxZoom = 6f;         // 过场用
public float minZoomInGame = 0.5f, maxZoomInGame = 6f; // 游戏用

// 着陆动画
private @Nullable LaunchAnimator launchAnimator;
float landTime;                    // 着陆剩余时间
float targetscale, camerascale;    // 目标/当前缩放
float shakeIntensity, shakeReduction, shakeTime; // 屏幕震动

// 环境渲染器
public Seq<EnvRenderer> envRenderers = new Seq<>();
public ObjectMap<String, Runnable> customBackgrounds = new ObjectMap<>();

// 纹理
public TextureRegion[] bubbles = new TextureRegion[16];
public TextureRegion[] splashes = new TextureRegion[12];
public TextureRegion[][] fluidFrames;
```

### 渲染管线（draw() 方法）

```
1. Layer.background     → drawBackground()
2. Layer.floor          → floor.drawFloor()
3. Layer.block - 1      → blocks.drawShadows()
4. Layer.block - 0.09f  → floor.drawLayer(CacheLayer.walls)
5. Layer.blockBuilding   → Shaders.blockbuild（建筑建造动画）
6. 环境渲染器             → 按 env 标志匹配
7. Layer.light           → lights.draw()（如果启用）
8. Layer.darkness        → blocks.drawDarkness()（如果启用）
9. Layer.bullet - 0.02f  → bloom.capture()
10. Layer.effect + 0.02f → bloom.render()
11. Layer.plans          → overlays.drawBottom()
12. Layer.shields        → effectBuffer + Shaders.shield（如果启用）
13. Layer.buildBeam      → effectBuffer + Shaders.buildBeam（如果启用）
14. 目标标记渲染
```

### update() 逻辑

1. 更新 camerascale（lerp 到 targetscale）
2. 从 settings 读取渲染选项（激光透明度、动画等）
3. 处理着陆动画（landTime 递减）
4. 更新 camera 尺寸
5. 处理屏幕震动
6. 调用 draw() 或 pixelator.drawPixelate()

### 常见误区

- **minZoom = 1.5f 是缩小**：值越大视野越小（zoom out）
- **maxZoom = 6f 是放大**：值越大视野越大（zoom in）— 等等，这是反直觉的
- **landTime 从正数递减到 0**：正值表示正在着陆
- **animateShields 默认 false**：需要在设置中开启
- **bloom 可为 null**：不是所有设备都支持
- **effectBuffer 用于水波和护盾**：不是所有特效
- **envRenderers 按环境标志匹配**：`(renderer.env & state.rules.env) == renderer.env`

## 2. BlockRenderer（`graphics/BlockRenderer.java`，650行）

### 字段

```java
public class BlockRenderer {
    public static final int crackRegions = 8, maxCrackSize = 7;
    public static final Color shadowColor = new Color(0, 0, 0, 0.71f);
    public final FloorRenderer floor = new FloorRenderer();
    public TextureRegion[][] cracks;

    // 四叉树
    private BlockQuadtree blockTree;
    private BlockLightQuadtree blockLightTree;
    private OverlayQuadtree overlayTree;
    private FloorQuadtree floorTree;

    // FrameBuffer
    private FrameBuffer shadows = new FrameBuffer();
    private FrameBuffer dark = new FrameBuffer();
}
```

### 常见误区

- **crackRegions = 8**：最大 7 级裂纹（maxCrackSize = 7）
- **shadowColor 是半透明黑色**：`(0, 0, 0, 0.71f)`
- **四叉树用于空间查询**：不是渲染数据结构
- **FloorRenderer 是 BlockRenderer 的成员**：不是独立的渲染器

## 3. 输入系统（`input/InputHandler.java`，2554行）

### 字段

```java
public abstract class InputHandler implements InputProcessor, GestureListener {
    // 逻辑过场
    public boolean logicCutscene;
    public float logicCamSpeed = 0.1f;
    public float logicCutsceneZoom = -1f;

    // 输入锁
    public Seq<Boolp> inputLocks = Seq.with(() -> renderer.isCutscene(), () -> logicCutscene);

    // 建造
    public @Nullable Block block;
    public boolean overrideLineRotation;
    public int rotation = 1;
    public boolean droppingItem;
    public float itemDepositCooldown;
    public boolean isBuilding = true;

    // 计划
    public BuildPlan resultplan;
    public BuildPlan bplan = new BuildPlan();
    public Seq<BuildPlan> linePlans = new Seq<>();
    public Seq<BuildPlan> selectPlans = new Seq<>(BuildPlan.class);

    // 指挥
    public Seq<Unit> selectedUnits = new Seq<>();
    public Seq<Building> commandBuildings = new Seq<>(false);
    public boolean commandMode = false;
    public boolean commandRect = false;
}
```

### 子类

| 类 | 平台 | 关键区别 |
|----|------|----------|
| **DesktopInput** | PC | 鼠标+键盘，拖拽建造，快捷键 |
| **MobileInput** | 移动端 | 触摸手势，长按选择，虚拟摇杆 |

### 关键静态方法

```java
static void transferItemEffect(Item item, float x, float y, Itemsc to)
static void transferItemTo(@Nullable Unit unit, Item item, int amount, float x, float y, Building build)
static void commandUnits(Player player, int[] unitIds, @Nullable Building buildTarget, @Nullable Unit unitTarget, @Nullable Vec2 posTarget, boolean queueCommand, boolean finalBatch)
static void setUnitCommand(Player player, int[] unitIds, UnitCommand command)
static void setUnitStance(Player player, int[] unitIds, UnitStance stance, boolean enable)
static void commandBuilding(Player player, int[] buildings, Vec2 target)
static void pingLocation(Player player, float x, float y, @Nullable String text)
static void requestItem(Player player, Building build, Item item, int amount)
static void transferInventory(Player player, Building build)
static void createItemTransfer(Item item, int amount, float x, float y, Position to, Runnable done)
static void deletePlans(Player player, int[] positions)
```

### 常见误区

- **InputHandler 是抽象类**：不直接实例化
- **rotation 默认 1**：不是 0
- **isBuilding 默认 true**：默认处于建造模式
- **commandMode 默认 false**：需要手动激活
- **inputLocks 有两个默认锁**：过场动画和逻辑过场
- **logicCutsceneZoom 默认 -1f**：表示未设置

## 4. 网络包（`net/Packets.java`）

### 包类型

```java
public class Packets {
    // 连接
    public static class Connect extends Packet {
        String addressTCP;
    }
    public static class Disconnect extends Packet {
        String reason;
    }
    public static class ConnectPacket extends Packet {
        int version;
        String versionType;
        Seq<String> mods;
        String name, locale, uuid, usid;
        boolean mobile;
        int color;
    }

    // 流传输
    public static class WorldStream extends Streamable {}
    public static class StreamBegin extends Packet {
        int id, total;
        byte type;
    }
    public static class StreamChunk extends Packet {
        int id;
        byte[] data;
    }
}
```

### 常见误区

- **ConnectPacket 包含 mods 列表**：用于 mod 同步检查
- **ConnectPacket 包含 uuid 和 usid**：用于身份验证
- **StreamBegin 有 type 字节**：区分不同类型的流（世界数据、地图等）
- **Disconnect 有 reason 字符串**：断开原因
- **WorldStream 继承 Streamable**：不是 Packet，是流式传输

## 5. 渲染层级参考（来自 Layer 类）

```
background=-10, floor=0, scorch=10, debris=20
blockUnder=29.5, block=30, blockCracks=30.1, blockAfterCracks=30.2
blockAdditive=31, blockProp=32, blockOver=35
blockBuilding=40, turret=50, turretHeat=50.1
groundUnit=60, power=70, legUnit=75
darkness=80, plans=85
flyingUnitLow=90, bullet=100, effect=110, flyingUnit=115
overlayUI=120, buildBeam=122, shields=125
weather=130, light=140, playerName=150
fogOfWar=155, space=160, end=200, endPixeled=210, max=220
```

### 渲染顺序关键点

1. **Floor 先于 Block**：地面在墙壁下面
2. **阴影在 block 层之前**：`Layer.block - 1`
3. **turret 在 block 之后**：炮塔有独立层
4. **子弹在单位之后**：`bullet=100 > groundUnit=60`
5. **特效在子弹之后**：`effect=110 > bullet=100`
6. **护盾在特效之后**：`shields=125 > effect=110`
7. **天气在护盾之后**：`weather=130 > shields=125`
8. **光照在天气之后**：`light=140 > weather=130`
9. **战争迷雾在光照之后**：`fogOfWar=155 > light=140`
