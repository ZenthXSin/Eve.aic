# Mindustry 逻辑处理器、Unit AI、Effect 系统源码验证

> Source: `源码和示例/Mindustry/core/src/` 直接验证，2026-06-08

## 1. LogicBlock（`world/blocks/logic/LogicBlock.java`，912行）

### Block 字段

```java
public class LogicBlock extends Block {
    public int maxInstructionScale = 5;       // 最大指令倍率
    public int instructionsPerTick = 1;       // 每 tick 指令数（默认 1）
    public int maxInstructionsPerTick = 40;   // 特权处理器最大指令数
    public float range = 8 * 10;              // 链接范围（默认 80）
}
```

**构造函数默认值**：
- `update = true`
- `solid = true`
- `configurable = true`

**静态常量**：
- `maxByteLen = 1024 * 100`（100KB）
- `maxCompressedLen = 16_000`
- `maxLinks = 6000`
- `maxNameLength = 32`

### LogicBuild 内部类

```java
public class LogicBuild extends Building implements Ranged, LReadable, LWritable {
    public String code = "";                    // 逻辑源码（汇编语句列表）
    public LExecutor executor = new LExecutor(); // 执行器
    public float accumulator = 0;               // 累加器（用于 ipt 计时）
    public Seq<LogicLink> links = new Seq<>();  // 链接列表
    public boolean checkedDuplicates = false;
    public int ipt = instructionsPerTick;       // 当前每 tick 指令数（特权处理器可动态修改）
    public @Nullable String tag;                // 显示名称（世界处理器专用）
    public char iconTag;
    public @Nullable LVar linksVar;
    public @Nullable Runnable loadBlock;        // 加载后执行的代码块
}
```

**初始化块**：
```java
{
    executor.privileged = privileged;  // 从 Block 继承
    executor.build = this;
}
```

### 常见误区

- **instructionsPerTick 默认 1**：不是 10，不是 100
- **maxInstructionsPerTick 是特权处理器专用**：默认 40，普通处理器用 instructionsPerTick
- **range 默认 80**：`8 * 10`，不是无限
- **maxLinks 是 6000**：不是无限
- **code 是汇编格式**：不是 LISP 格式（LISP 是编辑器界面，存储时转为汇编）
- **executor 在 Building 创建时就初始化**：不是运行时才创建
- **privileged 是 Block 字段**：不是 Building 字段，所有实例共享

## 2. LExecutor（`logic/LExecutor.java`，2386行）

### 字段

```java
public class LExecutor {
    public static int maxInstructions = 1000;    // 最大指令数（静态）
    public static final int maxGraphicsBuffer = 256;
    public static final int maxDisplayBuffer = 1024;
    public static final int maxTextBuffer = 400;

    public LInstruction[] instructions = {};     // 指令数组
    public LVar[] vars = {};                     // 变量数组（网络同步用）
    public LVar counter, unit, thisv, ipt, queryResult; // 内置变量
    public int[] binds;                          // 绑定数组
    public boolean yield;                        // 是否让出执行
    public LongSeq graphicsBuffer = new LongSeq(); // 图形缓冲区
    public StringBuilder textBuffer = new StringBuilder(); // 文本缓冲区
    public Building[] links = {};                // 链接的建筑数组
    public @Nullable LogicBuild build;           // 所属建筑
    public IntSet linkIds = new IntSet();        // 链接 ID 集合
    public Team team = Team.derelict;
    public boolean privileged = false;           // 是否特权模式
}
```

### 常见误区

- **maxInstructions 是 1000**：不是无限，超过会中断
- **graphicsBuffer 上限 256**：不是无限
- **textBuffer 上限 400 字符**：不是无限
- **yield 标志**：用于让出执行，下次 tick 继续
- **privileged 模式**：可访问世界处理器、绕过权限检查
- **vars 是网络同步用的**：常量不在 vars 中，通过 constobj/constvals 存储

## 3. MemoryBlock（`world/blocks/logic/MemoryBlock.java`，119行）

### Block 字段

