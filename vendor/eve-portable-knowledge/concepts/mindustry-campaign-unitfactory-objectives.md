# Mindustry 战役系统、单位工厂、目标系统源码验证

> Source: `源码和示例/Mindustry/core/src/` 直接验证，2026-06-08

## 1. CoreBlock（`world/blocks/storage/CoreBlock.java`，882行）

### 继承链

```
Block → StorageBlock → CoreBlock
```

### 字段

```java
public class CoreBlock extends StorageBlock {
    public float thrusterLength = 14f/4f, thrusterOffset = 0f;
    public boolean isFirstTier;                    // 是否为第一级核心
    public boolean allowSpawn = true;              // 是否允许生成
    public boolean requiresCoreZone;               // 是否需要核心区域
    public boolean incinerateNonBuildable = false;  // 焚烧不可建造物品
    public UnitType unitType = UnitTypes.alpha;    // 核心单位类型
    public float landDuration = 160f;              // 着陆持续时间
    public float launchSoundVolume = 1f, landSoundVolume = 1f;
    public float landZoomFrom = 0.02f, landZoomTo = 4f; // 着陆缩放
    public float captureInvicibility = 60f * 15f;  // 占领无敌时间（15秒）
}
```

### CoreBuild 内部类

```java
public class CoreBuild extends Building implements LaunchAnimator {
    public int storageCapacity;                    // 存储容量
    public boolean noEffect = false;               // 无特效
    public float iframes = -1f;                    // 无敌帧数
    public float thrusterTime = 0f;                // 推进器时间
    public @Nullable Vec2 commandPos;              // 指令位置
}
```

### 关键方法

```java
boolean canBreak(Tile tile)        // 核心不能被拆除
boolean canReplace(Block other)    // 核心不能被替换
boolean canPlaceOn(Tile tile, Team team, int rotation) // 检查核心区域
boolean canUnload()                // 是否可以卸载物品
boolean acceptItem(Building source, Item item) // 接受物品
float launchDuration()             // 发射持续时间
boolean canPickup()                // 核心不能被拾取
boolean canControlSelect(Unit player) // 是否可控制选择
float zoomLaunch()                 // 发射缩放
```

### 常见误区

- **unitType 默认 alpha**：不是 null，核心默认生成 alpha 单位
- **landDuration 默认 160 tick**：约 2.67 秒
- **captureInvicibility 默认 15 秒**：`60f * 15f`
- **isFirstTier 是手动设置的**：不是自动判断
- **核心不能被拆除/替换**：`canBreak` 和 `canReplace` 返回 false
- **核心不能被拾取**：`canPickup()` 返回 false
- **storageCapacity 在 Building 上**：不是 Block 字段

## 2. LaunchPad（`world/blocks/campaign/LaunchPad.java`，313行）

### 字段

```java
public class LaunchPad extends Block {
    public float launchTime = 1f;                  // 发射间隔（tick，不是秒！）
    public float launchSoundPitchRand = 0.1f;
    public Sound launchSound = Sounds.padLaunch;
    public boolean acceptMultipleItems = false;     // 是否接受多种物品
    public float lightStep = 1f;
    public int lightSteps = 3;
    public float liquidPad = 2f;
    public @Nullable Liquid drawLiquid;            // 绘制用液体
    public Color lightColor = Color.valueOf("eab678");
    public Color bottomColor = Pal.darkerMetal;
}
```

### 构造函数默认值

```java
public LaunchPad(String name) {
    super(name);
    hasItems = true;
    solid = true;
    update = true;
    configurable = true;
    flags = EnumSet.of(BlockFlag.launchPad);
}
```

### LaunchPadBuild 内部类

```java
public class LaunchPadBuild extends Building {
    public float launchCounter;                    // 发射计时器
}
```

### 常见误区

- **launchTime 默认 1f**：这是 1 tick，不是 1 秒！需要手动设置
- **outputsItems() 返回 false**：发射台不输出物品到相邻建筑
- **acceptMultipleItems 默认 false**：默认只接受一种物品
- **flags 包含 BlockFlag.launchPad**：用于 AI 识别

