# Mindustry Building / Tile / Power / Liquid 源码验证

> Source: `源码和示例/Mindustry/core/src/` 直接验证，2026-06-08

## 1. Tile 类（`world/Tile.java`）

```java
public class Tile implements Position, QuadTreeObject, Displayable {
    public byte data, floorData, overlayData;  // 3个独立数据字节
    public int extraData;                        // 额外4字节，8字节对齐无额外开销
    public @Nullable Building build;             // 建筑实体，无建筑时为 null
    public short x, y;                           // 坐标（short，不是int）
    protected Block block;                       // 墙壁/建筑类型
    protected Floor floor;                       // 地面
    protected Floor overlay;                     // 覆盖层（矿/沙等）
}
```

**关键方法**：
- `floor()` → 返回地面 Floor
- `block()` → 返回墙壁/建筑 Block
- `overlay()` → 返回覆盖层 Floor
- `build` → 直接字段访问，不是方法
- `team()` → 从 build 获取，无 build 时返回 `Team.derelict`
- `pos()` → `Point2.pack(x, y)`
- `setBlock(type, team, rotation)` → 设置建筑类型并创建 Building

**常见误区**：
- Tile 坐标是 `short`，不是 `int`
- `block` 是 protected，通过 `block()` 方法访问
- `build` 可为 null（纯地面无建筑时）
- `data`/`floorData`/`overlayData` 是三个独立字节，分别用于不同层
- `isCenter()` → 多格建筑中只有中心 tile 返回 true

## 2. BuildingComp 组件（`entities/comp/BuildingComp.java`）

### 字段

```java
@Import float x, y, health, maxHealth;  // 从 Positionc/Healthc 导入
@Import Team team;
@Import boolean dead;

transient Tile tile;                     // 关联的 Tile
transient Block block;                   // 建筑类型
transient Seq<Building> proximity;       // 邻近建筑列表（6容量初始）
transient int cdump;                     // 当前倾倒方向索引
transient int rotation;                  // 旋转方向
transient boolean enabled = true;        // 是否启用

@Nullable PowerModule power;             // null if !block.hasPower
@Nullable ItemModule items;              // null if !block.hasItems
@Nullable LiquidModule liquids;          // null if !block.hasLiquids

transient float efficiency;              // 最终效率（所有消费者的最小值）
transient float optionalEfficiency;      // 可选消费者效率
transient float potentialEfficiency;     // 潜在效率（shouldConsume为true时的值）
transient boolean shouldConsumePower;    // 是否需要电力
```

### 模块初始化（`create()` 方法）

```java
public Building create(Block block, Team team) {
    if(block.hasItems) items = new ItemModule();
    if(block.hasLiquids) liquids = new LiquidModule();
    if(block.hasPower) {
        power = new PowerModule();
        power.graph.add(self());  // 立即加入电力网络
    }
}
```

**关键**：模块是根据 Block 的 `hasItems`/`hasLiquids`/`hasPower` 自动创建的，JSON 不需要手动创建

### 更新循环

```
update() 
  → updateConsumption()   // 计算效率
  → updateTile()          // 子类逻辑（工厂合成、炮塔射击等）
```

**updateConsumption() 逻辑**：
1. 若无消费者或作弊模式 → efficiency = 1f
2. 若 !enabled → 全部为 0
3. 遍历 nonOptionalConsumers → 取最小 efficiency
4. 遍历 optionalConsumers → 取最小 optionalEfficiency
5. 若 !shouldConsume || !productionValid → efficiency = 0
6. 调用 updateEfficiencyMultiplier()（可被子类重写）
7. 若 efficiency > 0 → 更新 updateConsumers

**delta() / edelta()**：
- `delta()` = `Time.delta * timeScale`（考虑超速）
- `edelta()` = `efficiency * delta()`（效率 × 时间增量）
- 工厂合成进度通常用 `edelta()` 推进

### 关键方法签名

| 方法 | 说明 |
|------|------|
| `handleItem(Building source, Item item)` | 接收物品并存入 items |
| `acceptItem(Building source, Item item)` | 判断是否接受物品 |
| `acceptLiquid(Building source, Liquid liquid)` | 判断是否接受液体 |
| `handleLiquid(Building source, Liquid liquid, float amount)` | 接收液体 |
| `dumpLiquid(Liquid liquid)` | 向邻近建筑倾倒液体 |
| `getPowerProduction()` | 返回发电量（默认0） |
| `shouldConsume()` | 是否应该消耗（默认 true） |
| `productionValid()` | 生产是否有效（默认 false） |
| `warmup()` | 预热值（默认0） |
| `progress()` | 合成进度（默认0） |
| `totalProgress()` | 总进度（持续递增） |
| `status()` | 返回 BlockStatus 枚举 |
| `consume()` | 触发所有消费者 |
| `config()` | 返回配置对象（默认 null） |
| `draw()` | 绘制（调用 block.drawer.draw(build)） |
| `drawLight()` | 绘制光照 |

## 3. PowerModule（`world/modules/PowerModule.java`）

```java
public class PowerModule extends BlockModule {
    public float status = 0.0f;      // 0~1，unbuffered 为供给比例，buffered 为储能比例
    public boolean init;              // 是否已初始化
    public PowerGraph graph;          // 关联的电力网络（默认 new PowerGraph()）
    public IntSeq links;              // 电力连接目标（存档用）
}
```

