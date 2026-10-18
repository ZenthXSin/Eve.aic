# AI System

> 源码位置：`mindustry/entities/units/UnitController.java`(34行)、`mindustry/entities/units/AIController.java`(450行)、`mindustry/ai/types/`(16个AI子类共2817行)、`mindustry/ai/RtsAI.java`(364行)、`mindustry/ai/BaseBuilderAI.java`(265行)
> 验证版本：v136.2

## 核心架构

### 接口层次
```
UnitController (接口)
  ├── AIController (基类，450行)
  │   ├── FlyingAI          — 飞行单位默认AI
  │   ├── GroundAI          — 地面单位默认AI
  │   │   └── SuicideAI     — 自爆单位AI（爬虫）
  │   ├── CommandAI         — 指令AI（玩家命令/stance系统）
  │   ├── LogicAI           — 逻辑处理器控制AI
  │   ├── BuilderAI         — 建造者AI
  │   ├── MinerAI           — 采矿AI
  │   ├── RepairAI          — 维修AI
  │   ├── DefenderAI        — 防御者AI（护卫）
  │   ├── MissileAI         — 导弹AI
  │   ├── CargoAI           — 货运AI
  │   ├── FlyingFollowAI    — 跟随飞行AI（波次支援）
  │   ├── HugAI             — 绕墙AI（Crawler变体）
  │   ├── BoostAI           — 加速AI（RTS专用）
  │   ├── AssemblerAI       — 组装者AI（UnitAssembler定位）
  │   └── PrebuildAI        — 预建造AI
  └── Player — 玩家控制器（不是AI）
```

### 全局AI管理器
- **RtsAI** — RTS模式的队伍级AI，管理小队分配、目标选择、建造路径
- **BaseBuilderAI** — AI队伍的基地建造AI，管理蓝图放置、路径计算

## AIController 基类（450行）

### 关键字段
```java
protected Unit unit;                    // 控制的单位
protected Interval timer = new Interval(4); // 4个定时器
protected @Nullable AIController fallback;  // 回退AI
protected float noTargetTime;           // 无目标时间
protected @Nullable Teamc target;       // 主目标
protected @Nullable Teamc bomberTarget; // 轰炸目标
protected boolean turningAway;          // 碰撞回避中
```

### 核心循环
```java
updateUnit()
  → useFallback()? → fallback.updateUnit()  // 回退AI接管
  → updateVisuals()     // 视觉（wobble、朝向）
  → updateTargeting()   // 目标选择+武器更新
  → updateMovement()    // 移动逻辑（子类重写）
```

### 目标选择
```java
findMainTarget(x, y, range, air, ground)  // 子类重写
findTarget(x, y, range, air, ground)      // 子类重写
target(x, y, range, air, ground)          // 基础：Units.closestTarget
targetFlag(x, y, flag, enemy)             // 按BlockFlag找建筑
targetFlagActive(x, y, flag, enemy)       // 按BlockFlag找活跃建筑
retarget()                                // 每40/90tick重新搜索
```

### 武器更新（updateWeapons）
```
对每个mount:
  → 跳过不可控/无攻击武器
  → 单目标模式：所有mount共享target
  → 多目标模式：每个mount独立findTarget
  → 计算射击角度（Predict.intercept）
  → 检查射程 → 设置shoot/rotate
  → 无目标5秒后回转到baseRotation
```

### 移动辅助
```java
moveTo(target, circleLength, smooth, keepDistance, offset, arrive)
circleAttack(circleLength)  // 环绕攻击
circle(target, circleLength) // 环绕移动
pathfind(pathTarget, stopAtTargetTile, avoidance) // 流场寻路
faceTarget()  // 面向目标（omniMovement单位）
prefSpeed()   // 返回unit.speed()
```

## 各AI子类详解

### FlyingAI（74行）— 飞行默认
- **目标优先级**：core > targetFlags > 最近目标
- **移动**：circleTarget时环绕攻击，否则moveTo+lookAt
- **randomWaveAI**：随机选择BlockFlag目标（core/storage/generator/launchPad/factory/repair/battery/reactor/drill）

### GroundAI（86行）— 地面默认
- **移动**：pathfind(fieldCore)，有卡住检测（stuckTime/stuckRange）
- **卡住处理**：卡住后启用avoidance，通知附近同AI单位
- **降落**：canBoost单位自动降低elevation
- **攻击**：靠近敌人核心时强制target=core

### SuicideAI（105行）— 自爆/爬虫
- 继承GroundAI
- **特殊**：直接移动到目标（不寻路），射线检测障碍
- **不攻击**：墙壁/运输方块/导管（只攻击核心）
- **绕过**：遇到障碍物会射击清除

### CommandAI（596行）— 指令系统
- **UnitCommand**：move/attack/patrol/mine/assist/idle/retreat/regroup/repair
- **UnitStance**：射击模式（holdPosition/shoot/mineAuto等）
- **commandQueue**：最多50个排队命令
- **commandController**：根据当前command切换子AI
- **group**：单位组协同

