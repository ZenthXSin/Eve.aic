# World Processor 查询/批量指令导致卡死排查

> Source: conversation + Mindustry 155.4 source analysis

## 结论

`world processor` 的“query/查询类”逻辑本身没有找到一个单独的必现崩溃点；更像是**脚本级主线程过载**：

- `LogicBlock.LogicBuild.updateTile()` 会在主线程里按 `ipt` 循环执行 `executor.runOnce()`
- `worldProcessor` 是 `privileged = true`
- `worldProcessor.maxInstructionsPerTick = 1000`
- `setrate` 指令可把 `ipt` 直接改到 `1..1000`

所以只要脚本里把 `ipt` 拉高，再叠加大循环、无 `wait`/无 `yield` 的批量 `fetch` / `getblock` / 其他 world 指令，就能把单帧主线程吃满，表现成“死机”或严重卡住。

## 关键源码证据

### 1. world processor 允许超高 ipt

`mindustry/content/Blocks.java`

```java
worldProcessor = new LogicBlock("world-processor"){{
    instructionsPerTick = 8;
    privileged = true;
    maxInstructionsPerTick = 1000;
    range = Float.MAX_VALUE;
}};
```

### 2. 每帧主线程循环执行逻辑

`mindustry/world/blocks/logic/LogicBlock.java`

```java
while(accumulator >= 1f){
    executor.runOnce();
    if(executor.yield){
        executor.yield = false;
        break;
    }
    accumulator --;
}
accumulator += edelta() * ipt;
```

这说明逻辑指令不是异步跑的，就是在方块更新里硬跑。

### 3. setrate 可直接把 ipt 拉满

`mindustry/logic/LExecutor.java`

```java
exec.build.ipt = Mathf.clamp(amount.numi(), 1, ((LogicBlock)exec.build.block).maxInstructionsPerTick);
```

### 4. fetch 主要走 team cache，不是全图扫描

`mindustry/logic/LExecutor.java`

`FetchI.run()` 读取：
- `data.units`
- `data.players`
- `data.cores`
- `data.buildings`
- `data.unitCache(type)`
- `data.getBuildings(block)`

说明 `fetch` 本身更像**便宜的索引访问**，不是天然的全图暴扫。

### 5. getblock 是单点 world tile 查询

`mindustry/logic/LExecutor.java`

```java
Tile tile = world.tile(Mathf.round(x.numf()), Mathf.round(y.numf()));
```

这也是单次点查，不是一次扫整张地图。

## 因此更可能的根因

不是“某个 query 指令一调用就炸”，而是下面这种组合把 CPU 拉满：

- `setrate 1000`
- `jump` 自旋 / 大循环
- 每次循环中做大量 `fetch` / `getblock` / `setblock` / `clientdata` / `sync`
- 没有 `wait`
- 试图在一帧内遍历大量坐标或大量实体

## 排查建议

1. 先把脚本里的 `setrate` 降到 `1~20`
2. 暂时注释所有 `fetch/getblock/setblock/sync/clientdata`
3. 逐段恢复，看哪段一恢复就卡
4. 所有遍历都改成分帧：每 tick 只处理少量索引
5. 循环里加入 `wait`
6. 不要在单帧里扫全图或扫全队伍所有实体

## 可操作判断

如果把 `setrate` 降低后“query 导致死机”立刻缓解，那根因基本就是**高 ipt + 无分帧批量查询**，不是引擎层单点空指针。

## Related

- [安卓端 InputHandler BuildPlan 空值崩溃](android-inputhandler-buildplan-null-crash.md) — 这是另一类真崩溃，根因是 `null BuildPlan`，不是逻辑过载
- [VoidShield numfWorld NoSuchMethodError 修复](voidshield-numfWorld-crash-fix.md) — 这是版本 API 不兼容导致的崩溃，和本问题不同