```java
public class MemoryBlock extends Block {
    public int memoryCapacity = 32;  // 内存容量（默认 32 个 double）
}
```

**构造函数默认值**：
- `destructible = true`
- `solid = true`
- `group = BlockGroup.logic`
- `drawDisabled = false`
- `envEnabled = Env.any`
- `canOverdrive = false`

### MemoryBuild 内部类

```java
public class MemoryBuild extends Building implements LReadable, LWritable {
    public double[] memory = new double[memoryCapacity]; // 内存数组
}
```

### 常见误区

- **memoryCapacity 默认 32**：不是 64，不是 1024
- **存储类型是 double[]**：不是 float[]，不是 int[]
- **canOverdrive = false**：内存块不能被超速
- **privileged 内存块不可被破坏**：`damage()` 中特权块直接 return
- **越界访问返回 NaN**：不是 0，不是抛异常
- **canPickup = false**：内存块不能被拾起（同步问题）

## 4. AIController（`entities/units/AIController.java`，450行）

### 字段

```java
public class AIController implements UnitController {
    protected Unit unit;
    protected Interval timer = new Interval(4);  // 4 个定时器
    protected @Nullable AIController fallback;    // 备用 AI
    protected float noTargetTime;                 // 无目标时间
    protected @Nullable Teamc target;             // 主目标
    protected @Nullable Teamc bomberTarget;       // 轰炸目标
    protected boolean turningAway;                // 是否正在转向
}
```

### 更新循环

```java
public void updateUnit() {
    if(useFallback() && fallback != null) {
        fallback.updateUnit();  // 使用备用 AI
        return;
    }
    updateVisuals();    // 视觉更新
    updateTargeting();  // 目标选择
    updateMovement();   // 移动控制
}
```

### 关键方法

| 方法 | 说明 |
|------|------|
| `updateMovement()` | 更新移动（子类重写） |
| `updateTargeting()` | 更新目标选择 |
| `updateWeapons()` | 更新武器射击 |
| `findTarget()` | 查找目标 |
| `findMainTarget()` | 查找主目标 |
| `targetFlag()` | 按 BlockFlag 查找目标 |
| `pathfind()` | 寻路 |
| `shouldFire()` | 是否应该开火 |
| `invalid()` | 目标是否无效 |
| `retarget()` | 是否需要重新选目标 |

## 5. AI 类型层次

```
AIController (base)
├── FlyingAI          — 飞行单位默认 AI
├── GroundAI          — 地面单位默认 AI
│   └── SuicideAI     — 自杀单位 AI
├── CommandAI         — 指令 AI（玩家下达命令）
├── LogicAI           — 逻辑控制 AI（处理器控制）
├── BuilderAI         — 建造者 AI
├── MinerAI           — 采矿者 AI
├── RepairAI          — 维修者 AI
├── DefenderAI        — 防御者 AI
├── MissileAI         — 导弹 AI
├── BoostAI           — 推进 AI
├── AssemblerAI       — 组装者 AI
├── HugAI             — 贴墙 AI
├── CargoAI           — 货运 AI
├── PrebuildAI        — 预建造 AI
└── EffectDroneAI     — 无人机中心 AI（内部类）
```

### LogicAI 字段

```java
public class LogicAI extends AIController {
    public float moveX, moveY, moveRad;   // 移动目标坐标和半径
    public float controlTimer;             // 控制超时计时器
    public float targetTimer;              // 目标计时器
    public @Nullable Building controller;  // 控制器建筑
    public boolean boost;                  // 是否推进
    public boolean shoot;                  // 是否射击
}
```

### 常见误区

- **LogicAI 不是默认 AI**：只有被处理器 `control` 指令控制时才切换
- **controlTimer 是超时机制**：超时后自动恢复默认 AI
- **CommandAI 有 commandQueue**：命令队列，不是单个命令
- **SuicideAI 继承 GroundAI**：不是直接继承 AIController
- **MissileAI 是独立 AI**：不是 FlyingAI 的子类
- **fallback AI 机制**：某些 AI 可以使用备用 AI（如 BuilderAI 用 MinerAI 做 fallback）

