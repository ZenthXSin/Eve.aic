# World Processor Seq serialization crash

> Source: conversation + Mindustry 155.4 source analysis

## 结论

这次不是单纯的 world processor `query/fetch` 导致主线程卡死，而是 **逻辑变量里存进了 `arc.struct.Seq`，随后在世界保存/玩家加入发送世界数据时序列化崩溃**。

报错：

```text
java.lang.IllegalArgumentException: Unknown object type: class arc.struct.Seq
    at mindustry.io.TypeIO.writeObject(TypeIO.java:153)
    at mindustry.world.blocks.logic.LogicBlock$LogicBuild.write(LogicBlock.java:739)
```

## 关键源码证据

### 1. `LogicBuild.write()` 会把逻辑变量对象逐个丢给 `TypeIO.writeObject`

`mindustry/world/blocks/logic/LogicBlock.java`

```java
if(writeUnit){
    write.str("@unit");
    TypeIO.writeObject(write, executor.unit.objval);
}

for(int i = 0; i < executor.vars.length; i++){
    LVar v = executor.vars[i];
    if(v.isobj && v.objval == null) continue;
    write.str(v.name);
    Object value = v.isobj ? v.objval : v.numval;
    TypeIO.writeObject(write, value);
}
```

所以只要某个变量的 `objval` 是 `Seq`，保存时就会炸。

### 2. `TypeIO.writeObject()` 根本不支持 `arc.struct.Seq`

`mindustry/io/TypeIO.java`

支持的是一组白名单类型，例如：
- `Integer / Long / Float / String`
- `Content`
- `IntSeq`
- `Point2 / Point2[]`
- `Boolean / Double`
- `Building / Unit / Team / Vec2 / Vec2[]`
- `int[] / boolean[] / byte[] / Object[]`
- `UnitCommand`

末尾直接：

```java
throw new IllegalArgumentException("Unknown object type: " + object.getClass());
```

没有 `arc.struct.Seq` 分支。

### 3. 崩溃发生在世界写盘/发世界数据链路

`SaveVersion.writeMap()`：

```java
out.b(tile.build.version());
 tile.build.writeAll(out);
```

`BuildingComp.writeAll()`：

```java
writeBase(write);
write(write);
```

所以 `LogicBuild.write()` 在存档、发图、玩家加入同步世界时都会被走到。

## 根因判断

世界处理器脚本或相关 Java/JS 代码，把某个逻辑变量对象写成了 `arc.struct.Seq`。

常见形式：
- 自定义指令/扩展把 `Seq` 直接塞进 `LVar.objval`
- mod 代码把查询结果集合原样挂到逻辑变量
- 试图把 `Groups.unit`、`Seq<Building>`、`Seq<Unit>` 之类集合直接暴露给逻辑存档

## 修复方向

不要把 `Seq` 存进逻辑变量持久状态。改成：

1. **转成 `Object[]`** 再存，如果元素本身也是 `TypeIO` 支持类型
2. 只存 **索引/ID/坐标/长度**，运行时重建集合
3. 若只是临时查询结果，**不要落进 `executor.vars`**
4. 若你在改 mod，检查所有给 `LVar.objval` 赋值的位置，搜：
   - `objval =`
   - `executor.vars`
   - 自定义 `LInstruction`
   - world processor 扩展/query 扩展

## 和之前“query 死机”结论的关系

之前那个更像高 `ipt` + 批量查询的主线程过载；这次日志已经明确到另一个独立问题：**非法对象类型进入逻辑序列化**。

## Related

- [World Processor 查询/批量指令导致卡死排查](world-processor-query-freeze-diagnosis.md) — 区分脚本过载卡死与本次序列化异常
- [VoidShield numfWorld NoSuchMethodError 修复](voidshield-numfWorld-crash-fix.md) — 另一类运行时 API 不兼容崩溃