### LogicAI（158行）— 逻辑控制
- **LUnitControl**：move/approach/pathfind/autoPathfind/idle/stop/target/targetp
- **超时**：`logicControlTimeout=600tick`(10秒)，超时后resetController
- **keepState()=true**：读档后不重置状态
- **特殊**：retarget()=true（每次重新瞄准），invalid()=false（目标不失效）
- **boost**：可选悬浮

### BuilderAI（265行）— 建造者
- **建造半径**：`buildRadius=1500`
- **跟随**：可跟随其他BuilderAI协同建造
- **逃跑**：受到攻击时逃回核心
- **rebuildPeriod**：重建间隔（AI队伍=10tick，普通=120tick）

### MinerAI（93行）— 采矿
- **状态机**：mining=true采矿 → 背包满 → mining=false送回核心 → 循环
- **目标选择**：`mineItems.min()`选择核心最少的可采矿物
- **stance**：支持ItemUnitStance按矿物类型指定

### RepairAI（81行）— 维修
- **目标**：优先找`Units.findDamagedTile()`，否则用武器目标
- **回避**：不维修时远离敌人（`fleeRange=310`）
- **ConstructBuild**：跳过正在建造的方块

### DefenderAI（46行）— 护卫
- **目标**：找同队血量最高的可操控单位
- **移动**：跟随目标，保持距离
- **fallback**：无护卫目标时攻击敌人

### MissileAI（50行）— 导弹
- **特殊**：`homingDelay`后才追踪shooter.aimX/Y
- **移动**：永远向前，加速度曲线（`missileAccelTime`）
- **碰撞**：碰到敌方建筑直接kill()
- **retarget**：每4tick重新瞄准（高频）

### CargoAI（179行）— 货运
- **绑定**：BuildingTetherc，绑定到UnitCargoLoader
- **流程**：空手→回装载点→取物品→找卸载点→卸载→循环
- **目标**：按核心库存最少的物品优先

### FlyingFollowAI（61行）— 跟随飞行
- **专用**：波次支援飞行单位
- **跟随**：找血量最高的同队非同类单位
- **fallback**：PvP或非波次队伍时回退到FlyingAI

### BoostAI（23行）— RTS加速
- **专用**：RTS AI控制的boost移动单位
- **行为**：boosting=true，靠近目标时切换为moveCommand

### AssemblerAI（25行）— 组装者定位
- **专用**：UnitAssembler的单位定位
- **行为**：移动到targetPos，到达后转向targetAngle

### HugAI（62行）— 绕墙
- **行为**：射线检测障碍，有障碍时绕行（旋转90度移动）
- **类似**：SuicideAI的射线检测 + GroundAI的寻路

### PrebuildAI（250行）— 预建造
- **专用**：核心预建造单位
- **行为**：按蓝图放置建筑，完成后移动到下一个位置

## 寻路系统

### 流场寻路（Flowfield Pathfinding）
```java
pathfinder.getField(team, costType, pathTarget)
  → getNextTile(tile, avoidance)
  → 返回下一步应走的Tile
```

### costType
```java
unit.type.flowfieldPathType  // 单位的代价类型
// 不同单位类型有不同的通行代价（地面/飞行/水上等）
```

### pathTarget
```java
Pathfinder.fieldCore   // 到核心
Pathfinder.fieldRally  // 到集结点
// 其他预定义目标
```

## 硬性规则

1. **Building不参与AI**：Building有自己的更新逻辑，AI只控制Unit
2. **AIController.unit**是protected，通过unit()/unit(Unit)访问
3. **target**可为Unit或Building（都是Teamc）
4. **retarget()**默认每40/90tick重新搜索目标
5. **LogicAI.keepState()=true**：读档后不重置逻辑控制状态
6. **LogicAI超时=10秒**：无逻辑控制后自动resetController
7. **CommandAI.isLogicControllable()**：有命令时不可被逻辑控制
8. **MissileAI**不走寻路，直接向前移动+homing
9. **GroundAI有卡住检测**：卡住后启用avoidance并通知附近单位
10. **FlyingAI.findMainTarget**：core在范围内优先，否则按targetFlags
11. **SuicideAI不攻击墙壁/运输方块**：只攻击核心和单位
12. **RtsAI是队伍级管理器**：不是UnitController，管理小队分配
13. **BaseBuilderAI**填充核心物品到满（作弊行为）
14. **BuilderAI.rebuildPeriod**：AI队伍10tick，普通120tick
15. **pathfind()的avoidance参数**：卡住后启用碰撞回避
16. **wobble**：飞行单位默认抖动（type.wobble控制）
17. **faceTarget**：只对omniMovement/Mechc单位有效
18. **circleTargetRadius**：默认15f，环形攻击半径

## 常见误区

| 错误认知 | 正确事实 |
|----------|----------|
| AI控制Building | AI只控制Unit |
| LogicAI没有超时 | 10秒超时自动重置 |
| GroundAI直接移动到目标 | 使用流场寻路pathfind() |
| MissileAI有复杂行为 | 只是向前+homing |
| CommandAI是单个命令 | 有命令队列(最多50) |
| 所有AI共享同一目标选择 | 每个子类重写findTarget |
| RtsAI是UnitController | 它是队伍级管理器 |
| BuilderAI只能建造 | 还能逃跑、跟随、协助 |
| DefenderAI攻击敌人 | 优先护卫同队单位 |
