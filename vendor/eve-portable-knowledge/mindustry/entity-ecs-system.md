# Entity and ECS System

> 源码位置：`mindustry/entities/comp/`(48个组件文件)、`mindustry/entities/GroupDefs.java`、`mindustry/entities/EntityGroup.java`(290行)、`mindustry/annotations/`(注解处理器)
> 验证版本：v136.2

## 核心架构

Mindustry 使用 **编译时组件系统**（不是运行时ECS），通过注解处理器在编译期生成实体类。

### 工作流程
```
@Component → 编译时注解处理器 → 生成具体实体类（Unit/Bullet/Building/...）
@EntityDef → 定义实体包含哪些组件接口
@Import → 从其他组件导入字段（不重复存储）
@Replace → 方法覆盖
@Final → 方法不可覆盖
@MethodPriority → 方法执行顺序
```

### 组件（48个）

#### 基础组件
- **EntityComp** — 实体基类（id、added、add/remove、read/write）
- **HealthComp** — 血量（health/maxHealth/dead/damage/kill/heal）
- **HitboxComp** — 碰撞箱（hitSize）
- **Posc** — 位置（x/y）
- **Rotc** — 旋转（rotation）
- **Teamc** — 队伍（team）
- **Drawc** — 绘制
- **Syncc** — 网络同步

#### 单位组件
- **UnitComp** — 单位核心（986行，最大组件之一）
- **Weaponsc** — 武器系统
- **Statusc** — 状态效果
- **Shieldc** — 护盾
- **Itemsc** — 物品
- **Builderc** — 建造能力
- **Minerc** — 采矿能力
- **LegsComp** — 腿部移动
- **MechComp** — 机甲移动
- **TankComp** — 坦克移动
- **ElevationMoveComp** — 飞行移动
- **PayloadComp** — 载荷
- **SegmentComp** — 蛇形单位

#### 建筑组件
- **BuildingComp** — 建筑核心（2274行，最大组件）

#### 子弹组件
- **BulletComp** — 子弹核心（402行）

#### 其他组件
- **PlayerComp** — 玩家（495行）
- **FireComp** — 火焰
- **PuddleComp** — 液体坑
- **EffectStateComp** — 特效状态
- **DrawComp** — 绘制状态
- **BlockUnitComp** — 方块控制的单位
- **BuildingTetherComp** — 绑定建筑的单位
- **LaunchCoreComp** — 发射核心
- **CrawlComp** — 爬行

### 实体组（Groups）

```java
// GroupDefs.java 定义的实体组
Groups.all      — Entityc, 所有实体
Groups.player   — Playerc, mapping=true
Groups.bullet   — Bulletc, spatial=true, collide=true
Groups.unit     — Unitc, spatial=true, mapping=true
Groups.build    — Buildingc
Groups.sync     — Syncc, mapping=true
Groups.draw     — Drawc
Groups.fire     — Firec
Groups.puddle   — Puddlec
Groups.weather  — WeatherStatec
Groups.label    — WorldLabelc, mapping=true
Groups.powerGraph — PowerGraphUpdaterc
```

### 组特性
- **spatial=true**：使用QuadTree空间索引（用于碰撞检测）
- **mapping=true**：使用ID→实体映射（用于网络同步）
- **collide=true**：启用碰撞检测

### 实体生成规则
```java
@EntityDef(value = {Unitc.class, Weaponsc.class, ...})
// → 编译时生成 Unit.java，实现所有指定接口
// → 自动注册到对应的Groups
```

## 关键规则

### 组件规则
1. **@Import**不存储数据，从其他组件引用
2. **@Component(base=true)**只能有一个，是实体的基类
3. **@Replace**方法会覆盖其他组件的同名方法
4. **@MethodPriority**控制方法调用顺序（数值越大越晚）
5. **@InternalImpl**由注解处理器实现（如classId()、serialize()）

### 实体规则
6. **实体类是生成的**，不要直接写Unit.java/Bullet.java
7. **Building是例外**：BuildingComp直接写，不通过@EntityDef
8. **EntityGroup**管理实体的增删查
9. **Groups.build**不使用spatial（建筑固定位置）
10. **Groups.bullet**使用spatial+collide（子弹碰撞检测）
11. **Groups.unit**使用spatial+mapping（单位空间查询+网络映射）

### 序列化规则
12. **serialize=true**的实体自动存档
13. **genio=true**生成read/write代码
14. **@NoSync**字段不同步到客户端
15. **@SyncField**字段插值同步（linear/angular）
16. **@SyncLocal**字段不同步本地玩家状态

## 常见误区

| 错误认知 | 正确事实 |
|----------|----------|
| 运行时ECS | 编译时生成，不是运行时组合 |
| 直接写Unit.java | Unit是生成的，只能写UnitComp |
| Building也是生成的 | BuildingComp是手写的，不走@EntityDef |
| Groups.all包含Building | 是的，Building也是Entityc |
| 实体ID可自定义 | id=EntityGroup.nextId()自动分配 |
| @Import存储数据 | 只是引用，不额外存储 |