## 3. UnitFactory（`world/blocks/units/UnitFactory.java`，492行）

### 继承链

```
Block → PayloadBlock → UnitBlock → UnitFactory
```

### 字段

```java
public class UnitFactory extends UnitBlock {
    public int[] capacities = {};                  // 物品容量数组
    public Seq<UnitPlan> plans = new Seq<>(4);     // 生产计划列表
    public float createSoundVolume = 1f;
}
```

### UnitPlan（静态内部类）

```java
public static class UnitPlan {
    public UnitType unit;                          // 单位类型
    public ItemStack[] requirements;               // 物品需求
    public float time;                             // 生产时间（tick）
}
```

### UnitFactoryBuild 内部类

```java
public class UnitFactoryBuild extends UnitBuild {
    public @Nullable Vec2 commandPos;              // 生成后指令位置
    public @Nullable UnitCommand command;          // 生成后指令
    public int currentPlan = -1;                   // 当前计划索引

    public float fraction() {
        return currentPlan == -1 ? 0 : progress / plans.get(currentPlan).time;
    }
}
```

### 常见误区

- **plans 是 Seq<UnitPlan>**：不是数组
- **currentPlan 默认 -1**：表示无计划选中
- **UnitPlan.requirements 是 ItemStack[]**：不是 Seq
- **UnitPlan.time 是 tick**：不是秒
- **capacities 默认空数组**：不指定则无容量限制

## 4. Reconstructor（`world/blocks/units/Reconstructor.java`，422行）

### 继承链

```
Block → PayloadBlock → UnitBlock → Reconstructor
```

### 字段

```java
public class Reconstructor extends UnitBlock {
    public float constructTime = 60 * 2;           // 构造时间（默认 2 秒）
    public Seq<UnitType[]> upgrades = new Seq<>(); // 升级列表（每对是 [输入, 输出]）
    public int[] capacities = {};                  // 物品容量
    public Sound createSound = Sounds.unitCreate;
    public float createSoundVolume = 1f;
}
```

### 构造函数默认值

```java
public Reconstructor(String name) {
    super(name);
    regionRotated1 = 1;
    regionRotated2 = 2;
    commandable = true;
}
```

### 关键方法

```java
UnitType unit()           // 当前正在构建的单位类型
boolean constructing()    // 是否正在构建
boolean hasUpgrade(UnitType type) // 是否有该单位的升级
UnitType upgrade(UnitType type)   // 获取升级后的单位类型
```

### 常见误区

- **upgrades 是 UnitType[] 数组的 Seq**：每对是 `[输入, 输出]`，不是单独的 UnitType
- **constructTime 默认 120 tick（2 秒）**：不是 60 tick
- **commandable 默认 true**：可指定生成后的指令
- **capacities 默认空数组**：不指定则无容量限制

## 5. UnitAssembler（`world/blocks/units/UnitAssembler.java`，770行）

### 继承链

```
Block → PayloadBlock → UnitAssembler
```

### 字段

```java
public class UnitAssembler extends PayloadBlock {
    public int areaSize = 11;                      // 区域大小
    public UnitType droneType = UnitTypes.assemblyDrone; // 无人机类型
    public int dronesCreated = 4;                  // 无人机数量
    public float droneConstructTime = 60f * 4f;    // 无人机构造时间
    public int[] capacities = {};                  // 物品容量
    public Seq<AssemblerUnitPlan> plans = new Seq<>(4); // 生产计划
    public float createSoundVolume = 1f;
}
```

### AssemblerUnitPlan（静态内部类）

```java
public static class AssemblerUnitPlan {
    public UnitType unit;
    @Nullable public Seq<PayloadStack> requirements;  // 载荷需求
    @Nullable public ItemStack[] itemReq;              // 物品需求
    @Nullable public LiquidStack[] liquidReq;          // 液体需求
    public float time;
}
```

### UnitAssemblerBuild 内部类

