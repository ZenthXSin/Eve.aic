# Mindustry 电力系统持久化

原版电力系统的持久化主要通过建筑的 `PowerModule` 完成，而不是直接序列化整张 `PowerGraph`。

## 关键代码

### 1. `PowerModule` 保存/读取连接与状态

```java
@Override
public void write(Writes write){
    write.s(links.size);
    for(int i = 0; i < links.size; i++){
        write.i(links.get(i));
    }
    write.f(status);
}

@Override
public void read(Reads read){
    links.clear();
    short amount = read.s();
    for(int i = 0; i < amount; i++){
        links.add(read.i());
    }
    status = read.f();
    if(Float.isNaN(status) || Float.isInfinite(status)) status = 0f;
}
```

这段会持久化：
- `links`：电力连接的建筑索引
- `status`：电力状态/电量比例

### 2. `BuildingComp` 在读档时恢复 `PowerModule`

```java
if((moduleBits & (1 << 1)) != 0) (power == null ? new PowerModule() : power).read(read, legacy);
```

这说明建筑实体存档中只要带有 power 模块，就会在读档阶段恢复电力数据。

## 运行时图结构

`PowerGraph` 负责：
- 组网
- 供需计算
- 更新电力流动

它更偏向运行时结构，持久化通常通过 `PowerModule` 间接完成。

## 结论

**原版电力系统持久化的核心是 `PowerModule.write/read()`，`BuildingComp` 负责把它纳入建筑存档流程。**
