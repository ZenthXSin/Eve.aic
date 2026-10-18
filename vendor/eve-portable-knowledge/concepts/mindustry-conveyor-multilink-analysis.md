# Mindustry Conveyor 多方向连接实现解析

## 结论
原版 Conveyor 的“多方向连接”不是在一个方块上同时输出到多个方向，而是通过 **每个建筑实例只维护相邻四方向的连接状态**，再由地块上的 `next` 指针/网络关系，把传送带串成一条有方向的链。显示上的“四向连通”来自 **纹理状态 + 连接判定**，而物品流动实际只沿当前传送方向推进。

## 核心机制

### 1. 连接对象是邻居建筑，不是全局网络
Conveyor 处在网格上，和上下左右相邻的建筑做连接判断。每个 `Building` 会根据周围是否存在可连接的同类传送带，更新自己的连接状态。

关键点：
- 连接判定只看 **相邻格**
- 连接状态用于决定：
  - 贴图怎么画
  - 物品是否能继续往前传
  - 是否形成桥接、转向、交汇等视觉形态

### 2. “多方向”本质是四向状态组合
Conveyor 的外观会根据邻居情况出现：
- 直线
- 角转
- T 型
- 十字
- 端头

这些不是独立逻辑，而是通过四个方向的布尔状态组合出来的。很多情况下，Conveyor 需要判断：
- 前方是否可输出
- 左右是否有连接
- 反向是否连回来源

然后用这些状态选择对应的 sprite / region / tile index。

### 3. 物品流动实际是“单向推进”
Conveyor 的物品并不会真的“往多方向同时送”。典型逻辑是：
- 当前格里有 item
- 目标方向是 `next` 或由旋转/邻接决定的下一格
- 满足条件则把 item 推到下一个建筑

所以，**图形上是多向连通，运行上是单向链式输送**。

### 4. 方向由放置旋转和邻接共同决定
玩家放下 Conveyor 时，初始朝向由旋转决定；之后系统会根据邻居自动微调连接表现。Mindustry 的很多运输类块都依赖：
- `rotation`
- `proximity`
- `nearby(...)`
- `neighbors` / `tile` 相对坐标

### 5. 更新链路依赖邻接刷新
当邻居新增/移除时，会触发更新：
- 重新计算连接状态
- 重新决定纹理
- 必要时重建运输网络

因此 Conveyor 的“多方向连接”不是一次性算死，而是随着局部变化动态刷新。

## 对 mod 开发的启发
如果你要做类似 Conveyor 的系统，通常要拆成两层：
1. **逻辑层**：只维护实际物品流向、下一跳、缓存和投递
2. **表现层**：根据四向邻接生成连接图样

不要把“能连到多边”理解成“能同时分发到多边”；Mindustry 原版 Conveyor 更接近 **邻接驱动的链式传输 + 多状态贴图映射**。

## 相关知识
- [Block and Building System](../mindustry/block-building-system.md)
- [Mindustry HeatBlock 4-State Tiling](mindustry-heatblock-4-state-tiling.md)
- [Mindustry Building 与 Tile 坐标关系](mindustry-building-tile-coordinate-relationship.md)
- [Mindustry Placement/HUD UI 修改](mindustry-placement-ui-hud.md)

## 备注
此页用于回答“原版 Conveyor 如何实现多方向连接”的常见疑问，强调其本质是 **邻接状态映射**，不是一个方块真实分叉输送。