```java
public class UnitAssemblerBuild extends PayloadBlockBuild<BuildingPayload> {
    public UnitType unit;
    @Nullable public Seq<PayloadStack> requirements;
    public float progress, warmup, droneWarmup, powerWarmup, sameTypeWarmup;
    public float invalidWarmup = 0f;
    public int currentTier = 0;
    public int lastTier = -2;
    public boolean wasOccupied = false;
    public float droneProgress, totalDroneProgress;
    public @Nullable Vec2 commandPos;
    public Seq<Unit> units = new Seq<>();
    public Seq<UnitAssemblerModuleBuild> modules = new Seq<>();
}
```

### 常见误区

- **areaSize 默认 11**：11x11 区域
- **droneType 默认 assemblyDrone**：不是 null
- **dronesCreated 默认 4**：不是 1
- **droneConstructTime 默认 4 秒**：`60f * 4f`
- **requirements 是 PayloadStack**：不是 ItemStack，是载荷需求
- **AssemblerUnitPlan 有三种需求**：PayloadStack/ItemStack/LiquidStack，都可为 null
- **UnitAssembler 继承 PayloadBlock**：不是 UnitBlock

## 6. MapObjectives（`game/MapObjectives.java`）

### MapObjective 基类

```java
public static abstract class MapObjective implements Serializable {
    public boolean hidden;                         // 是否隐藏
    public @Nullable @Multiline String details;    // 详细描述
    public @Nullable @LogicCode String completionLogicCode; // 完成时执行的逻辑代码
    public @Unordered String[] flagsAdded = {};    // 完成时添加的标志
    public @Unordered String[] flagsRemoved = {};  // 完成时移除的标志
    public ObjectiveMarker[] markers = {};         // 标记
    public transient Seq<MapObjective> parents;    // 父目标
    private boolean completed;                     // 是否已完成
    private transient boolean depFinished;         // 依赖是否完成
}
```

### 目标类型

| 类 | 说明 |
|----|------|
| ResearchObjective | 研究某项内容 |
| ProduceObjective | 生产某物品 |
| ItemObjective | 收集物品 |
| CoreItemObjective | 核心中有物品 |
| BuildCountObjective | 建造数量 |
| UnitCountObjective | 单位数量 |
| DestroyUnitsObjective | 摧毁单位 |
| TimerObjective | 计时器 |
| DestroyBlockObjective | 摧毁方块 |
| DestroyBlocksObjective | 摧毁多个方块 |
| CommandModeObjective | 指挥模式 |
| FlagObjective | 标志检查 |
| DestroyCoreObjective | 摧毁核心 |

### 标记类型

| 类 | 说明 |
|----|------|
| ShapeTextMarker | 形状文本 |
| PointMarker | 点标记 |
| ShapeMarker | 形状标记 |
| TextMarker | 文本标记 |
| LineMarker | 线标记 |
| TextureMarker | 纹理标记 |

### 常见误区

- **目标有依赖链**：parents 必须全部完成才能执行
- **completionLogicCode 是逻辑代码**：完成时通过 `LExecutor.runLogicScript()` 执行
- **flagsAdded/flagsRemoved 在 done() 中执行**：不是 update() 中
- **markers 是 ObjectiveMarker[] 数组**：不是 Seq
- **目标系统是运行时的**：在 GameState 中管理，不在 Block 中

## 7. UnitBlock 基类（`world/blocks/units/UnitBlock.java`，42行）

```java
public class UnitBlock extends PayloadBlock {
    public UnitBlock(String name) {
        super(name);
        group = BlockGroup.units;
        outputsPayload = true;
        rotate = true;
        update = true;
        solid = true;
    }
}
```

### UnitBuild 内部类

```java
public class UnitBuild extends PayloadBlockBuild<UnitPayload> {
    public float progress, time, speedScl;
}
```

### 常见误区

- **UnitBlock 继承 PayloadBlock**：不是直接继承 Block
- **UnitBuild 持有 UnitPayload**：不是 UnitType
- **outputsPayload 默认 true**：建造完成后输出载荷
