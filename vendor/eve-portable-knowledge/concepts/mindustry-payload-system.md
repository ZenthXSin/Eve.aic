# Mindustry 载荷系统源码验证

> Source: `mindustry/world/blocks/payloads/Payload.java` (114行), `PayloadBlock.java` (281行), `PayloadConveyor.java` (390行), `PayloadRouter.java` (220行), `PayloadMassDriver.java` (528行), `PayloadLoader.java` (263行), `PayloadDeconstructor.java` (254行), `PayloadUnloader.java` (124行), `PayloadVoid.java` (66行), `PayloadSource.java` (190行), `BuildPayload.java` (133行), `UnitPayload.java` (184行)

## Payload 接口

```java
public interface Payload extends Position {
    int payloadUnit = 0, payloadBlock = 1;  // 类型标识
    void set(float x, float y, float rotation);
    void draw();
    float size();
    ItemStack[] requirements();  // 建造需求
    float buildTime();           // 建造时间
    boolean fits(float s);       // size/tilesize <= s
    default boolean dump() { return false; }
    default float rotation() { return 0f; }
}
```

**两种实现**：
- `BuildPayload`：方块载荷，`size = block.size * tilesize`
- `UnitPayload`：单位载荷，`size = unit.hitSize`

## PayloadBlock 基类

继承 `Block`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `payloadSpeed` | 0.7f | 载荷移动速度 |
| `payloadRotateSpeed` | 5f | 载荷旋转速度 |
| `group` | BlockGroup.payloads | 载荷组 |
| `acceptsUnitPayloads` | true | 接受单位载荷 |
| `envEnabled` | += space, underwater | 太空/水下可用 |

### PayloadBlockBuild 泛型内部类
- `payload: T` — 当前持有的载荷（nullable）
- `payVector: Vec2` — 载荷相对位置偏移
- `payRotation: float` — 载荷旋转
- `carried: boolean` — 是否被携带（影响 onRemoved 行为）

**核心方法**：
- `moveInPayload(rotate)` — 载荷向中心移动，到位返回 true
- `moveOutPayload()` — 载荷向出口移动，到达后 transfer 或 dump
- `dumpPayload()` — 释放载荷到地图
- `acceptPayload(source, payload)` — 默认 payload == null 时接受
- `handlePayload(source, payload)` — 设置 payload 和 payVector
- `pushOutput(payload, progress)` — 推开路径上的单位（progress > 0.55 时生效）

**front 为 null 或非 solid 时 canDump=true；front 有 outputsPayload 或 acceptsPayload 时 canMove=true**

## PayloadConveyor（载荷传送带）

继承 `Block`（不是 PayloadBlock），关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `moveTime` | 45f | 移动周期（ticks） |
| `moveForce` | 201f | 推力 |
| `interp` | Interp.pow5 | 插值函数 |
| `payloadLimit` | 3f | 最大载荷尺寸（tile 单位） |
| `pushUnits` | true | 推动单位 |
| `size` | 3 | 默认3x3 |
| `outputsPayload` | true | 输出载荷 |
| `underBullets` | true | 子弹可穿过 |

### PayloadConveyorBuild
- `item: Payload` — 当前载荷
- `progress: float` — 当前周期进度
- `step: int` / `stepAccepted: int` — 步进计数器
- `next: Building` — 下一个目标建筑
- `blocked: boolean` — 前方是否堵塞

**传输机制**：
1. 基于 `time() % moveTime` 计算进度
2. 每个 moveTime 周期完成一次传输尝试
3. `fract()` 使用 `interp.apply(progress/moveTime)` 插值
4. 传输时先 updateTile 下一个，再 acceptPayload → handlePayload
5. 前方无建筑且不堵塞时 dump（释放到地图）
6. `acceptPayload` 要求 `progress <= 5f`（周期刚开始才能接受）

**unitOn 推力**：基于 `curInterp - lastInterp` 计算速度差，按质量施加力

## PayloadRouter（载荷路由器）

继承 `PayloadConveyor`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `invert` | false | 反转匹配 |
| `outputFacing` | false | 不限输出方向 |
| `configurable` | true | 可配置 |
| `clearOnDoubleTap` | true | 双击清除配置 |

**配置类型**：`Block.class` 或 `UnitType.class`

### PayloadRouterBuild
- `sorted: UnlockableContent` — 排序目标（方块或单位类型）
- `recDir: int` — 接收方向
- `matches: boolean` — 当前载荷是否匹配
- `controlTime: float` — 手动控制锁定时间（6帧*60=6秒）

**路由逻辑**：
1. 接收载荷后 `checkMatch()` 判断是否匹配 sorted
2. 匹配 → 直接从 recDir 方向输出
3. 不匹配 → 轮转4方向尝试输出（跳过 recDir）
4. `invert=true` 时匹配逻辑反转
5. 逻辑控制的 rotation 会在 controlTime 后自动恢复

## PayloadMassDriver（载荷质量驱动器）