## 6. UnitCommand（`ai/UnitCommand.java`）

```java
public class UnitCommand extends MappableContent {
    public boolean switchToMove = true;    // 切换时是否移动
    public boolean drawTarget = false;     // 是否绘制目标
    public boolean resetTarget = true;     // 切换时是否重置目标
    public boolean snapToBuilding = false; // 是否吸附到建筑
    public boolean exactArrival = false;   // 是否精确到达
    public boolean refreshOnSelect = false;// 选择时是否刷新
    public @Nullable KeyBind keybind = null; // 绑定的按键
}
```

**关键**：UnitCommand 是 MappableContent 的子类，可以通过 JSON mod 添加自定义命令

## 7. UnitStance（`ai/UnitStance.java`）

```java
public class UnitStance extends MappableContent {
    public String icon;
    public @Nullable KeyBind keybind;
    public boolean toggle = true;  // 是否可切换
}
```

**关键**：UnitStance 也是 MappableContent，可被 mod 扩展

## 8. Effect 系统（`entities/Effect.java`，331行）

### 字段

```java
public class Effect {
    public static final Seq<Effect> all = new Seq<>(); // 所有特效注册表
    public final int id;                               // 自动分配 ID
    public Cons<EffectContainer> renderer = e -> {};   // 渲染函数
    public float lifetime = 50f;                       // 持续时间（tick）
    public float clip;                                 // 裁剪大小
    public float startDelay;                           // 开始延迟
    public float baseRotation;                         // 基础旋转
    public boolean followParent = true;                // 是否跟随父对象
    public boolean rotWithParent;                      // 是否随父对象旋转
    public float layer = Layer.effect;                 // 渲染层（默认 Layer.effect = 110）
    public float layerDuration;                        // 层持续时间
}
```

### 构造函数

```java
public Effect(float life, float clipsize, Cons<EffectContainer> renderer) // 完整构造
public Effect(float life, Cons<EffectContainer> renderer) // clipsize 默认 50
public Effect()  // 空构造（自定义实现用）
```

### at() 方法（触发特效）

```java
void at(Position pos)                           // 默认白色，无旋转
void at(Position pos, boolean parentize)        // 可选跟随
void at(Position pos, float rotation)           // 指定旋转
void at(float x, float y)                       // 最简
void at(float x, float y, float rotation)       // 指定旋转
void at(float x, float y, float rotation, Color color)  // 指定颜色
void at(float x, float y, Color color)          // 指定颜色无旋转
void at(float x, float y, float rotation, Color color, Object data) // 完整
void at(float x, float y, float rotation, Object data) // 指定数据
```

### create() 逻辑

```java
public void create(float x, float y, float rotation, Color color, Object data) {
    if(!shouldCreate()) return;  // headless 或 Fx.none 或特效关闭时跳过
    // 摄像机裁剪检查
    if(Core.camera.bounds(...).overlaps(...)) {
        if(!initialized) { initialized = true; init(); }
        if(startDelay <= 0f) add(...);
        else Time.run(startDelay, () -> add(...));
    }
}
```

### EffectContainer 内部类

```java
public static class EffectContainer implements Scaled {
    public float x, y, time, lifetime, rotation;
    public Color color;
    public int id;
    public Object data;  // 附加数据（可以是 Unit、Building 等）
}
```

### 常见误区

- **lifetime 默认 50 tick**：不是 1 秒（60 tick），是 50/60 ≈ 0.83 秒
- **layer 默认 Layer.effect (110)**：不是 Layer.flyingUnit (115)
- **followParent 默认 true**：特效会跟随 data 中的 Posc 对象
- **at() 最终调用 create()**：create 做裁剪检查和延迟处理
- **shouldCreate() 检查 headless**：服务器端不渲染特效
- **data 可以是任何 Object**：通常传 Unit 或 Building 实例作为父对象
- **startDelay 是延迟触发**：不是延长生命周期
- **baseRotation 是叠加到 rotation 上的**：不是替代
