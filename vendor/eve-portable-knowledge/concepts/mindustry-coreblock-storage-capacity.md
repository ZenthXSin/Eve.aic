# Mindustry CoreBlock 核心容量详解

## 核心字段

| 字段 | 位置 | 含义 |
|------|------|------|
| `block.itemCapacity` | `StorageBlock.java` | 核心方块的静态基础容量 |
| `CoreBuild.storageCapacity` | `CoreBlock.java:255` | **容器拓展后**的实际容量 = 基础 + 邻近加成 |

## 关键点

**`build.storageCapacity`** 才是容器拓展后的实际容量，会随容器拓展（复制核心等邻近建筑）而变化。

注意：`build.block.itemCapacity` 是静态 Block 级别容量，不含加成。

---

## storageCapacity 计算逻辑（完整版）

源码：`CoreBlock.java` 的 `onProximityUpdate()` 方法

### 关键发现：所有同阵营核心共享同一个 storageCapacity

**最终公式**：
```
任意核心.storageCapacity = Σ(每个核心.itemCapacity + 该核心的所有邻近加成)
                        = 队伍所有核心容量之和 + 队伍所有邻近加成之和
```

所有同阵营核心的 `storageCapacity` 被**统一**为同一个值，这个值是整队所有核心及其邻近加成存储建筑的总和。

### 三步计算过程

**第一步：本核心基础容量**
```java
storageCapacity = itemCapacity + proximity.sum(e -> owns(e) ? e.block.itemCapacity : 0);
```
本核心的 `block.itemCapacity` + 所有**相邻且被本核心"拥有"的非核心建筑**的 `itemCapacity` 之和。

**第二步：累加其他核心的容量**
```java
for(Building other : state.teams.cores(team)){
    if(other.tile == tile) continue;
    storageCapacity += other.block.itemCapacity
        + other.proximity.sum(e -> owns(other, e) ? e.block.itemCapacity : 0);
}
```
把队伍里**其他核心**的容量（自身 + 其邻近加成）全部累加进来。

**第三步：同步到所有核心**
```java
for(CoreBuild other : state.teams.cores(team)){
    other.storageCapacity = storageCapacity;
}
```
队伍里**所有核心**的 `storageCapacity` 被**统一**为同一个值。

### owns() 判断逻辑

```java
public boolean owns(Building core, Building tile){
    return tile instanceof StorageBuild b
        && ((StorageBlock)b.block).coreMerge    // 该建筑允许与核心合并
        && (b.linkedCore == core || b.linkedCore == null);  // 未被其他核心绑定或被本核心绑定
}
```
必须是 `StorageBlock`（非核心仓储方块）且开启 `coreMerge` 才行。

---

### 不同场景的行为

| 场景 | storageCapacity 实际行为 |
|------|---------|
| 单核心 | `本核心.itemCapacity + 邻近加成` |
| 多核心同阵营 | **所有核心**的容量相加并共享同一个值 |
| 核心被复制升级 | 新核心触发 `onProximityUpdate`，**重新计算并同步所有核心**的 `storageCapacity` |

---

## 逻辑传感器

源码：`CoreBlock.java:552`

```java
@Override
public double sense(LAccess sensor){
    if(sensor == LAccess.itemCapacity) return storageCapacity;
    // ...
}
```

逻辑处理器中 `@itemCapacity` 读取的正是 `storageCapacity`。

---

## 代码示例

```java
// 获取主核心（含容器拓展）的实际存储上限
int capacity = state.rules.defaultTeam.core().storageCapacity;

// 所有核心总容量
int total = state.teams.cores().sumc(c -> c.storageCapacity);

// 逻辑处理器中等价写法
double cap = core.sense(LAccess.itemCapacity);
```

---

## 与旧字段 `itemCapacity` 的区别

| 字段 | 作用域 | 是否含容器加成 |
|------|--------|--------------|
| `block.itemCapacity` | 静态定义（Block 级别） | 否 |
| `CoreBuild.storageCapacity` | 实例级别（每个核心建筑） | 是 |

---

## 来源

- `CoreBlock.java` 第 255 行：`public int storageCapacity;`
- `CoreBlock.java` 第 552 行：`if(sensor == LAccess.itemCapacity) return storageCapacity;`
- `CoreBlock.java` 第 699 行：`return state.rules.coreIncinerates ? Integer.MAX_VALUE/2 : storageCapacity;`
- `CoreBlock.java` 第 713 行：`storageCapacity = itemCapacity + proximity.sum(e -> owns(e) ? e.block.itemCapacity : 0);`
- `CoreBlock.java` 第 731 行：`other.storageCapacity = storageCapacity;`
- `StorageBlock.java`：`itemCapacity` 字段定义处

## 关联知识

- [Mindustry CoreBlock Item Capacity](concepts/mindustry-coreblock-item-capacity.md) — JSON 中 `itemCapacity` 写法与注意事项