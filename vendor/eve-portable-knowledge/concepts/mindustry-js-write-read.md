# Mindustry JS Mod write() / read() 完整指南

> Source: BuildingComp.java 源码 + CurseOfFlesh mod 9 个 JS 示例

## 核心结论

Building 子类自定义字段必须重写 `write()` / `read()`，否则存档丢失。

### JS 方法签名

```js
buildType = prov(() => {
    let myFloat = 0, myFlag = false;
    return extend(Building, {
        write(write) {
            this.super$write(write);    // 必须调用
            write.f(myFloat);
            write.bool(myFlag);
        },
        read(read, revision) {
            this.super$read(read, revision); // 必须调用
            myFloat = read.f();
            myFlag = read.bool();
        }
    });
});
```

### 调用链

```
存档保存 → build.writeAll(out) → this.super$write(write) + write(write)
存档读取 → build.readAll(in, revision) → this.super$read(read, revision) + read(read, revision)
联机同步 → writeSync = writeAll / readSync = readAll
```

### Writes / Reads API

| 类型 | 写入 | 读取 | 字节 |
|------|------|------|------|
| byte | `write.b(值)` | `read.b()` | 1 |
| unsigned byte | — | `read.ub()` | 1 (0~255) |
| short | `write.s(值)` | `read.s()` | 2 |
| int | `write.i(值)` | `read.i()` | 4 |
| long | `write.l(值)` | `read.l()` | 8 |
| float | `write.f(值)` | `read.f()` | 4 |
| double | `write.d(值)` | `read.d()` | 8 |
| boolean | `write.bool(值)` | `read.bool()` | 1 |
| String | `write.str(值)` | `read.str()` | 变长 |
| byte[] | `write.b(数组)` | `read.b(len)` | 指定长度 |

### 特殊类型（TypeIO）

| 用途 | 写入 | 读取 |
|------|------|------|
| 可空 Vec2 | `TypeIO.writeVecNullable(write, vec)` | `TypeIO.readVecNullable(read)` |
| Payload | `TypeIO.writePayload(write, p)` | `TypeIO.readPayload(read)` |
| UnitCommand | `TypeIO.writeCommand(write, cmd)` | `TypeIO.readCommand(read)` |

### 版本兼容

`revision` 由 Java 侧 `Building.version()` 决定（默认 0），JS 中可覆盖。

```js
// 在 extend 对象中
version() { return 2; },

read(read, revision) {
    this.super$read(read, revision);
    oldField = read.f();
    if (revision >= 2) {
        newField = read.i();  // v2 新增字段
    }
}
```

### 三条铁律

1. **必须调 super**：否则父类的物品、进度、warmup 全部丢失
2. **读写顺序严格一致**：错 1 字节后续全部报废
3. **只存自定义字段**：父类字段（health、items、rotation、power）super 已处理

### 动态数组写入模式

```js
// 1. 写长度 → 2. 逐个写 ID
write.i(myList.size);
for (var i = 0; i < myList.size; i++) {
    write.i(myList.get(i).id);
}

// 读取：先读长度 → 逐个读 ID → Groups.getByID 恢复引用
var count = read.i();
for (var i = 0; i < count; i++) {
    var entity = Groups.unit.getByID(read.i());
    if (entity != null) myList.add(entity);
}
```

### Building vs Unit 对比

| 对比项 | Building | Unit |
|--------|----------|------|
| write 签名 | `write(write)` | `write(write)` |
| read 签名 | `read(read, revision)` | `read(read)` — 无 revision |
| 注册方式 | `block.buildType = prov(...)` | `unit.constructor = () => extend(...)` |

### CurseOfFlesh 示例索引

| 复杂度 | 文件 | 特点 |
|--------|------|------|
| 简单 | `偏转力场.js` | 2 字段 (float + bool) |
| 简单 | `炸弹.js` | 1 字段 (float 计时器) |
| 中等 | `载荷发射器.js` | `TypeIO.writeVecNullable` (Vec2) |
| 较复杂 | `核心.js` | 物品 ID + Payload 数组 |
| 复杂 | `重生点.js` | 动态数组 + Groups.getByID 恢复实体引用 |
---

## Related

- [Building 与 Tile 坐标关系](mindustry-building-tile-coordinate-relationship.md) — Building 坐标绑定
- [Mindustry Public Fields vs Getters/Setters](mindustry-public-fields-vs-getters-setters.md) — 字段风格
- [JSON 问题解决流程](json-problem-solving-flow.md) — 排查通用流程