**关键**：
- `status` 是 0~1 浮点数，不是百分比
- `graph` 是运行时网络，存档时重建
- `links` 存储连接目标的位置 ID，用于存档恢复
- PowerModule 没有 stored/capacity 字段——储能由 PowerGraph 的电池逻辑管理

## 4. PowerGraph（`world/blocks/power/PowerGraph.java`）

```java
public class PowerGraph {
    // 关键方法：
    float getPowerBalance()         // 生产-消耗差值
    float getSatisfaction()          // 需求满足率（0~1）
    float getPowerProduced()         // 总发电量
    float getPowerNeeded()           // 总需求量
    float getBatteryStored()         // 电池当前储能
    float getBatteryCapacity()       // 电池总容量
    void transferPower(float amount) // 传输电力
    void update()                    // 每 tick 更新
    void add(Building build)         // 添加建筑到网络
    void remove(Building build)      // 从网络移除建筑
    void addGraph(PowerGraph graph)  // 合并两个网络
    void reflow(Building tile)       // 从建筑重新发现网络
}
```

**关键**：
- PowerGraph 不是 Block 字段，是 Building 的 PowerModule 持有
- 电力网络是运行时动态形成的，通过 `add`/`remove` 维护
- 建筑被摧毁时自动从 graph 移除并触发网络分裂
- `getSatisfaction()` 是判断电力是否充足的核心方法

## 5. LiquidModule（`world/modules/LiquidModule.java`）

```java
public class LiquidModule extends BlockModule {
    // 关键方法：
    Liquid current()                    // 当前主要液体
    float currentAmount()               // 当前主要液体量
    float get(Liquid liquid)            // 获取特定液体的量
    void add(Liquid liquid, float amount)
    void remove(Liquid liquid, float amount)
    void set(Liquid liquid, float amount)
    void clear()
    float getFlowRate(Liquid liquid)    // 流速
    void updateFlow()                   // 更新流速统计
}
```

**关键**：
- LiquidModule 内部用 `ObjectFloatMap<Liquid>` 存储各液体的量
- 没有显式 capacity 字段——容量由 Block.liquidCapacity 决定
- `current()` 返回量最多的液体
- 流速是运行时统计，用于 UI 显示

## 6. ItemModule（`world/modules/ItemModule.java`）

```java
public class ItemModule extends BlockModule {
    int total()                         // 总物品数
    boolean has(Item item)              // 是否有某物品
    boolean has(Item item, int amount)  // 是否有足够数量
    int get(Item item)                  // 获取某物品数量
    void add(Item item, int amount)
    void remove(Item item, int amount)
    void set(Item item, int amount)
    boolean empty()                     // 是否为空
    @Nullable Item first()              // 第一个非零物品
    @Nullable Item take()               // 取出一个物品
}
```

**关键**：
- 内部用 `int[]` 数组存储（按 item.id 索引）
- 容量由 Block.itemCapacity 决定（默认 10）
- `total()` 返回所有物品总数
- 没有单独的 "满" 判断——需要 `total() >= block.itemCapacity`

## 7. Liquid 类（`type/Liquid.java`）

```java
public class Liquid extends UnlockableContent implements Senseable {
    public boolean gas = false;              // 是否为气体
    public Color color;                      // 颜色（构造函数中初始化）
    public Color gasColor = Color.lightGray.cpy();
    public @Nullable Color barColor;         // UI 条颜色（null 则用 color）
    public Color lightColor = Color.clear.cpy();
    public float flammability;               // 可燃性（0~1）
    public float temperature = 0.5f;         // 温度（0~1，0.5为中性）
    public float heatCapacity = 0.5f;        // 热容量
    public float viscosity = 0.5f;           // 粘度（影响流速）
    public float explosiveness;              // 爆炸性（0~1）
    public boolean blockReactive = true;     // 是否与方块反应
    public boolean coolant = true;           // 是否可作冷却剂
    public boolean moveThroughBlocks = false;// 是否可穿过方块
    public boolean incinerable = true;       // 是否可焚烧
    public Effect particleEffect = Fx.none;  // 粒子特效
    public float particleSpacing = 60f;
    public float boilPoint = 2f;             // 沸点（温度超过此值蒸发）
    public boolean capPuddles = true;        // 是否限制水坑大小
    public Effect vaporEffect = Fx.vapor;    // 蒸发特效
    public boolean hidden;                   // 是否隐藏
}
```

**常见误区**：
- `temperature` 默认 0.5f，不是 0
- `heatCapacity` 默认 0.5f，不是 0
- `viscosity` 默认 0.5f，不是 0
- `blockReactive` 默认 true（会与某些方块反应）
- `coolant` 默认 true（大多数液体可作冷却剂）
- `incinerable` 默认 true（可被焚烧销毁）
- `boilPoint` 默认 2f（温度超过 2 时蒸发）
- `gas` 默认 false（大部分液体不是气体）

## 8. Block 的 @NoPatch 字段

```java
// 在 Block.java 中声明（@NoPatch 注解，不在组件系统中）
public @NoPatch boolean hasItems;
public @NoPatch boolean hasLiquids;
public @NoPatch boolean hasPower;
```

**关键**：这些字段在 Block（类型定义）上，不是 Building（实例）上。Building 通过 `block.hasItems` 访问。
GenericCrafter 构造函数中 `hasItems=true`，所有 GenericCrafter 的建筑实例都会共享这个值。