继承 `PayloadBlock`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `range` | 100f | 射程 |
| `rotateSpeed` | 5f | 旋转速度 |
| `length` | 89/8f ≈ 11.125 | 炮管长度 |
| `knockback` | 5f | 后坐力 |
| `reload` | 30f | 装填时间 |
| `chargeTime` | 100f | 充能时间 |
| `maxPayloadSize` | 3f | 最大载荷尺寸 |
| `hasPower` | true | 需要电力 |
| `configurable` | true | 可配置目标 |
| `group` | BlockGroup.units | 单位组（不是 payloads） |

**状态机**（PayloadDriverState）：
- `idle` — 空闲
- `accepting` — 接收模式（等待被射击）
- `shooting` — 射击模式（准备发射）

**射击流程**：
1. idle → 有 link 且有 payload → shooting
2. shooting: moveInPayload → 载荷到位 → loaded=true
3. 对齐目标角度 + 接收端 accepting + 双方 reload 完毕
4. charge 充能达到 chargeTime → 发射
5. transferEffect 播放动画 → handlePayload 到接收端
6. 双方重置为 idle

**配置**：通过 `Point2.class` 或 `Integer.class` 设置 link（目标位置）

## PayloadLoader（载荷装载器）

继承 `PayloadBlock`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `loadTime` | 2f | 装载时间 |
| `itemsLoaded` | 8 | 每次装载物品数 |
| `liquidsLoaded` | 40f | 每次装载液体量 |
| `maxBlockSize` | 3 | 最大方块尺寸 |
| `maxPowerConsumption` | 40f | 最大电力消耗 |
| `loadPowerDynamic` | true | 动态电力消耗 |
| `canOverdrive` | false | 不可加速 |

**接受条件**：payload 必须是 BuildPayload，且：
- 有物品栏 + unloadable + itemCapacity >= 10 + size <= maxBlockSize
- 或有液体栏 + liquidCapacity >= 10f
- 或有缓冲电力（consPower.buffered）

**工作模式**：往 payload 内的建筑填充物品/液体/电力

## PayloadDeconstructor（载荷解构器）

继承 `PayloadBlock`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `deconstructSpeed` | 0.5f | 解构速度倍率 |
| `maxBlockSize` | 3 | 最大方块尺寸 |

**功能**：拆解 BuildPayload，返还材料

## PayloadUnloader（载荷卸载器）

继承 `PayloadBlock`，关键字段：

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `unloadTime` | 3f | 卸载间隔 |
| `maxBlockSize` | 3 | 最大方块尺寸 |

**功能**：从 payload 内的建筑中提取物品/液体

## PayloadVoid（载荷销毁器）
- 继承 PayloadBlock
- `update = false`
- 接受并销毁载荷

## PayloadSource（载荷生成器）
- 继承 PayloadBlock
- sandbox 模式专用
- 可配置生成方块或单位载荷

## 硬性规则

1. **Payload 是接口** — BuildPayload 和 UnitPayload 两种实现
2. **PayloadConveyor 继承 Block 不是 PayloadBlock** — 独立实现
3. **PayloadConveyor.moveTime 默认 45f** — 不是秒，是 ticks
4. **payloadLimit=3f 是 tile 单位** — fits() 检查 `size/tilesize <= s`
5. **PayloadMassDriver group=units** — 不是 payloads
6. **PayloadMassDriver 有3态状态机** — idle/accepting/shooting
7. **PayloadMassDriver.chargeTime=100f** — 充能时间，不是射击间隔
8. **pushOutput 阈值 0.55** — progress > 55% 才开始推开单位
9. **PayloadRouter outputFacing=false** — 默认不限输出方向
10. **PayloadRouter 手动控制锁定 6 秒** — controlTime = 60*6
11. **PayloadLoader.canOverdrive 默认 false** — 不可加速
12. **PayloadLoader 接受 3 种容器** — 物品容器/液体容器/缓冲电池
13. **PayloadBlock.acceptsUnitPayloads 默认 true**
14. **PayloadBlock.envEnabled 含 space+underwater**
15. **PayloadConveyor.underBullets=true** — 子弹穿过
16. **BuildPayload.size = block.size * tilesize**
17. **UnitPayload.size = unit.hitSize**
18. **载荷序列化** — write bool + type byte + 数据
19. **PayloadConveyor 接受条件 progress <= 5f** — 周期开始才能接受
20. **PayloadMassDriver 配置通过 Point2** — 相对坐标+偏移

## 常见误区

- ❌ PayloadConveyor 继承 PayloadBlock → 继承 Block
- ❌ PayloadMassDriver group=payloads → group=units
- ❌ payloadLimit 单位是格子 → 是 tile 单位（3 = 3 tiles = 24px）
- ❌ 载荷传送带每帧传输 → 每 moveTime(45 ticks) 一次
- ❌ pushOutput 始终生效 → 需要 progress > 55%
- ❌ PayloadLoader 可超速 → canOverdrive=false
- ❌ 载荷只有方块 → 方块+单位两种

## Related

- [Mindustry 工厂输入/输出载荷生产](mindustry-load-production.md)
- [Mindustry Payload 输入/输出关键片段](mindustry-payload-io-snippets.md)
- [Mindustry GenericCrafter 增加载荷输入/输出](mindustry-genericcrafter-payload-io.md)
- [Mindustry Building/Tile/Power/Liquid 源码验证](mindustry-building-tile-power-liquid.md)
