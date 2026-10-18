# Logic System

> 源码位置：`mindustry/logic/LExecutor.java`(2386行)、`mindustry/logic/LStatements.java`(2596行)、`mindustry/logic/LAssembler.java`(156行)、`mindustry/logic/LParser.java`(214行)、`mindustry/world/blocks/logic/LogicBlock.java`(912行)、`mindustry/world/blocks/logic/MemoryBlock.java`(118行)
> 验证版本：v136.2

## 核心架构

### 执行流程
```
代码文本 → LParser.parse() → LAssembler.assemble() → LInstruction[]
  → LExecutor.runOnce() → 逐条执行LInstruction.run(exec)
```

### 关键限制
```java
maxInstructions = 1000        // 每帧最大指令数
instructionsPerTick = 1       // 基础每tick指令数
maxInstructionScale = 5       // 最大倍率（超频）
maxInstructionsPerTick = 40   // 特权处理器最大
maxByteLen = 1024 * 100       // 代码最大字节数
maxCompressedLen = 16_000     // 压缩后最大
maxLinks = 6000               // 最大链接数
maxGraphicsBuffer = 256       // 图形缓冲区
maxDisplayBuffer = 1024       // 显示缓冲区
maxTextBuffer = 400           // 文本缓冲区
```

### LExecutor 关键字段
```java
LInstruction[] instructions;  // 指令数组
LVar[] vars;                  // 变量数组
LVar counter, unit, thisv, ipt, queryResult;  // 内置变量
int[] binds;                  // 单位绑定
Building[] links;             // 链接的建筑
boolean privileged;           // 特权处理器
Team team;                    // 队伍
```

### LVar（变量）
```java
int id;               // 变量索引
double numval;        // 数值
Object objval;        // 对象值
boolean isobj;        // 是否对象
boolean constant;     // 是否常量
String name;          // 变量名
```

## 指令类型（LInstruction实现）

### 单位控制
- **UnitBindI** — 绑定单位（按类型/范围）
- **UnitControlI** — 控制单位（move/stop/boost/build/mine等）
- **UnitLocateI** — 定位（ore/building/spawn/damaged）

### 数据操作
- **SetI** — 赋值
- **OpI** — 运算（+/-/*/÷/%/pow/sqrt/sin/cos/...）
- **SelectI** — 条件选择
- **ReadI/WriteI** — 读写内存/建筑
- **SenseI** — 感知属性（LAccess枚举）

### 建筑操作
- **GetLinkI** — 获取链接建筑
- **ControlI** — 控制建筑（enabled/config/color/shoot）
- **RadarI** — 雷达搜索

### 图形/输出
- **DrawI** — 绘制指令（line/rect/circle/polygon/text/...）
- **DrawFlushI** — 刷新到显示
- **PrintI/PrintCharI/FormatI/PrintFlushI** — 文本输出

### 流程控制
- **EndI** — 结束
- **NoopI** — 空操作（跳转目标）

## 方块类型

### LogicBlock（912行）
- **LogicBuild**：存储代码、执行器、链接列表
- **privileged**：特权处理器（可访问世界处理器等）
- **tag**：标签（特权处理器可设置）
- **iconTag**：图标标签
- **代码存储**：压缩后存入存档（`readCompressed`/`writeCompressed`）

### MemoryBlock（118行）
- **memoryCapacity=32**：默认32个double
- **MemoryBuild**：实现LReadable/LWritable
- **canPickup()=false**：防止同步问题

### SwitchBlock
- 开关方块，逻辑可控制

### MessageBlock
- 消息方块，显示文本

### LogicDisplay
- 显示方块，接收图形指令

### CanvasBlock
- 画布方块，像素级绘制

## LAccess（感知/控制属性）

### 只读（senseable）
totalItems/firstItem/totalLiquids/totalPower/itemCapacity/liquidCapacity/powerCapacity/powerNetStored/powerNetCapacity/powerNetIn/powerNetOut/ammo/ammoCapacity/health/maxHealth/heat/shield/armor/efficiency/progress/timescale/rotation/x/y/velocityX/velocityY/shootX/shootY/size/solid/dead/range/shooting/boosting/mining/team/type/flag/controlled/controller/name/id/...

### 可写（settable）
x/y/velocityX/velocityY/rotation/speed/armor/health/shield/team/flag/totalPower/payloadType/bulletTime/bulletLifetime

### 可控（controls，需要参数）
enabled/to/shoot/x,y/shootp/unit,shoot/config/to/color/to

## LUnitControl（单位控制命令）

idle/stop/move(x,y)/approach(x,y,radius)/pathfind(x,y)/autoPathfind/boost(enable)/target(x,y,shoot)/targetp(unit,shoot)/itemDrop(to,amount)/itemTake(from,item,amount)/payDrop/payTake(takeUnits)/payEnter/mine(x,y)/flag(value)/build(x,y,block,rotation,config)/deconstruct(x,y)/getBlock(x,y,type,building,floor)/within(x,y,radius,result)/unbind

## 硬性规则

1. **LExecutor**是执行器，不是编译器
2. **LAssembler**组装变量和指令，**LParser**解析文本
3. **privileged处理器**可访问世界处理器、设置tag
4. **LogicBlock.range**默认8*10=80（链接范围）
5. **代码压缩存储**：GZIPOutputStream压缩后存档
6. **maxLinks=6000**：链接数限制
7. **指令超频**：`instructionsPerTick * maxInstructionScale`
8. **MemoryBlock.canPickup()=false**：防止同步问题
9. **LVar.numf()**返回float，**numi()**返回int，**num()/numb()**返回double/boolean
10. **World Processor**：privileged=true，可执行世界级操作
11. **LogicAI**：逻辑控制单位时的AI，10秒超时
12. **图形缓冲区256条**，超过丢弃
13. **文本缓冲区400字符**，超过丢弃

## 常见误区

| 错误认知 | 正确事实 |
|----------|----------|
| 逻辑代码实时编译 | 预编译为LInstruction[]，每帧执行 |
| 所有处理器可访问世界 | 只有privileged=true可以 |
| 逻辑变量是动态类型 | LVar同时有numval和objval |
| 链接数无限制 | maxLinks=6000 |
| 图形指令直接渲染 | 先缓冲，DrawFlush时批量提交 |
| MemoryBlock可拾取 | canPickup()=false |
